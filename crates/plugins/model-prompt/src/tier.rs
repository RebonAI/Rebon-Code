//! Which prompt profile a model's capability earns.
//!
//! This is the second axis of this plugin, and it is not the same question
//! as [`ModelFamily`](crate::ModelFamily). A family says *this model has a
//! quirk to compensate for* — Astra's batching habit, say — and the
//! compensation goes to that family whatever else is true of it. A tier
//! says *how much scaffolding this model needs*: how to decompose work,
//! when to stop, when to keep going, how many times to be told to be
//! concise.
//!
//! Only scaffolding scales. Facts (paths, tool names, formats) and
//! contracts (confirming irreversible actions, writing the report a
//! coordinator worker owes) go to every tier unchanged — a model does not
//! earn the right to skip a contract by being clever, and it cannot guess a
//! fact by being clever either. Scaffolding is different: it is insurance
//! against a model that would otherwise leave work undone, and for a model
//! that plans its own work it stops being redundant and becomes a *second
//! plan*, competing with the one the model already made. That is where
//! "stopped halfway" and "would not stop" come from.
//!
//! This module classifies models and nothing else: the per-tier
//! scaffolding it was written for was measured and dropped, so no prompt
//! text is changed from here.
//!
//! # Measure first, judge second
//!
//! The first source is a leaderboard. The model table carries each model's
//! standing on LMArena's agent board — agent sessions, which is the work
//! rebon does — and where a standing exists it is the answer.
//!
//! Nothing read off a model's name can do that job. Generation and variant
//! are a reasonable prior right up until they are wrong, and they are
//! wrong across generations: `gemini-3.8-flash` stands at rank 14 while
//! `gemini-3.1-pro-preview` stands at 37, so "pro beats flash" and "newer
//! beats older" disagree, and only one of them is measured. Price cannot
//! do it either — `gpt-5.6-luna` at $1.20/M output and `gpt-5.6-sol` at
//! $20/M declare identical effort vocabularies, and the cheapest model on
//! the board outranks several of the dearest.
//!
//! Boards are slower than launches, so the name-based judgement stays as
//! the *second* source: [`FRONTIER_FAMILIES`] carries a model from its
//! launch until a board rates it, and then stops mattering for it.
//!
//! The last source is one signal — does it reason at all — and it lands in
//! the middle or the bottom. **Neither fallback yields
//! [`ModelTier::Frontier`].** Giving a strong model two sentences it did
//! not need costs a few tokens. Taking scaffolding away from a model that
//! needed it costs work left undone, and nothing in the loop would tell us
//! it happened.
//!
//! Scores travel under CC-BY: any surface that shows one must show
//! [`rebon_api::model_table::score_meta`]'s attribution with it.

use rebon_api::model_table;

/// How much scaffolding a model's prompt carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModelTier {
    /// Plans and checks its own work; extra steering costs performance.
    Frontier,
    /// Does the work, but missed steps and early stops are real.
    Working,
    /// No reasoning, or an unsteady tool loop.
    Plain,
}

/// The frontier line for models **no board has rated yet**.
///
/// A launch beats a leaderboard by days or weeks, and a brand-new flagship
/// should not spend that window being talked to like a small model. This
/// table is what covers the gap; the moment a score arrives for a model it
/// stops applying to it.
///
/// Two axes, in this order. **Generation first** — a bigger number is a
/// stronger model, and `gemini-2.5-pro` is not what `gemini-3.1-pro` is.
/// **Variant second** — within a generation, `pro` and `flash` are
/// different products. Both are priors, not findings: where the board
/// disagrees, the board wins.
///
/// models.dev's `family` carries the variant but not the generation
/// (`gemini-pro` spans 2.5 and 3.1), so the family names the line and
/// [`generation`] reads the number off the model id. A family listed at
/// `0.0` is one whose name already pins a single line (`gpt-astra`,
/// `kimi-k3`), so there is no threshold left to apply.
///
/// The list stays short, and it may not contradict the board. A prior for
/// a family whose rated members sit mid-table is not a prior, it is a
/// wrong answer waiting for the next unrated sibling — which is why
/// `gemini-pro` is absent (the board puts `gemini-3.1-pro-preview` at 37
/// of 43) and why `claude-sonnet` starts at 5 rather than 4.5 (4.6 measured
/// 28). `the_family_priors_do_not_contradict_the_board` enforces exactly
/// that, so this list cannot quietly drift away from the evidence.
///
/// Coarse families stay out on their own account: `glm` spans a flagship
/// and a 4.5-era air model, `gpt` spans fourteen models across four
/// generations, and no version gate rescues a name that vague.
const FRONTIER_FAMILIES: &[(&str, f32)] = &[
    // Each names one model line already.
    ("gpt-astra", 0.0),
    ("gpt-sol", 0.0),
    ("kimi-k3", 0.0),
    // Claude names the variant; the generation is on the id
    // (`claude-opus-4-5` is 4.5, `claude-opus-5` is 5).
    ("claude-opus", 4.5),
    ("claude-sonnet", 5.0),
    ("claude-fable", 5.0),
];

/// The generation on a model id — `gemini-3.1-pro-preview` is 3.1,
/// `claude-opus-4-5` is 4.5, `kimi-k3` is 3, `deepseek-v4-pro` is 4.
///
/// Vendors spell a version three ways: dotted (`5.6`), split across
/// segments (`4-5`), and behind a letter (`k3`, `v4`, `M2.7`). All three
/// are read here.
///
/// Two guards keep it from reading a number that is not a version:
///
/// * only the first three segments are considered, so
///   `deep-research-preview-04-2026` has no generation rather than
///   generation 4 — a date is not a version;
/// * a split minor must be one or two digits, so the dated snapshot in
///   `claude-sonnet-4-5-20250929` stays 4.5 and does not become 4.20250929.
fn generation(model_id: &str) -> Option<f32> {
    let id = model_id.trim().to_ascii_lowercase();
    let segments: Vec<&str> = id.split(['-', '_']).collect();
    for (index, segment) in segments.iter().enumerate().take(3) {
        // A version segment is an optional single letter, then digits,
        // optionally a dotted minor: `5`, `5.6`, `k3`, `m2.7`.
        let digits = segment
            .strip_prefix(|c: char| c.is_ascii_alphabetic())
            .unwrap_or(segment);
        let (major, dotted_minor) = match digits.split_once('.') {
            Some((major, minor)) => (major, Some(minor)),
            None => (digits, None),
        };
        // `4o` is generation 4; the trailing letters are a name, not a
        // version, so they are read off the end rather than rejected.
        let major: String = major.chars().take_while(char::is_ascii_digit).collect();
        if major.is_empty() {
            continue;
        }
        let minor = match dotted_minor {
            Some(minor) => minor
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>(),
            // Claude's `opus-4-5`: the minor is the next segment, but only
            // when it is short enough not to be a date.
            None => segments
                .get(index + 1)
                .filter(|next| {
                    (1..=2).contains(&next.len()) && next.chars().all(|c| c.is_ascii_digit())
                })
                .map(|next| (*next).to_string())
                .unwrap_or_default(),
        };
        let version = if minor.is_empty() {
            major
        } else {
            format!("{major}.{minor}")
        };
        return version.parse().ok();
    }
    None
}

/// Force every model to one tier, for an A/B run.
///
/// `REBON_PROMPT_TIER=frontier|working|plain`. The point of a tier is that
/// the prompt changes with it, and the only way to know whether that helps
/// is to run one model under both prompts — which needs the tier to be
/// separable from the model. An out-of-tree A/B bench is the caller.
///
/// It is read per call rather than once: a bench sets it before the process
/// starts, and a stale cached answer would silently merge the two arms.
fn forced_tier() -> Option<ModelTier> {
    match std::env::var("REBON_PROMPT_TIER")
        .ok()?
        .trim()
        .to_lowercase()
        .as_str()
    {
        "frontier" => Some(ModelTier::Frontier),
        "working" => Some(ModelTier::Working),
        "plain" => Some(ModelTier::Plain),
        // A typo here would silently run both arms of an A/B on the same
        // prompt, so it says so on stderr rather than shrugging.
        other => {
            eprintln!("rebon: ignoring REBON_PROMPT_TIER={other:?} (not a tier)");
            None
        }
    }
}

/// How far down the board a model may stand and still be read as
/// [`ModelTier::Frontier`]: the top quartile.
///
/// A fraction of the board rather than a score, because the board's numbers
/// are on its own scale and move with every publish, while "top quarter of
/// the models anyone bothered to rate against each other" means the same
/// thing next month.
const FRONTIER_STANDING: f64 = 0.25;

/// The tier for `model`.
///
/// Three answers, in order of how much they know:
///
/// 1. **A leaderboard has rated it.** Then that is the answer, and it
///    outranks any guess made from the name. This is the part a version
///    rule cannot do: `gemini-3.8-flash` stands at rank 14 and
///    `gemini-3.1-pro-preview` at 37, so the newer Flash is the better
///    model and no reading of "pro beats flash" would have found that.
/// 2. **No rating, but a family we have judged.** New models ship faster
///    than boards rate them; [`FRONTIER_FAMILIES`] is what carries a launch
///    until a score exists.
/// 3. **Neither.** One signal — does it reason — and the middle or the
///    bottom, never the top.
///
/// Model resolution is the table's own: case-insensitive, a Vertex
/// `@`-suffix ignored, a dated snapshot suffix resolving to its base row. A
/// model no table lists at all is [`ModelTier::Working`], the middle:
/// there is nothing to read, and the bottom would over-steer a capable
/// self-hosted model as surely as the top would under-steer a weak one.
pub fn model_tier(model: &str) -> ModelTier {
    if let Some(forced) = forced_tier() {
        return forced;
    }
    let Some(row) = model_table::model(None, model) else {
        return ModelTier::Working;
    };
    if let Some(score) = row.score.as_ref() {
        return if score.standing() < FRONTIER_STANDING {
            ModelTier::Frontier
        } else {
            // Rated and not near the top. The name does not get a second
            // vote here — being measured is the stronger evidence, and
            // letting the family table override it is how a stale judgement
            // outlives the data that disproved it.
            ModelTier::Working
        };
    }
    let family = row.family.as_deref().unwrap_or_default();
    let frontier = FRONTIER_FAMILIES
        .iter()
        .find(|(known, _)| *known == family)
        .is_some_and(|(_, since)| {
            // A threshold of 0 asks no question; anything else needs a
            // generation to compare, and an id with none does not qualify.
            *since <= 0.0 || generation(&row.id).is_some_and(|found| found >= *since)
        });
    if frontier {
        return ModelTier::Frontier;
    }
    if row.reasoning {
        ModelTier::Working
    } else {
        ModelTier::Plain
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_api::vendor::ProviderVendor;

    #[test]
    fn the_anchors_land_where_the_design_puts_them() {
        for model in [
            "gpt-6-astra",
            "gpt-5.6-sol",
            "claude-opus-5",
            "claude-sonnet-5",
            "kimi-k3",
        ] {
            assert_eq!(model_tier(model), ModelTier::Frontier, "{model}");
        }
        // Capable, but not models we take scaffolding away from.
        for model in [
            "gpt-5.6-luna",
            "gpt-5.6-terra",
            "gpt-5.4-mini",
            "claude-haiku-4-5",
            "glm-5.3",
            "deepseek-v4-flash",
        ] {
            assert_eq!(model_tier(model), ModelTier::Working, "{model}");
        }
        // No reasoning at all.
        assert_eq!(model_tier("gpt-4o"), ModelTier::Plain);
    }

    /// The case a name-based rule gets backwards.
    ///
    /// "Newer beats older" and "pro beats flash" both sound right, and both
    /// fail here: the newer Flash stands at rank 14 on the agent board and
    /// the older Pro at 37. Only the measurement knows.
    #[test]
    fn a_measured_standing_beats_what_the_name_suggests() {
        let flash = model_table::model(None, "gemini-3.8-flash").expect("in the table");
        let pro = model_table::model(None, "gemini-3.1-pro-preview").expect("in the table");
        let flash_score = flash.score.expect("the board rates it");
        let pro_score = pro.score.expect("the board rates it");
        assert!(
            flash_score.rank < pro_score.rank,
            "the board puts the newer flash ahead: {} vs {}",
            flash_score.rank,
            pro_score.rank
        );
        // And the tier follows the board, not the `gemini-pro >= 3.0` prior.
        assert_eq!(model_tier("gemini-3.1-pro-preview"), ModelTier::Working);
        assert_eq!(model_tier("gemini-2.5-pro"), ModelTier::Working);
    }

    /// A rated model does not get a second vote from its family.
    ///
    /// `claude-sonnet-4-6` sits in a frontier family and clears the 4.5
    /// generation gate; the board puts it at rank 28 of 43. The measurement
    /// is what stands, or a stale judgement outlives the data that
    /// disproved it.
    #[test]
    fn the_family_table_does_not_override_a_measured_model() {
        let row = model_table::model(None, "claude-sonnet-4-6").expect("in the table");
        assert_eq!(row.family.as_deref(), Some("claude-sonnet"));
        assert!(generation("claude-sonnet-4-6").unwrap() >= 4.5);
        assert!(row.score.is_some(), "the board rates it");
        assert_eq!(model_tier("claude-sonnet-4-6"), ModelTier::Working);
    }

    /// The family table still carries a model no board has reached.
    #[test]
    fn an_unrated_model_falls_back_on_its_family_and_generation() {
        for model in ["gemini-3.5-flash", "claude-opus-4-6"] {
            let row = model_table::model(None, model).expect("in the table");
            assert!(
                row.score.is_none(),
                "{model} is unrated, which is the point"
            );
        }
        // Frontier family, generation over the threshold, no score.
        assert_eq!(model_tier("claude-opus-4-6"), ModelTier::Frontier);
        // Neither, so the reasoning signal decides.
        assert_eq!(model_tier("gemini-3.5-flash"), ModelTier::Working);
        // And an unrated sibling of a mid-table line does not inherit a
        // standing its family never had.
        assert_eq!(model_tier("gemini-3-pro-image"), ModelTier::Working);
    }

    #[test]
    fn the_generation_parser_reads_every_spelling_vendors_use() {
        assert_eq!(generation("gemini-3.1-pro-preview"), Some(3.1));
        assert_eq!(generation("gemini-3-flash-preview"), Some(3.0));
        assert_eq!(generation("gpt-5.6-sol"), Some(5.6));
        assert_eq!(generation("gpt-6-astra"), Some(6.0));
        assert_eq!(generation("gpt-4o"), Some(4.0));
        // Split across segments, and the dated snapshot after it must not
        // be swallowed as the minor.
        assert_eq!(generation("claude-opus-4-5"), Some(4.5));
        assert_eq!(generation("claude-sonnet-4-5-20250929"), Some(4.5));
        assert_eq!(generation("claude-opus-5"), Some(5.0));
        // Behind a letter.
        assert_eq!(generation("kimi-k3"), Some(3.0));
        assert_eq!(generation("deepseek-v4-pro"), Some(4.0));
        assert_eq!(generation("minimax-m2.7"), Some(2.7));
        // A date is not a version: the number sits too far into the id.
        assert_eq!(generation("deep-research-preview-04-2026"), None);
        assert_eq!(generation("some-self-hosted-thing"), None);
    }

    /// A dated research preview shares `gemini-pro` with the real Pro
    /// models. It must not reach the top tier by having a number in it.
    #[test]
    fn a_dated_preview_does_not_ride_its_familys_threshold() {
        assert_eq!(
            model_tier("deep-research-preview-04-2026"),
            ModelTier::Working
        );
    }

    #[test]
    fn an_unlisted_model_lands_in_the_middle_rather_than_at_either_end() {
        assert_eq!(model_tier("some-self-hosted-thing"), ModelTier::Working);
        assert_eq!(model_tier(""), ModelTier::Working);
    }

    #[test]
    fn a_dated_snapshot_suffix_and_odd_casing_resolve_to_the_base_row() {
        assert_eq!(model_tier("gpt-6-astra-20260903"), ModelTier::Frontier);
        assert_eq!(model_tier("GPT-6-Astra"), ModelTier::Frontier);
    }

    /// Tier and family are independent axes: Astra is a frontier model
    /// *and* has a quirk to compensate for, and neither answer may start
    /// depending on the other.
    #[test]
    fn tier_and_family_are_independent() {
        assert_eq!(model_tier("gpt-6-astra"), ModelTier::Frontier);
        assert_eq!(
            crate::model_family("gpt-6-astra"),
            crate::ModelFamily::Astra
        );

        assert_eq!(model_tier("claude-opus-5"), ModelTier::Frontier);
        assert_eq!(
            crate::model_family("claude-opus-5"),
            crate::ModelFamily::Default
        );
    }

    /// Every frontier family must still exist upstream.
    ///
    /// The tier table is keyed on models.dev's `family`, so an upstream
    /// rename would not fail anything — it would silently drop a model to
    /// the middle tier and nobody would notice. This is the guard.
    #[test]
    fn every_frontier_family_is_still_a_family_the_table_carries() {
        let mut seen: Vec<&str> = Vec::new();
        for vendor in ProviderVendor::ALL {
            for model in model_table::models_for_vendor(*vendor) {
                let Some(family) = model.family.as_deref() else {
                    continue;
                };
                if let Some((known, _)) =
                    FRONTIER_FAMILIES.iter().find(|(known, _)| *known == family)
                {
                    if !seen.contains(known) {
                        seen.push(known);
                    }
                }
            }
        }
        let missing: Vec<&str> = FRONTIER_FAMILIES
            .iter()
            .map(|(family, _)| *family)
            .filter(|family| !seen.contains(family))
            .collect();
        assert!(
            missing.is_empty(),
            "{missing:?} no longer name a family in the model table; upstream renamed \
             them or dropped them, and every model that was in them has silently \
             fallen to the middle tier"
        );
    }

    /// A family listed with a threshold must still have a model that meets
    /// it. `("gemini-pro", 3.0)` going quiet would mean either the ids
    /// stopped spelling their generation the way [`generation`] reads it,
    /// or the whole line left the table — both silent demotions.
    #[test]
    fn every_frontier_threshold_still_admits_something() {
        for (family, since) in FRONTIER_FAMILIES {
            if *since <= 0.0 {
                continue;
            }
            let admitted = ProviderVendor::ALL.iter().any(|vendor| {
                model_table::models_for_vendor(*vendor).iter().any(|model| {
                    model.family.as_deref() == Some(*family)
                        && generation(&model.id).is_some_and(|found| found >= *since)
                })
            });
            assert!(
                admitted,
                "no model in family {family:?} reaches generation {since}; the whole \
                 family has silently fallen to the middle tier"
            );
        }
    }

    /// Nothing reaches the top tier except by being measured there or by
    /// being named in the list.
    #[test]
    fn the_fallback_never_yields_frontier() {
        for vendor in ProviderVendor::ALL {
            for model in model_table::models_for_vendor(*vendor) {
                if model_tier(&model.id) != ModelTier::Frontier {
                    continue;
                }
                let family = model.family.as_deref().unwrap_or_default();
                let measured_there = model
                    .score
                    .as_ref()
                    .is_some_and(|score| score.standing() < FRONTIER_STANDING);
                let listed = FRONTIER_FAMILIES.iter().any(|(known, _)| *known == family);
                assert!(
                    measured_there || listed,
                    "{} reached Frontier with neither a top-quartile standing nor a listed family ({family:?})",
                    model.id
                );
            }
        }
    }

    /// A prior may not contradict the board it is standing in for.
    ///
    /// [`FRONTIER_FAMILIES`] exists to carry a model until a leaderboard
    /// reaches it. If the board has already reached that family's models
    /// and put them mid-table, the entry is not covering a gap — it is
    /// asserting something the data denies, and the next unrated sibling
    /// inherits the wrong answer. So: every rated model that clears a
    /// listed threshold must in fact stand in the top quartile.
    #[test]
    fn the_family_priors_do_not_contradict_the_board() {
        for (family, since) in FRONTIER_FAMILIES {
            for vendor in ProviderVendor::ALL {
                for model in model_table::models_for_vendor(*vendor) {
                    if model.family.as_deref() != Some(*family) {
                        continue;
                    }
                    let clears =
                        *since <= 0.0 || generation(&model.id).is_some_and(|found| found >= *since);
                    let Some(score) = model.score.as_ref().filter(|_| clears) else {
                        continue;
                    };
                    assert!(
                        score.standing() < FRONTIER_STANDING,
                        "{} is rated {}/{} — the prior ({family:?} >= {since}) claims a \
                         standing the board does not give it",
                        model.id,
                        score.rank,
                        score.of
                    );
                }
            }
        }
    }

    /// The snapshot must carry scores, and carry the credit they travel
    /// under. A sync that silently stopped joining them would hand every
    /// tier back to the weaker source with nothing on screen to say so.
    #[test]
    fn the_snapshot_carries_scores_and_their_attribution() {
        let meta = model_table::score_meta().expect("the snapshot names a board");
        assert!(!meta.attribution.is_empty(), "CC-BY needs the credit line");
        assert!(meta.count > 0);

        let scored = ProviderVendor::ALL
            .iter()
            .flat_map(|vendor| model_table::models_for_vendor(*vendor))
            .filter(|model| model.score.is_some())
            .count();
        assert!(scored > 20, "only {scored} models carry a standing");
    }

    /// Not an assertion — the review surface. `cargo test -p
    /// rebon-plugin-model-prompt tier_snapshot -- --nocapture` prints how
    /// every model in the table is read, which is the only way a human
    /// catches a judgement that has gone stale.
    #[test]
    fn tier_snapshot() {
        let mut counts = [0usize; 3];
        for vendor in ProviderVendor::ALL {
            let models = model_table::models_for_vendor(*vendor);
            if models.is_empty() {
                continue;
            }
            println!("\n== {vendor:?} ==");
            for model in models {
                if !vendor.is_chat_model_id(&model.id) {
                    continue;
                }
                let tier = model_tier(&model.id);
                counts[tier as usize] += 1;
                let standing = match model.score.as_ref() {
                    Some(score) => format!("rank {}/{}", score.rank, score.of),
                    None => "unrated".to_string(),
                };
                println!(
                    "  {:<28} {:<9} {:<13} family={}",
                    model.id,
                    format!("{tier:?}"),
                    standing,
                    model.family.as_deref().unwrap_or("-")
                );
            }
        }
        println!(
            "\nfrontier={} working={} plain={}",
            counts[0], counts[1], counts[2]
        );
        assert!(counts[0] > 0 && counts[1] > 0, "the snapshot read nothing");
    }
}
