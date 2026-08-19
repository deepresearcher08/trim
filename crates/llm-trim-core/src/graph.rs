//! Cross-file dependency graph and graph-centrality engine.
//!
//! Builds a lightweight in-memory directed reference graph from AST symbol
//! definitions and extracted references with zero external dependencies.
//! Computes PageRank and degree centrality to boost foundational modules
//! and provides caller/callee traversal.

use crate::unit::CodeUnit;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Default)]
pub struct CodeGraph {
    /// unit_id -> list of unit_ids that this unit depends on (callees / types used)
    pub dependencies: HashMap<usize, Vec<usize>>,
    /// unit_id -> list of unit_ids that depend on this unit (callers / consumers)
    pub dependents: HashMap<usize, Vec<usize>>,
    /// unit_id -> PageRank centrality score in [0.0, 1.0]
    pub centrality: HashMap<usize, f32>,
}

impl CodeGraph {
    /// Build a dependency graph across all extracted code units.
    pub fn build(units: &[CodeUnit]) -> Self {
        // Map symbol name -> unit IDs that define it
        let mut name_to_ids: HashMap<&str, Vec<usize>> = HashMap::new();
        for u in units {
            if !u.name.is_empty() && u.name != "<anonymous>" {
                name_to_ids.entry(&u.name).or_default().push(u.id);
            }
        }

        let mut dependencies: HashMap<usize, Vec<usize>> = HashMap::with_capacity(units.len());
        let mut dependents: HashMap<usize, Vec<usize>> = HashMap::with_capacity(units.len());

        for u in units {
            dependencies.entry(u.id).or_default();
            dependents.entry(u.id).or_default();
        }

        for u in units {
            let mut target_ids = HashSet::new();
            for ref_name in &u.references {
                if let Some(target_list) = name_to_ids.get(ref_name.as_str()) {
                    for &target_id in target_list {
                        if target_id != u.id {
                            target_ids.insert(target_id);
                        }
                    }
                }
            }

            for target_id in target_ids {
                dependencies.entry(u.id).or_default().push(target_id);
                dependents.entry(target_id).or_default().push(u.id);
            }
        }

        let centrality = Self::compute_pagerank(units, &dependents, &dependencies);

        Self {
            dependencies,
            dependents,
            centrality,
        }
    }

    /// Compute PageRank centrality scores using power iteration.
    fn compute_pagerank(
        units: &[CodeUnit],
        dependents: &HashMap<usize, Vec<usize>>,
        dependencies: &HashMap<usize, Vec<usize>>,
    ) -> HashMap<usize, f32> {
        let n = units.len();
        if n == 0 {
            return HashMap::new();
        }

        let init_val = 1.0 / (n as f32);
        let mut ranks: HashMap<usize, f32> = units.iter().map(|u| (u.id, init_val)).collect();
        let damping = 0.85f32;
        let iterations = 20;

        for _ in 0..iterations {
            let mut next_ranks: HashMap<usize, f32> = HashMap::with_capacity(n);
            let base_rank = (1.0 - damping) / (n as f32);

            for u in units {
                let mut incoming_sum = 0.0f32;
                // Units that depend on u (callers) vote for u
                if let Some(callers) = dependents.get(&u.id) {
                    for &caller_id in callers {
                        let caller_out_degree = dependencies
                            .get(&caller_id)
                            .map(|deps| deps.len())
                            .unwrap_or(0);
                        if caller_out_degree > 0 {
                            let caller_rank = ranks.get(&caller_id).copied().unwrap_or(0.0);
                            incoming_sum += caller_rank / (caller_out_degree as f32);
                        }
                    }
                }
                next_ranks.insert(u.id, base_rank + damping * incoming_sum);
            }
            ranks = next_ranks;
        }

        // Normalize max rank to 1.0 for scale consistency
        let max_rank = ranks.values().copied().fold(0.0f32, f32::max).max(1e-6);
        ranks
            .into_iter()
            .map(|(id, score)| (id, (score / max_rank).min(1.0)))
            .collect()
    }

    /// Combine lexical BM25 relevance scores with PageRank centrality.
    ///
    /// If weight <= 0.0, graph boost is bypassed and pure lexical scores are returned.
    /// If an intent query is active (lexical scores > 0):
    /// `boosted = lexical * (1.0 + weight * centrality) + (2.0 * weight * centrality)`
    ///
    /// If no intent is given (lexical scores == 0):
    /// `boosted = 1.0 + weight * centrality`
    pub fn apply_centrality_boost(
        &self,
        lexical_scores: &HashMap<usize, f32>,
        units: &[CodeUnit],
        weight: f32,
    ) -> HashMap<usize, f32> {
        if weight <= 0.0 {
            return lexical_scores.clone();
        }

        let has_lexical = lexical_scores.values().any(|&s| s > 0.0);
        let mut combined = HashMap::with_capacity(units.len());

        for u in units {
            let lex = lexical_scores.get(&u.id).copied().unwrap_or(0.0);
            let cent = self.centrality.get(&u.id).copied().unwrap_or(0.0);

            let score = if has_lexical {
                lex * (1.0 + weight * cent) + (2.0 * weight * cent)
            } else {
                1.0 + weight * cent
            };

            combined.insert(u.id, score);
        }

        combined
    }

    /// Retrieve direct dependencies (callees) for a given unit ID.
    pub fn get_dependencies(&self, unit_id: usize) -> &[usize] {
        self.dependencies
            .get(&unit_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Retrieve direct callers/dependents for a given unit ID.
    pub fn get_dependents(&self, unit_id: usize) -> &[usize] {
        self.dependents
            .get(&unit_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Given unit IDs selected for Full inclusion, find all direct dependencies (callees)
    /// that should be pulled into context to avoid orphaned fragments.
    pub fn pull_direct_dependencies(&self, full_unit_ids: &[usize]) -> HashSet<usize> {
        let mut direct_deps = HashSet::new();
        for &id in full_unit_ids {
            if let Some(deps) = self.dependencies.get(&id) {
                for &dep_id in deps {
                    direct_deps.insert(dep_id);
                }
            }
        }
        direct_deps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::UnitKind;
    use std::path::PathBuf;

    fn make_unit_with_refs(id: usize, name: &str, refs: Vec<&str>) -> CodeUnit {
        CodeUnit {
            id,
            file: PathBuf::from(format!("src/{name}.rs")),
            kind: UnitKind::Function,
            name: name.to_string(),
            doc_comment: None,
            signature: format!("fn {name}()"),
            full_text: format!("fn {name}() {{}}"),
            compact_text: format!("fn {name}() {{}}"),
            skeleton_text: format!("fn {name}() {{}}"),
            start_line: 1,
            end_line: 5,
            est_tokens_full: 20,
            est_tokens_compact: 15,
            est_tokens_skeleton: 5,
            references: refs.into_iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn test_code_graph_centrality() {
        // u0 (core_util) is referenced by u1, u2, u3
        let u0 = make_unit_with_refs(0, "core_util", vec![]);
        let u1 = make_unit_with_refs(1, "service_a", vec!["core_util"]);
        let u2 = make_unit_with_refs(2, "service_b", vec!["core_util"]);
        let u3 = make_unit_with_refs(3, "service_c", vec!["core_util"]);
        let units = vec![u0, u1, u2, u3];

        let graph = CodeGraph::build(&units);
        assert_eq!(graph.get_dependents(0).len(), 3);
        assert_eq!(graph.get_dependencies(1), &[0]);

        // core_util has the highest centrality
        let c0 = graph.centrality.get(&0).copied().unwrap_or(0.0);
        let c1 = graph.centrality.get(&1).copied().unwrap_or(0.0);
        assert!(c0 > c1, "core_util centrality ({c0}) should exceed service_a ({c1})");

        let pulled = graph.pull_direct_dependencies(&[1]);
        assert!(pulled.contains(&0));
    }
}
