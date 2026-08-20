use crate::unit::CodeUnit;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

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
pub struct BudgetCannibalizationWarning {
    pub unit_id: usize,
    pub unit_name: String,
    pub tokens_used: usize,
    pub pct_of_budget: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetPlan {
    pub budget_tokens: usize,
    pub used_tokens: usize,
    pub included: Vec<PlannedUnit>,
    pub excluded_unit_ids: Vec<usize>,
    /// Unit IDs that had high/medium relevance but remained skeletons specifically due to budget exhaustion
    pub budget_exhausted_units: Vec<usize>,
    /// Warnings for units that consume >60% of total budget
    pub cannibalization_warnings: Vec<BudgetCannibalizationWarning>,
}

/// Three-tier greedy budget allocation with degradation sanity and empty-intent coverage:
///
/// Pass 1 (Candidate Admission & Depth) — For candidate code units in priority order:
/// admit structural skeleton, then greedily upgrade to Full or Compact tier.
/// Prevents a single unit from cannibalizing the entire budget when multiple candidates exist.
///
/// Pass 2 (Breadth Filling) — For background units: admit structural skeletons
/// with remaining budget to maximize codebase visibility.
///
/// Pass 3 (Second-Chance Compact Sweep) — Iterate remaining skeleton units in score order
/// to upgrade them to Compact format with leftover budget.
pub fn select_within_budget(
    units: &[CodeUnit],
    scores: &HashMap<usize, f32>,
    budget_tokens: usize,
) -> BudgetPlan {
    let has_positive_scores = units
        .iter()
        .any(|u| scores.get(&u.id).copied().unwrap_or(0.0) > 0.0);

    let mut order: Vec<&CodeUnit> = units.iter().collect();

    if !has_positive_scores {
        // Empty intent: coverage-first round-robin ordering across files/modules
        let mut by_file: HashMap<&Path, Vec<&CodeUnit>> = HashMap::new();
        for u in units {
            by_file.entry(&u.file).or_default().push(u);
        }
        let mut file_keys: Vec<&Path> = by_file.keys().copied().collect();
        file_keys.sort();

        let mut round_robin = Vec::with_capacity(units.len());
        let max_len = by_file.values().map(|v| v.len()).max().unwrap_or(0);
        for round in 0..max_len {
            for file in &file_keys {
                if let Some(list) = by_file.get(file) {
                    if round < list.len() {
                        round_robin.push(list[round]);
                    }
                }
            }
        }
        order = round_robin;
    } else {
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
    }

    let mut used = 0usize;
    let mut included: Vec<PlannedUnit> = Vec::new();
    let mut excluded: Vec<usize> = Vec::new();
    let mut budget_exhausted_units = Vec::new();
    let mut cannibalization_warnings = Vec::new();

    let candidate_count = order
        .iter()
        .filter(|u| !has_positive_scores || scores.get(&u.id).copied().unwrap_or(0.0) > 0.0)
        .count();

    // Pass 1: Candidate admission and depth upgrade
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
                    // Cannibalization check: if multiple candidates exist, don't let 1 unit take >60% of budget leaving no room
                    let would_cannibalize = candidate_count > 1
                        && u.est_tokens_full > (budget_tokens * 60) / 100
                        && (used + marginal_full + (candidate_count.saturating_sub(1) * 15) > budget_tokens);

                    if !would_cannibalize && used + marginal_full <= budget_tokens {
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

                if current_tokens > (budget_tokens * 60) / 100 {
                    let pct = (current_tokens as f32 / budget_tokens as f32) * 100.0;
                    cannibalization_warnings.push(BudgetCannibalizationWarning {
                        unit_id: u.id,
                        unit_name: u.name.clone(),
                        tokens_used: current_tokens,
                        pct_of_budget: pct,
                    });
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
        cannibalization_warnings,
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
            call_sites: vec![],
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
        let units = vec![u2.clone(), u1.clone()];

        let scores = HashMap::new();
        let plan1 = select_within_budget(&units, &scores, 100);
        let plan2 = select_within_budget(&units, &scores, 100);

        assert_eq!(plan1.included[0].unit_id, plan2.included[0].unit_id);
    }
}