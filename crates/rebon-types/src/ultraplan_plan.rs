use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ultraplan_plan_hash, ExecutionCard, PlanCoverageResult, RequirementLedgerEntry};

pub const ULTRAPLAN_PLAN_ANALYZER_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStepCoverageInput {
    pub step_id: String,
    #[serde(default)]
    pub requirement_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStepAnalysis {
    pub step_id: String,
    pub title: String,
    pub line: usize,
    pub requirement_ids: Vec<String>,
    pub card: ExecutionCard,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanAnalysis {
    pub analyzer_version: u32,
    pub plan_hash: String,
    pub requirements_hash: String,
    pub steps: Vec<PlanStepAnalysis>,
    pub coverage: PlanCoverageResult,
    #[serde(default)]
    pub coverage_by_requirement: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub duplicate_step_ids: Vec<String>,
    #[serde(default)]
    pub unknown_step_ids: Vec<String>,
    #[serde(default)]
    pub incomplete_step_ids: Vec<String>,
}

impl PlanAnalysis {
    pub fn is_structurally_valid(&self) -> bool {
        !self.steps.is_empty()
            && self.duplicate_step_ids.is_empty()
            && self.unknown_step_ids.is_empty()
            && self.incomplete_step_ids.is_empty()
            && self.coverage.missing.is_empty()
            && self.coverage.unknown_ids.is_empty()
    }
}

pub fn ultraplan_plan_step_ids(plan: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    parse_plan_steps(plan)
        .into_iter()
        .filter_map(|step| seen.insert(step.step_id.clone()).then_some(step.step_id))
        .collect()
}

pub fn analyze_ultraplan_plan(
    plan: &str,
    requirements: &[RequirementLedgerEntry],
    step_coverage: &[PlanStepCoverageInput],
) -> PlanAnalysis {
    let parsed_steps = parse_plan_steps(plan);
    let mut seen_steps = BTreeSet::new();
    let mut duplicate_step_ids = BTreeSet::new();
    for step in &parsed_steps {
        if !seen_steps.insert(step.step_id.clone()) {
            duplicate_step_ids.insert(step.step_id.clone());
        }
    }

    let known_steps = parsed_steps
        .iter()
        .map(|step| step.step_id.clone())
        .collect::<BTreeSet<_>>();
    let known_requirements = requirements
        .iter()
        .map(|entry| entry.id.clone())
        .collect::<BTreeSet<_>>();
    let mut coverage_by_step: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut coverage_by_requirement: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut unknown_step_ids = BTreeSet::new();
    let mut unknown_requirement_ids = BTreeSet::new();

    for mapping in step_coverage {
        let step_id =
            canonical_step_id(&mapping.step_id).unwrap_or_else(|| mapping.step_id.clone());
        if !known_steps.contains(&step_id) {
            unknown_step_ids.insert(step_id);
            continue;
        }
        for requirement_id in &mapping.requirement_ids {
            if !known_requirements.contains(requirement_id) {
                unknown_requirement_ids.insert(requirement_id.clone());
                continue;
            }
            coverage_by_step
                .entry(step_id.clone())
                .or_default()
                .insert(requirement_id.clone());
            coverage_by_requirement
                .entry(requirement_id.clone())
                .or_default()
                .insert(step_id.clone());
        }
    }

    let covered = coverage_by_requirement.keys().cloned().collect::<Vec<_>>();
    let missing = known_requirements
        .difference(&coverage_by_requirement.keys().cloned().collect())
        .cloned()
        .collect::<Vec<_>>();
    let unknown_ids = unknown_requirement_ids.into_iter().collect::<Vec<_>>();

    let mut incomplete_step_ids = Vec::new();
    let steps = parsed_steps
        .into_iter()
        .map(|mut step| {
            let requirement_ids = coverage_by_step
                .remove(&step.step_id)
                .unwrap_or_default()
                .into_iter()
                .collect::<Vec<_>>();
            step.card.covers = (!requirement_ids.is_empty()).then(|| requirement_ids.join(","));
            if !step.card.is_complete() {
                incomplete_step_ids.push(step.step_id.clone());
            }
            PlanStepAnalysis {
                step_id: step.step_id,
                title: step.title,
                line: step.line,
                requirement_ids,
                card: step.card,
            }
        })
        .collect();

    let requirements_hash = {
        let bytes = serde_json::to_vec(requirements).unwrap_or_default();
        format!("{:x}", Sha256::digest(bytes))
    };

    PlanAnalysis {
        analyzer_version: ULTRAPLAN_PLAN_ANALYZER_VERSION,
        plan_hash: ultraplan_plan_hash(plan),
        requirements_hash,
        steps,
        coverage: PlanCoverageResult {
            covered,
            missing,
            unknown_ids,
        },
        coverage_by_requirement: coverage_by_requirement
            .into_iter()
            .map(|(requirement_id, step_ids)| (requirement_id, step_ids.into_iter().collect()))
            .collect(),
        duplicate_step_ids: duplicate_step_ids.into_iter().collect(),
        unknown_step_ids: unknown_step_ids.into_iter().collect(),
        incomplete_step_ids,
    }
}

#[derive(Debug)]
struct ParsedStep {
    step_id: String,
    title: String,
    line: usize,
    card: ExecutionCard,
}

fn parse_plan_steps(plan: &str) -> Vec<ParsedStep> {
    let mut steps = Vec::new();
    let mut current: Option<ParsedStep> = None;
    let mut in_fence = false;

    for (index, raw_line) in plan.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if let Some((step_id, title)) = parse_step_heading(line) {
            if let Some(step) = current.take() {
                steps.push(step);
            }
            current = Some(ParsedStep {
                card: ExecutionCard {
                    step: step_id.clone(),
                    covers: None,
                    files: Vec::new(),
                    change: String::new(),
                    verify: String::new(),
                },
                step_id,
                title,
                line: line_number,
            });
            continue;
        }
        let Some(step) = current.as_mut() else {
            continue;
        };
        if let Some(value) = line
            .strip_prefix("- files:")
            .or_else(|| line.strip_prefix("* files:"))
        {
            step.card.files.extend(split_files(value));
        } else if let Some(value) = line
            .strip_prefix("- change:")
            .or_else(|| line.strip_prefix("* change:"))
        {
            step.card.change = value.trim().to_string();
        } else if let Some(value) = line
            .strip_prefix("- verify:")
            .or_else(|| line.strip_prefix("* verify:"))
        {
            step.card.verify = value.trim().to_string();
        }
    }
    if let Some(step) = current {
        steps.push(step);
    }
    steps
}

fn parse_step_heading(line: &str) -> Option<(String, String)> {
    let candidate = line.trim_start_matches('#').trim_start();
    if let Some(rest) = candidate.strip_prefix("Step ") {
        let token = rest
            .split_whitespace()
            .next()?
            .trim_end_matches(['.', ':', ')']);
        let step_id = canonical_step_id(token)?;
        let title = rest[token.len()..]
            .trim_start_matches(['.', ':', ')', ' ', '-'])
            .split("[COVERS:")
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        return Some((step_id, title));
    }

    let marker = candidate.strip_prefix('P')?;
    let digit_count = marker.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digit_count == 0 {
        return None;
    }
    let suffix = &marker[digit_count..];
    if !suffix.starts_with(['.', ':', ')']) {
        return None;
    }
    let step_id = format!("P{}", &marker[..digit_count]);
    let title = suffix[1..]
        .split("[COVERS:")
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    Some((step_id, title))
}

fn canonical_step_id(value: &str) -> Option<String> {
    let value = value.trim();
    let digits = value.strip_prefix('P').unwrap_or(value);
    (!digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit()))
        .then(|| format!("P{digits}"))
}

fn split_files(value: &str) -> Vec<String> {
    value
        .split([',', ';'])
        .map(str::trim)
        .filter(|part| !part.is_empty() && !part.eq_ignore_ascii_case("none"))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RequirementSource;

    fn requirements(ids: &[&str]) -> Vec<RequirementLedgerEntry> {
        ids.iter()
            .map(|id| RequirementLedgerEntry {
                id: (*id).to_string(),
                title: format!("Requirement {id}"),
                source: RequirementSource::Question,
                round_added: 1,
            })
            .collect()
    }

    #[test]
    fn step_id_projection_uses_the_canonical_parser() {
        let plan = "### Step 1: First\n```\nP9. fake\n```\nP2) Second\nP2. Duplicate";
        assert_eq!(ultraplan_plan_step_ids(plan), vec!["P1", "P2"]);
    }

    #[test]
    fn generates_coverage_from_typed_mapping_without_markers() {
        let plan = "### Step 1\n- files: src/a.rs\n- change: change a\n- verify: cargo test\n\nP2. Finish\n- files: src/b.rs\n- change: change b\n- verify: cargo test";
        let analysis = analyze_ultraplan_plan(
            plan,
            &requirements(&["R1", "R2"]),
            &[
                PlanStepCoverageInput {
                    step_id: "P1".into(),
                    requirement_ids: vec!["R1".into()],
                },
                PlanStepCoverageInput {
                    step_id: "2".into(),
                    requirement_ids: vec!["R2".into()],
                },
            ],
        );

        assert!(analysis.is_structurally_valid());
        assert_eq!(analysis.coverage.covered, vec!["R1", "R2"]);
        assert_eq!(analysis.steps[0].card.covers.as_deref(), Some("R1"));
        assert_eq!(analysis.steps[1].step_id, "P2");
    }

    #[test]
    fn reports_missing_unknown_and_unknown_steps_from_one_requirement_set() {
        let plan = "P1. Work\n- files: src/a.rs\n- change: change\n- verify: test";
        let analysis = analyze_ultraplan_plan(
            plan,
            &requirements(&["R1", "R2"]),
            &[
                PlanStepCoverageInput {
                    step_id: "P1".into(),
                    requirement_ids: vec!["R1".into(), "R9".into()],
                },
                PlanStepCoverageInput {
                    step_id: "P9".into(),
                    requirement_ids: vec!["R2".into()],
                },
            ],
        );

        assert_eq!(analysis.coverage.covered, vec!["R1"]);
        assert_eq!(analysis.coverage.missing, vec!["R2"]);
        assert_eq!(analysis.coverage.unknown_ids, vec!["R9"]);
        assert_eq!(analysis.unknown_step_ids, vec!["P9"]);
        assert!(analysis
            .coverage
            .missing
            .iter()
            .all(|id| !analysis.coverage.unknown_ids.contains(id)));
    }

    #[test]
    fn ignores_fake_steps_in_fenced_code_and_flags_incomplete_steps() {
        let plan = "```\nP9. fake\n```\nP1. Real\n- files: src/a.rs\n- change: change";
        let analysis = analyze_ultraplan_plan(
            plan,
            &requirements(&["R1"]),
            &[PlanStepCoverageInput {
                step_id: "P1".into(),
                requirement_ids: vec!["R1".into()],
            }],
        );

        assert_eq!(analysis.steps.len(), 1);
        assert_eq!(analysis.incomplete_step_ids, vec!["P1"]);
        assert!(!analysis.is_structurally_valid());
    }
}
