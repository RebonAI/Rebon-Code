//! The TypeSafe backend: the same decision, asked as typed questions.
//!
//! The text backend describes the candidates in a prompt and parses JSON out of
//! the answer. This one asks two Choice questions in a single call — which
//! provider and model, and at which reasoning effort — and reads the answers
//! back under the same ids. The rules both backends answer to live in
//! [`crate::resolve_decision`], so they cannot drift apart.
//!
//! Two questions rather than one because the classifier answers them in
//! parallel and against the same state: the effort is not conditioned on the
//! pair, so an effort the chosen model does not take is refused by the shared
//! rules rather than silently paired with it.

use std::collections::BTreeMap;

use anyhow::{ensure, Context as _};

use crate::typesafe::{ChoiceQuestion, Request, Response, SystemOneClient, DEFAULT_MODEL};
use crate::{
    model_takes_effort, resolve_decision, Candidates, ModelRoutingDecision, ModelRoutingInput,
    ProviderCandidate, RawChoice,
};
use rebon_types::ReasoningEffort;

/// The id the target question is asked and answered under.
pub(crate) const TARGET_QUESTION: &str = "target";
/// The id the reasoning-effort question is asked and answered under.
pub(crate) const EFFORT_QUESTION: &str = "effort";
/// The option that means "change nothing".
///
/// A Choice answer has to name one of the options it was given, so leaving the
/// session's choice alone has to be one of them.
pub(crate) const KEEP: &str = "keep";

/// How much of the first prompt is sent as the state.
///
/// The decision is about what the task is, and a first prompt can be a pasted
/// file; the head is where the ask is, and the cap is what keeps a megabyte of
/// paste from leaving the machine for the sake of one classification.
pub(crate) const MAX_STATE_CHARS: usize = 32_000;

/// TypeSafe takes 255 options per Choice question, and `keep` is one of them.
const MAX_TARGET_PAIRS: usize = 254;

/// What the classifier is asked, in both questions.
///
/// The user's policy comes first and is one instruction among two: TypeSafe
/// reads a list of instructions as one question, and the policy is the part
/// that outranks the default preference for cheap.
fn instructions(question: &str, policy: Option<&str>) -> serde_json::Value {
    match policy {
        Some(policy) => serde_json::json!([policy, question]),
        None => serde_json::json!(question),
    }
}

/// The option key one candidate pair is offered under.
///
/// Written once: the question builds the keys from it and the answer is looked
/// up by it, and a model id may itself carry a slash, so builder and lookup
/// have to agree exactly.
fn option_key(provider: &str, model: &str) -> String {
    format!("{provider}/{model}")
}

/// The pair to run on, plus the option that keeps the session as it is.
fn target_criteria(candidates: &Candidates) -> anyhow::Result<BTreeMap<String, Option<String>>> {
    let mut criteria = BTreeMap::new();
    criteria.insert(
        KEEP.to_string(),
        Some("Keep the current provider and model".to_string()),
    );
    let mut pairs = 0;
    for candidate in &candidates.providers {
        for model in &candidate.models {
            pairs += 1;
            criteria.insert(
                option_key(&candidate.id, model),
                Some(describe_target(candidate, model)),
            );
        }
    }
    ensure!(
        pairs <= MAX_TARGET_PAIRS,
        "the catalogue offers {pairs} provider/model pairs, more than the {MAX_TARGET_PAIRS} \
         a TypeSafe target question can list alongside the keep-current option"
    );
    Ok(criteria)
}

/// What the classifier is told about one candidate pair.
///
/// The catalogue carries price, size and accepted efforts, which is the part a
/// classification is usually made on; a model the catalogue never heard of is
/// described as such rather than left blank.
fn describe_target(candidate: &ProviderCandidate, model: &str) -> String {
    let key = option_key(&candidate.id, model);
    let mut text = match rebon_api::model_table::model(Some(&candidate.id), model) {
        Some(row) => {
            let mut facts = Vec::new();
            if let Some(family) = row.family.as_deref() {
                facts.push(family.to_string());
            }
            if let Some(context) = row.limit.context {
                facts.push(format!("{context} token context"));
            }
            if let Some(cost) = row.cost.as_ref() {
                if let (Some(input), Some(output)) = (cost.input, cost.output) {
                    facts.push(format!("${input}/${output} per MTok input/output"));
                }
            }
            if !row.reasoning_efforts.is_empty() {
                facts.push(format!("efforts: {}", row.reasoning_efforts.join(", ")));
            }
            if facts.is_empty() {
                key
            } else {
                format!("{key} — {}", facts.join(", "))
            }
        }
        None => format!(
            "{key} — no catalogue row, so its price, context window and efforts are whatever \
             the provider entry says"
        ),
    };
    let profiles: Vec<&str> = candidate
        .profiles
        .iter()
        .filter(|(_, target)| *target == model)
        .map(|(name, _)| name)
        .collect();
    if !profiles.is_empty() {
        text.push_str(&format!(
            ". Profiles pointing here: {}",
            profiles.join(", ")
        ));
    }
    text
}

/// The levels at least one candidate takes, plus `keep`.
///
/// A level no candidate accepts is left out rather than offered and refused:
/// the answer would have to be rejected by the same rules that built the
/// question. Which candidates accept one is [`model_takes_effort`] — the same
/// predicate the validation uses, so the question cannot offer what the answer
/// would be refused for.
fn effort_criteria(candidates: &Candidates) -> BTreeMap<String, Option<String>> {
    let mut criteria = BTreeMap::new();
    criteria.insert(
        KEEP.to_string(),
        Some("Keep the current reasoning effort".to_string()),
    );
    for level in ReasoningEffort::ALL {
        let accepted: Vec<String> = candidates
            .providers
            .iter()
            .flat_map(|candidate| {
                candidate
                    .models
                    .iter()
                    .filter(|model| model_takes_effort(&candidate.id, model, level.as_str()))
                    .map(|model| option_key(&candidate.id, model))
            })
            .collect();
        if accepted.is_empty() {
            continue;
        }
        criteria.insert(
            level.as_str().to_string(),
            Some(format!("Accepted by: {}", accepted.join(", "))),
        );
    }
    criteria
}

/// The questions, ready to send.
pub(crate) fn request(
    input: &ModelRoutingInput,
    candidates: &Candidates,
    policy: Option<&str>,
) -> anyhow::Result<Request> {
    let state = serde_json::json!({
        "prompt": input.prompt.chars().take(MAX_STATE_CHARS).collect::<String>(),
        "cwd": input.cwd.display().to_string(),
        "current": {
            "provider": input.provider_name,
            "model": input.model,
        },
    });
    let mut questions = BTreeMap::new();
    questions.insert(
        TARGET_QUESTION.to_string(),
        ChoiceQuestion::new(
            instructions(
                "Which provider and model should this task run on? Answer `keep` to leave the \
                 session where it is.",
                policy,
            ),
            target_criteria(candidates)?,
        ),
    );
    questions.insert(
        EFFORT_QUESTION.to_string(),
        ChoiceQuestion::new(
            instructions(
                "How much reasoning effort does this task need? Answer `keep` to leave the \
                 session's effort alone.",
                policy,
            ),
            effort_criteria(candidates),
        ),
    );
    Ok(Request {
        state,
        model: DEFAULT_MODEL.to_string(),
        questions,
    })
}

/// Ask, then put the answers through the rules every decision answers to.
pub(crate) async fn route(
    client: &SystemOneClient,
    input: &ModelRoutingInput,
    candidates: &Candidates,
    policy: Option<&str>,
) -> anyhow::Result<ModelRoutingDecision> {
    let request = request(input, candidates, policy)?;
    let response = client.ask(&request).await?;
    let decision = decide(&response, input, candidates)?;
    tracing::debug!(
        model = %response.model,
        provider = decision.provider.as_deref().unwrap_or(&input.provider_name),
        model_id = decision.model.as_deref().unwrap_or(&input.model),
        effort = decision
            .reasoning_effort
            .as_ref()
            .map(ReasoningEffort::as_str)
            .unwrap_or("kept"),
        "TypeSafe answered the routing questions"
    );
    Ok(decision)
}

fn decide(
    response: &Response,
    input: &ModelRoutingInput,
    candidates: &Candidates,
) -> anyhow::Result<ModelRoutingDecision> {
    let target = chosen(response, TARGET_QUESTION)?;
    let effort = chosen(response, EFFORT_QUESTION)?;
    let (provider, model) = if target == KEEP {
        (None, None)
    } else {
        let (provider, model) = pair_named(candidates, target)?;
        (Some(provider), Some(model))
    };
    let effort = (effort != KEEP).then(|| effort.to_string());
    resolve_decision(
        RawChoice {
            provider,
            model,
            effort,
        },
        candidates,
        &input.provider_name,
        &input.model,
    )
}

/// The option one question's answer names.
fn chosen<'a>(response: &'a Response, question: &str) -> anyhow::Result<&'a str> {
    let answer = response
        .answers
        .get(question)
        .with_context(|| format!("TypeSafe did not answer the `{question}` question"))?;
    let (choice, confidence) = answer.chosen().with_context(|| {
        format!(
            "the `{question}` answer is a `{}`, not a choice",
            answer.kind()
        )
    })?;
    tracing::debug!(question, choice, confidence, "TypeSafe routing answer");
    Ok(choice)
}

/// The candidate pair an option key names.
///
/// Looked up in the same list the question was built from rather than split on
/// `/`, since a model id may itself carry a slash.
fn pair_named(candidates: &Candidates, option: &str) -> anyhow::Result<(String, String)> {
    candidates
        .providers
        .iter()
        .find_map(|candidate| {
            candidate
                .models
                .iter()
                .find(|model| option_key(&candidate.id, model) == option)
                .map(|model| (candidate.id.clone(), model.clone()))
        })
        .with_context(|| {
            format!("TypeSafe chose `{option}`, which is not one of the candidates it was offered")
        })
}
