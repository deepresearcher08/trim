use crate::unit::CodeUnit;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Inclusion {
    Full,
    Compact,
    Skeleton,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkeletonReason {
    /// Skeletonized specifically because the token budget ran out before it could be upgraded to full/compact
    BudgetExhausted,
    /// Skeletonized because it had low relevance score compared to other candidate symbols
    LowRelevance,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedUnit {
    pub unit_id: usize,
    pub inclusion: Inclusion,
    pub tokens: usize,
    pub score: f32,
    pub skeleton_reason: Option<SkeletonReason>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetPlan {
    pub budget_tokens: usize,
    pub used_tokens: usize,
    pub included: Vec<PlannedUnit>,
    pub excluded_unit_ids: Vec<usize>,
    /// Unit IDs that had high/medium relevance but remained skeletons specifically due to budget exhaustion
    pub budget_exhausted_units: Vec<usize>,
}

/// Three-tier greedy budget allocation:
///
/// Pass 1 (High-Relevance Admission & Depth) — For candidate code units in descending
/// relevance order: admit their structural skeleton, then greedily upgrade to Full implementation
/// if marginal cost fits. If Full implementation is just over remaining budget, upgrade to Compact body
/// (docstring + signature + first statements + compact elision), killing the hard cliff.
///
/// Pass 2 (Breadth Filling) — For background / lower-relevance units: admit structural skeletons
/// with remaining budget to maximize codebase visibility and outline context.
///
/// Pass 3 (Second-Chance Compact Sweep) — Iterate remaining skeleton units in score order
/// to upgrade them to Compact format with any leftover token budget.
///
/// Tie-breaking is strictly deterministic: (score desc, file asc, start_line asc, name asc, id asc).
pub fn select_within_budget(
    units: &[CodeUnit],
    scores: &HashMap<usize, f32>,
    budget_tokens: usize,
) -> BudgetPlan {
    let mut order: Vec<&CodeUnit> = units.iter().collect();
    order.sort_by(|a, b| {
        let sa = scores.get(&a.id).copied().unwrap_or(0.0);
        let sb = scores.get(&b.id).copied().unwrap_or(0.0);
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.start_line.cmp(&b.start_line))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut used = 0usize;
    let mut included: Vec<PlannedUnit> = Vec::new();
    let mut excluded: Vec<usize> = Vec::new();
    let mut budget_exhausted_units = Vec::new();

    let has_positive_scores = order
        .iter()
        .any(|u| scores.get(&u.id).copied().unwrap_or(0.0) > 0.0);

    // Pass 1: For candidates in priority order, admit skeleton and immediately attempt depth upgrade (Full/Compact)
    for u in &order {
        let score = scores.get(&u.id).copied().unwrap_or(0.0);
        let is_candidate = !has_positive_scores || score > 0.0;

        if is_candidate {
            if used + u.est_tokens_skeleton <= budget_tokens {
                used += u.est_tokens_skeleton;

                let mut current_inclusion = Inclusion::Skeleton;
                let mut current_tokens = u.est_tokens_skeleton;
                let mut skeleton_reason = None;

                if u.est_tokens_full <= u.est_tokens_skeleton {
                    // Nothing to gain (const/static)
                    current_inclusion = Inclusion::Full;
                    current_tokens = u.est_tokens_full;
                } else {
                    let marginal_full = u.est_tokens_full - u.est_tokens_skeleton;
                    if used + marginal_full <= budget_tokens {
                        used += marginal_full;
                        current_inclusion = Inclusion::Full;
                        current_tokens = u.est_tokens_full;
                    } else if u.est_tokens_compact > u.est_tokens_skeleton {
                        let marginal_compact = u.est_tokens_compact - u.est_tokens_skeleton;
                        if used + marginal_compact <= budget_tokens {
                            used += marginal_compact;
                            current_inclusion = Inclusion::Compact;
                            current_tokens = u.est_tokens_compact;
                        } else {
                            skeleton_reason = Some(SkeletonReason::BudgetExhausted);
                            budget_exhausted_units.push(u.id);
                        }
                    } else {
                        skeleton_reason = Some(SkeletonReason::BudgetExhausted);
                        budget_exhausted_units.push(u.id);
                    }
                }

                included.push(PlannedUnit {
                    unit_id: u.id,
                    inclusion: current_inclusion,
                    tokens: current_tokens,
                    score,
                    skeleton_reason,
                });
            } else {
                excluded.push(u.id);
            }
        }
    }

    // Pass 2: Breadth filling for background units (score <= 0.0)
    let already_included: HashMap<usize, usize> = included
        .iter()
        .enumerate()
        .map(|(idx, p)| (p.unit_id, idx))
        .collect();

    for u in &order {
        if !already_included.contains_key(&u.id) && !excluded.contains(&u.id) {
            let score = scores.get(&u.id).copied().unwrap_or(0.0);
            if used + u.est_tokens_skeleton <= budget_tokens {
                used += u.est_tokens_skeleton;
                included.push(PlannedUnit {
                    unit_id: u.id,
                    inclusion: Inclusion::Skeleton,
                    tokens: u.est_tokens_skeleton,
                    score,
                    skeleton_reason: Some(SkeletonReason::LowRelevance),
                });
            } else {
                excluded.push(u.id);
            }
        }
    }

    // Pass 3: Second-chance compact sweep for remaining skeletons with leftover budget
    let included_by_id: HashMap<usize, usize> = included
        .iter()
        .enumerate()
        .map(|(idx, p)| (p.unit_id, idx))
        .collect();

    for u in &order {
        let Some(&idx) = included_by_id.get(&u.id) else {
            continue;
        };
        if included[idx].inclusion == Inclusion::Skeleton {
            if u.est_tokens_compact > u.est_tokens_skeleton {
                let marginal = u.est_tokens_compact - u.est_tokens_skeleton;
                if used + marginal <= budget_tokens {
                    used += marginal;
                    included[idx].inclusion = Inclusion::Compact;
                    included[idx].tokens = u.est_tokens_compact;
                    included[idx].skeleton_reason = None;
                }
            }
        }
    }

    BudgetPlan {
        budget_tokens,
        used_tokens: used,
        included,
        excluded_unit_ids: excluded,
        budget_exhausted_units,
    }
}

/// Render a BudgetPlan back into the final prompt payload text, grouped by
/// file, with files ordered by relevance score and units in original source order.
pub fn render_payload(units: &[CodeUnit], plan: &BudgetPlan) -> String {
    let mut plan_by_id: HashMap<usize, &PlannedUnit> = HashMap::new();
    for p in &plan.included {
        plan_by_id.insert(p.unit_id, p);
    }

    // Map file -> max score among its included units
    let mut file_scores: HashMap<&std::path::PathBuf, f32> = HashMap::new();
    for p in &plan.included {
        if let Some(u) = units.iter().find(|u| u.id == p.unit_id) {
            let entry = file_scores.entry(&u.file).or_insert(0.0);
            if p.score > *entry {
                *entry = p.score;
            }
        }
    }

    let mut files: Vec<&std::path::PathBuf> = file_scores.keys().copied().collect();
    files.sort_by(|a, b| {
        let sa = file_scores.get(a).copied().unwrap_or(0.0);
        let sb = file_scores.get(b).copied().unwrap_or(0.0);
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.cmp(b))
    });

    let mut out = String::new();
    for file in files {
        let mut file_units: Vec<&CodeUnit> = units
            .iter()
            .filter(|u| &u.file == file && plan_by_id.contains_key(&u.id))
            .collect();
        if file_units.is_empty() {
            continue;
        }
        file_units.sort_by_key(|u| u.start_line);

        // Suppress child units whose parent container in the same file is already rendered in Full
        let mut rendered_units = Vec::new();
        for u in &file_units {
            let is_enclosed_by_full_parent = file_units.iter().any(|parent| {
                if parent.id == u.id {
                    return false;
                }
                let is_enclosed = parent.start_line <= u.start_line && parent.end_line >= u.end_line;
                if !is_enclosed {
                    return false;
                }
                plan_by_id
                    .get(&parent.id)
                    .map(|p| p.inclusion == Inclusion::Full)
                    .unwrap_or(false)
            });

            if !is_enclosed_by_full_parent {
                rendered_units.push(*u);
            }
        }

        if rendered_units.is_empty() {
            continue;
        }

        out.push_str(&format!("// === {} ===\n", file.display().to_string().replace('\\', "/")));
        for u in rendered_units {
            let planned = plan_by_id[&u.id];
            let text = match planned.inclusion {
                Inclusion::Full => &u.full_text,
                Inclusion::Compact => &u.compact_text,
                Inclusion::Skeleton => &u.skeleton_text,
            };
            out.push_str(text);
            out.push_str("\n\n");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::UnitKind;
    use std::path::PathBuf;

    fn make_test_unit(
        id: usize,
        name: &str,
        tokens_full: usize,
        tokens_compact: usize,
        tokens_skeleton: usize,
    ) -> CodeUnit {
        CodeUnit {
            id,
            file: PathBuf::from("src/test.rs"),
            kind: UnitKind::Function,
            name: name.to_string(),
            doc_comment: None,
            signature: format!("fn {}()", name),
            full_text: format!("fn {}() {{ /* long body */ }}", name),
            compact_text: format!("fn {}() {{ /* compact */ }}", name),
            skeleton_text: format!("fn {}() {{ /* skeleton */ }}", name),
            start_line: id * 10,
            end_line: id * 10 + 9,
            est_tokens_full: tokens_full,
            est_tokens_compact: tokens_compact,
            est_tokens_skeleton: tokens_skeleton,
            references: vec![],
        }
    }

    #[test]
    fn test_three_tier_graceful_degradation() {
        let u1 = make_test_unit(0, "important_func", 100, 40, 10);
        let u2 = make_test_unit(1, "secondary_func", 100, 40, 10);
        let units = vec![u1, u2];

        let mut scores = HashMap::new();
        scores.insert(0, 10.0);
        scores.insert(1, 5.0);

        // Budget = 50:
        // u1 is top candidate: skeleton admitted (10), full (marginal 90) doesn't fit, compact (marginal 30) fits! Used = 40.
        // u2 is second candidate: skeleton admitted (10). Used = 50.
        // Hard cliff killed: u1 degrades to Compact instead of staying Skeleton!
        let plan = select_within_budget(&units, &scores, 50);
        assert_eq!(plan.included.len(), 2);
        assert_eq!(plan.included[0].inclusion, Inclusion::Compact);
        assert_eq!(plan.included[1].inclusion, Inclusion::Skeleton);
        assert_eq!(plan.used_tokens, 50);
    }

    #[test]
    fn test_deterministic_tie_breaking() {
        let u1 = make_test_unit(0, "alpha", 50, 25, 10);
        let u2 = make_test_unit(1, "beta", 50, 25, 10);
        let units = vec![u2.clone(), u1.clone()]; // reverse order in input

        let scores = HashMap::new(); // identical 0.0 scores
        let plan1 = select_within_budget(&units, &scores, 100);
        let plan2 = select_within_budget(&units, &scores, 100);

        assert_eq!(plan1.included[0].unit_id, plan2.included[0].unit_id);
        assert_eq!(plan1.included[0].unit_id, 0); // alpha sorted first by line/file
    }

    #[test]
    fn test_render_payload_deduplication_and_file_ordering() {
        let class_unit = CodeUnit {
            id: 0,
            file: PathBuf::from("src/auth.ts"),
            kind: UnitKind::Class,
            name: "AuthService".to_string(),
            doc_comment: None,
            signature: "class AuthService".to_string(),
            full_text: "class AuthService {\n    validate(token: string) { return true; }\n}".to_string(),
            compact_text: "class AuthService {\n    validate(token: string) { return true; }\n}".to_string(),
            skeleton_text: "class AuthService {\n    /* ... body elided ... */\n}".to_string(),
            start_line: 1,
            end_line: 10,
            est_tokens_full: 40,
            est_tokens_compact: 40,
            est_tokens_skeleton: 10,
            references: vec![],
        };

        let method_unit = CodeUnit {
            id: 1,
            file: PathBuf::from("src/auth.ts"),
            kind: UnitKind::Method,
            name: "validate".to_string(),
            doc_comment: None,
            signature: "validate(token: string)".to_string(),
            full_text: "validate(token: string) { return true; }".to_string(),
            compact_text: "validate(token: string) { return true; }".to_string(),
            skeleton_text: "validate(token: string) { /* ... */ }".to_string(),
            start_line: 2,
            end_line: 4,
            est_tokens_full: 15,
            est_tokens_compact: 15,
            est_tokens_skeleton: 5,
            references: vec![],
        };

        let units = vec![class_unit, method_unit];
        let mut scores = HashMap::new();
        scores.insert(0, 5.0);
        scores.insert(1, 10.0);

        // When budget allows both Full, method should NOT be duplicated in payload
        let plan = select_within_budget(&units, &scores, 100);
        let payload = render_payload(&units, &plan);

        let validate_count = payload.matches("validate(token: string)").count();
        assert_eq!(validate_count, 1, "validate method should appear exactly once, but appeared {validate_count} times in:\n{payload}");
    }
}