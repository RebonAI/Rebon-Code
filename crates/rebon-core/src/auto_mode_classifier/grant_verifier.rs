//! Whether the person's own words authorize a risky step a router proposes.
//!
//! The router drafts a grant ("`git push` to `branch main`, for task t-…")
//! when the person's request names a risky step, or when the person said yes
//! to a step the router proposed. A model draft proves nothing, so before
//! the host issues anything this verifier reads the draft against the
//! person's messages as the host recorded them at the live door — and only
//! those. It sees no transcript, no brief and no reasoning of the router's.
//!
//! 模型给出判定及引文；宿主只核对归一化空白后的引文确实来自记录，
//! 引文本身不能证明授权语义。肯定回复还须核对 `replyingTo` 的提案片段。

use async_trait::async_trait;

use super::{classifier_models, send_classifier_request, AutoModeClassifierStage};
use crate::query::SharedRuntimeModel;

const GRANT_VERIFIER_PROMPT: &str = include_str!("grant_verifier_prompt.md");
/// A quote shorter than this, with no proposal behind it, names nothing.
const MIN_OWN_WORDS_CHARS: usize = 3;

/// One draft and the evidence it is judged on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantVerifierRequest {
    pub action: String,
    pub target: String,
    pub summary: String,
    /// Everything the person typed to the router, oldest first.
    pub statements: Vec<rebon_tool::LauncherUserStatement>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantVerdict {
    Granted {
        user_quote: String,
        proposal_quote: Option<String>,
    },
    Refused {
        reason: String,
    },
}

#[async_trait]
pub trait GrantVerifier: Send + Sync {
    async fn verify(&self, request: &GrantVerifierRequest) -> GrantVerdict;
}

/// The verifier on the session's classifier model.
pub struct ModelGrantVerifier {
    runtime_model: SharedRuntimeModel,
}

impl ModelGrantVerifier {
    pub fn new(runtime_model: SharedRuntimeModel) -> Self {
        Self { runtime_model }
    }
}

#[async_trait]
impl GrantVerifier for ModelGrantVerifier {
    async fn verify(&self, request: &GrantVerifierRequest) -> GrantVerdict {
        if request.statements.is_empty() {
            return GrantVerdict::Refused {
                reason: "nothing the person typed to the router is on record".to_string(),
            };
        }
        let (runtime, session) = self.runtime_model.snapshot();
        let (model, _) = classifier_models(&runtime);
        let session = session.fork_for_sub_agent(None);
        let response = send_classifier_request(
            session.client().as_ref(),
            &model,
            GRANT_VERIFIER_PROMPT,
            verifier_user_prompt(request),
            AutoModeClassifierStage::Thinking,
        )
        .await;
        match response {
            Ok(response) => judge_response(&response, request),
            Err(error) => {
                tracing::warn!(%error, "grant verifier could not be reached");
                GrantVerdict::Refused {
                    reason: "the verifier could not be reached".to_string(),
                }
            }
        }
    }
}

fn verifier_user_prompt(request: &GrantVerifierRequest) -> String {
    let evidence = serde_json::json!({
        "proposedGrant": {
            "action": request.action,
            "target": request.target,
            "summary": request.summary,
        },
        "userMessages": request.statements,
    });
    format!(
        "{}\n\nGive your answer now, in the form described.",
        serde_json::to_string_pretty(&evidence).unwrap_or_default()
    )
}

/// Turn the model's answer into a verdict, checking its quotes against the
/// recorded messages. Anything malformed or unfound is a refusal.
pub(crate) fn judge_response(response: &str, request: &GrantVerifierRequest) -> GrantVerdict {
    let refuse = |reason: &str| GrantVerdict::Refused {
        reason: reason.to_string(),
    };
    let reason = last_tag(response, "reason").unwrap_or_default();
    match last_tag(response, "verdict").as_deref().map(str::trim) {
        Some("granted") => {}
        Some("refused") => {
            return refuse(if reason.is_empty() {
                "the person's words do not name this step"
            } else {
                reason.as_str()
            })
        }
        _ => return refuse("the verifier gave no verdict"),
    }
    let user_quote = last_tag(response, "user_quote").unwrap_or_default();
    let user_quote = user_quote.trim();
    if user_quote.is_empty() {
        return refuse("the verifier cited no words of the person's");
    }
    let proposal_quote = last_tag(response, "proposal_quote")
        .map(|quote| quote.trim().to_string())
        .filter(|quote| !quote.is_empty());

    // The message the quote is from; the latest one, if it appears in more.
    let Some(statement) = request
        .statements
        .iter()
        .rev()
        .find(|statement| contains_loosely(&statement.text, user_quote))
    else {
        return refuse("the cited words are not in anything the person typed");
    };
    // 引文可能是禁止语句的子串；找到原文不等于获得同意。
    let words = statement.text.to_lowercase();
    if [
        "don't",
        "do not",
        "never",
        "not yet",
        "no ",
        "not ",
        "stop",
        "不行",
        "拒绝",
        "不同意",
        "wait",
        "不要",
        "先别",
        "别",
        "请勿",
        "禁止",
        "取消",
        "撤销",
        "撤回",
        "暂停",
    ]
    .iter()
    .any(|denial| words.contains(denial))
    {
        return refuse("the cited message denies or postpones the step");
    }
    match &proposal_quote {
        Some(proposal) => {
            // 无附带动作参数的回复只能使用清晰肯定词；模型不能把「不行」解释成同意。
            if !matches!(
                words.trim(),
                "yes"
                    | "yep"
                    | "yeah"
                    | "go"
                    | "go ahead"
                    | "ok"
                    | "okay"
                    | "sure"
                    | "please do"
                    | "好"
                    | "好的"
                    | "可以"
                    | "同意"
                    | "是的"
                    | "按你说的做"
                    | "按这个方案执行吧"
            ) {
                return refuse("the person's reply did not directly affirm the proposal");
            }
            let answered = statement
                .replying_to
                .as_deref()
                .is_some_and(|reply| contains_loosely(reply, proposal));
            if !answered {
                return refuse("the cited proposal is not what the person was answering");
            }
        }
        None if user_quote.chars().count() < MIN_OWN_WORDS_CHARS => {
            return refuse("the cited words are too short to name a step");
        }
        None => {}
    }
    GrantVerdict::Granted {
        user_quote: user_quote.to_string(),
        proposal_quote,
    }
}

/// The text inside the last `<tag>…</tag>`, or after the last `<tag>` when
/// the answer stopped before its close.
fn last_tag(response: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = response.rfind(&open)? + open.len();
    let rest = &response[start..];
    let end = rest.find(&close).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// `needle` in `haystack`, whitespace runs compared as one space.
fn contains_loosely(haystack: &str, needle: &str) -> bool {
    let squash = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let needle = squash(needle);
    !needle.is_empty() && squash(haystack).contains(&needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::LauncherUserStatement;

    fn request(statements: &[(&str, Option<&str>)]) -> GrantVerifierRequest {
        GrantVerifierRequest {
            action: "git push".into(),
            target: "branch main".into(),
            summary: "push the CI fix to main".into(),
            statements: statements
                .iter()
                .enumerate()
                .map(|(n, (text, reply))| LauncherUserStatement {
                    seq: n as u64 + 1,
                    text: text.to_string(),
                    replying_to: reply.map(str::to_string),
                })
                .collect(),
        }
    }

    fn answer(verdict: &str, user: &str, proposal: &str) -> String {
        format!(
            "<verdict>{verdict}</verdict>\n<user_quote>{user}</user_quote>\n<proposal_quote>{proposal}</proposal_quote>\n<reason>because"
        )
    }

    #[test]
    fn the_persons_own_words_grant_when_they_are_really_there() {
        let request = request(&[("fix CI and then  push it to main", None)]);
        assert_eq!(
            judge_response(&answer("granted", "push it to main", ""), &request),
            GrantVerdict::Granted {
                user_quote: "push it to main".into(),
                proposal_quote: None
            }
        );
    }

    #[test]
    fn a_quote_the_person_never_typed_is_refused() {
        let request = request(&[("fix CI", None)]);
        assert!(matches!(
            judge_response(&answer("granted", "push it to main", ""), &request),
            GrantVerdict::Refused { .. }
        ));
    }

    #[test]
    fn an_explicitly_negated_quote_is_never_a_grant() {
        for text in [
            "actually, don't push to main",
            "do not push to main",
            "No push to main",
            "不要 push to main",
            "wait, not yet; push to main later",
        ] {
            let request = request(&[(text, None)]);
            assert!(
                matches!(
                    judge_response(&answer("granted", "push to main", ""), &request),
                    GrantVerdict::Refused { .. }
                ),
                "{text}"
            );
        }
    }

    #[test]
    fn a_negative_reply_to_a_proposal_is_not_an_affirmation() {
        for reply in ["No", "不行", "stop"] {
            let request = request(&[(reply, Some("Shall I push the fix to main?"))]);
            assert!(
                matches!(
                    judge_response(&answer("granted", reply, "push the fix to main"), &request),
                    GrantVerdict::Refused { .. }
                ),
                "{reply}"
            );
        }
    }

    #[test]
    fn a_yes_grants_only_against_the_proposal_it_answered() {
        let request = request(&[
            ("fix CI", None),
            ("好", Some("CI is green. Shall I push the fix to main?")),
        ]);
        assert!(matches!(
            judge_response(&answer("granted", "好", "push the fix to main"), &request),
            GrantVerdict::Granted { .. }
        ));
        // The proposal it cites is not the one the person answered.
        assert!(matches!(
            judge_response(&answer("granted", "好", "force-push to main"), &request),
            GrantVerdict::Refused { .. }
        ));
        // A bare yes with no proposal behind it names nothing.
        assert!(matches!(
            judge_response(&answer("granted", "好", ""), &request),
            GrantVerdict::Refused { .. }
        ));
    }

    #[test]
    fn a_refusal_or_a_malformed_answer_grants_nothing() {
        let request = request(&[("push it to main", None)]);
        assert!(matches!(
            judge_response(&answer("refused", "", ""), &request),
            GrantVerdict::Refused { .. }
        ));
        assert!(matches!(
            judge_response("sure, looks fine", &request),
            GrantVerdict::Refused { .. }
        ));
        assert!(matches!(
            judge_response(&answer("granted", "", ""), &request),
            GrantVerdict::Refused { .. }
        ));
    }

    #[test]
    fn the_last_tag_wins_and_an_unclosed_reason_still_reads() {
        assert_eq!(
            last_tag(
                "<verdict>refused</verdict> ... <verdict>granted</verdict>",
                "verdict"
            )
            .as_deref(),
            Some("granted")
        );
        assert_eq!(
            last_tag("<reason>cut short", "reason").as_deref(),
            Some("cut short")
        );
        assert_eq!(last_tag("nothing", "reason"), None);
    }

    #[test]
    fn the_prompt_carries_the_evidence_as_data() {
        let prompt = verifier_user_prompt(&request(&[("push it to main", Some("ok?"))]));
        let json_part = prompt.split("\n\nGive your answer").next().unwrap();
        let value: serde_json::Value = serde_json::from_str(json_part).unwrap();
        assert_eq!(value["proposedGrant"]["target"], "branch main");
        assert_eq!(value["userMessages"][0]["replyingTo"], "ok?");
    }
}
