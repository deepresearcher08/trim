use crate::unit::CodeUnit;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Inclusion {
    Full,
    Skeleton,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedUnit {
    pub unit_id: usize,
    pub inclusion: Inclusion,
    pub tokens: usize,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetPlan {
    pub budget_tokens: usize,
    pub used_tokens: usize,
    pub included: Vec<PlannedUnit>,
    pub excluded_unit_ids: Vec<usize>,
}

/// Greedy two-pass budget allocation:
///
/// Pass 1 — Admit structural skeletons for candidate code units in order
/// of relevance score until the target budget is exhausted. This maximizes
/// breadth of structural coverage (giving visibility to all relevant
/// symbol signatures) prior to spending tokens on implementation details.
///
/// Pass 2 — Walk candidate units in descending relevance order, upgrading
/// skeletonized signatures to full implementations where the marginal token
/// cost (full_tokens - skeleton_tokens) fits within remaining budget.
///
/// Code units maintain structural boundary integrity (either full skeleton
/// or full implementation), ensuring explicit elision rather than arbitrary
/// text truncation.
pub fn select_within_budget(
    units: &[CodeUnit],
    scores: &HashMap<usize, f32>,
    budget_tokens: usize,
) -> BudgetPlan {
    let mut order: Vec<&CodeUnit> = units.iter().collect();
    order.sort_by(|a, b| {
        let sa = scores.get(&a.id).copied().unwrap_or(0.0);
        let sb = scores.get(&b.id).copied().unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut used = 0usize;
    let mut included: Vec<PlannedUnit> = Vec::new();
    let mut excluded: Vec<usize> = Vec::new();

    // Pass 1: skeletons.
    for u in &order {
        let score = scores.get(&u.id).copied().unwrap_or(0.0);
        if used + u.est_tokens_skeleton <= budget_tokens {
            used += u.est_tokens_skeleton;
            included.push(PlannedUnit {
                unit_id: u.id,
                inclusion: Inclusion::Skeleton,
                tokens: u.est_tokens_skeleton,
                score,
            });
        } else {
            excluded.push(u.id);
        }
    }

    // Pass 2: upgrade to full text where the marginal cost fits.
    let included_by_id: HashMap<usize, usize> = included
        .iter()
        .enumerate()
        .map(|(idx, p)| (p.unit_id, idx))
        .collect();

    for u in &order {
        let Some(&idx) = included_by_id.get(&u.id) else {
            continue;
        };
        if u.est_tokens_full <= u.est_tokens_skeleton {
            continue; // nothing to gain (e.g. const/static units)
        }
        let marginal = u.est_tokens_full - u.est_tokens_skeleton;
        if used + marginal <= budget_tokens {
            used += marginal;
            included[idx].inclusion = Inclusion::Full;
            included[idx].tokens = u.est_tokens_full;
        }
    }

    BudgetPlan {
        budget_tokens,
        used_tokens: used,
        included,
        excluded_unit_ids: excluded,
    }
}

/// Render a BudgetPlan back into the final prompt payload text, grouped by
/// file, in original source order within each file.
pub fn render_payload(units: &[CodeUnit], plan: &BudgetPlan) -> String {
    let mut plan_by_id: HashMap<usize, &PlannedUnit> = HashMap::new();
    for p in &plan.included {
        plan_by_id.insert(p.unit_id, p);
    }

    let mut files: Vec<&std::path::PathBuf> = units.iter().map(|u| &u.file).collect();
    files.sort();
    files.dedup();

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

        out.push_str(&format!("// === {} ===\n", file.display()));
        for u in file_units {
            let planned = plan_by_id[&u.id];
            let text = match planned.inclusion {
                Inclusion::Full => &u.full_text,
                Inclusion::Skeleton => &u.skeleton_text,
            };
            out.push_str(text);
            out.push_str("\n\n");
        }
    }
    out
}