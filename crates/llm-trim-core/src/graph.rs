//! Cross-file AST-resolved dependency graph and PageRank centrality engine.
//!
//! Resolves AST call expressions, method invocations, and type instantiations
//! to concrete definitions across files. Computes PageRank centrality to boost
//! foundational symbols and provides caller/callee edge attribution for `--why` and `--deps`.

use crate::unit::CodeUnit;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEdge {
    pub caller_id: usize,
    pub callee_id: usize,
    pub caller_file: PathBuf,
    pub caller_line: usize,
    pub callee_name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodeGraph {
    /// unit_id -> list of unit_ids that this unit depends on (callees / types used)
    pub dependencies: HashMap<usize, Vec<usize>>,
    /// unit_id -> list of unit_ids that depend on this unit (callers / consumers)
    pub dependents: HashMap<usize, Vec<usize>>,
    /// unit_id -> PageRank centrality score in [0.0, 1.0]
    pub centrality: HashMap<usize, f32>,
    /// All resolved call graph edges
    pub edges: Vec<GraphEdge>,
    /// unit_id -> incoming edges (callers calling this unit)
    pub incoming_edges: HashMap<usize, Vec<GraphEdge>>,
    /// unit_id -> outgoing edges (calls made by this unit)
    pub outgoing_edges: HashMap<usize, Vec<GraphEdge>>,
}

impl CodeGraph {
    /// Build a true call graph across all extracted code units.
    pub fn build(units: &[CodeUnit]) -> Self {
        // Map symbol name -> candidate unit definitions
        let mut name_to_units: HashMap<&str, Vec<&CodeUnit>> = HashMap::new();
        for u in units {
            if !u.name.is_empty() && u.name != "<anonymous>" {
                name_to_units.entry(&u.name).or_default().push(u);
            }
        }

        let mut dependencies: HashMap<usize, Vec<usize>> = HashMap::with_capacity(units.len());
        let mut dependents: HashMap<usize, Vec<usize>> = HashMap::with_capacity(units.len());
        let mut incoming_edges: HashMap<usize, Vec<GraphEdge>> = HashMap::with_capacity(units.len());
        let mut outgoing_edges: HashMap<usize, Vec<GraphEdge>> = HashMap::with_capacity(units.len());
        let mut all_edges = Vec::new();

        for u in units {
            dependencies.entry(u.id).or_default();
            dependents.entry(u.id).or_default();
            incoming_edges.entry(u.id).or_default();
            outgoing_edges.entry(u.id).or_default();
        }

        for caller in units {
            let mut resolved_callees = HashSet::new();

            // 1. Process structured AST call sites if available
            if !caller.call_sites.is_empty() {
                for call in &caller.call_sites {
                    if let Some(candidates) = name_to_units.get(call.callee_name.as_str()) {
                        let mut matched_target: Option<&CodeUnit> = None;

                        if let Some(qualifier) = &call.module_qualifier {
                            for cand in candidates {
                                let cand_file_stem = cand.file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                                if cand_file_stem.eq_ignore_ascii_case(qualifier) {
                                    matched_target = Some(cand);
                                    break;
                                }
                            }
                        }

                        if matched_target.is_none() {
                            for cand in candidates {
                                if cand.file == caller.file && cand.id != caller.id {
                                    matched_target = Some(cand);
                                    break;
                                }
                            }
                        }

                        let target_units: Vec<&CodeUnit> = if let Some(target) = matched_target {
                            vec![target]
                        } else {
                            candidates.iter().copied().filter(|c| c.id != caller.id).collect()
                        };

                        for target in target_units {
                            if target.id != caller.id && resolved_callees.insert((target.id, call.line)) {
                                let edge = GraphEdge {
                                    caller_id: caller.id,
                                    callee_id: target.id,
                                    caller_file: caller.file.clone(),
                                    caller_line: call.line,
                                    callee_name: call.callee_name.clone(),
                                };
                                dependencies.entry(caller.id).or_default().push(target.id);
                                dependents.entry(target.id).or_default().push(caller.id);
                                outgoing_edges.entry(caller.id).or_default().push(edge.clone());
                                incoming_edges.entry(target.id).or_default().push(edge.clone());
                                all_edges.push(edge);
                            }
                        }
                    }
                }
            } else {
                // Fallback for units without AST call_sites (e.g. synthetic or test units)
                for ref_name in &caller.references {
                    if let Some(candidates) = name_to_units.get(ref_name.as_str()) {
                        for target in candidates {
                            if target.id != caller.id && resolved_callees.insert((target.id, caller.start_line)) {
                                let edge = GraphEdge {
                                    caller_id: caller.id,
                                    callee_id: target.id,
                                    caller_file: caller.file.clone(),
                                    caller_line: caller.start_line,
                                    callee_name: ref_name.clone(),
                                };
                                dependencies.entry(caller.id).or_default().push(target.id);
                                dependents.entry(target.id).or_default().push(caller.id);
                                outgoing_edges.entry(caller.id).or_default().push(edge.clone());
                                incoming_edges.entry(target.id).or_default().push(edge.clone());
                                all_edges.push(edge);
                            }
                        }
                    }
                }
            }
        }

        // Deduplicate neighbor lists
        for list in dependencies.values_mut() {
            list.sort_unstable();
            list.dedup();
        }
        for list in dependents.values_mut() {
            list.sort_unstable();
            list.dedup();
        }

        let centrality = Self::compute_pagerank(units, &dependents, &dependencies);

        Self {
            dependencies,
            dependents,
            centrality,
            edges: all_edges,
            incoming_edges,
            outgoing_edges,
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

    /// Combine lexical BM25 relevance scores with PageRank centrality with sanity capping.
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

            // Centered, transparent centrality scoring with saturating cap
            let cent_boost = (weight * cent * 2.0).min(4.0);

            let score = if has_lexical {
                lex + cent_boost
            } else {
                1.0 + cent_boost
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

    /// Retrieve incoming call graph edges for a given callee unit ID.
    pub fn get_incoming_edges(&self, unit_id: usize) -> &[GraphEdge] {
        self.incoming_edges
            .get(&unit_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Retrieve outgoing call graph edges for a given caller unit ID.
    pub fn get_outgoing_edges(&self, unit_id: usize) -> &[GraphEdge] {
        self.outgoing_edges
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
    use crate::unit::{CallSite, UnitKind};
    use std::path::PathBuf;

    fn make_unit_with_calls(id: usize, name: &str, file: &str, calls: Vec<(&str, usize)>) -> CodeUnit {
        let call_sites: Vec<CallSite> = calls
            .into_iter()
            .map(|(callee, line)| CallSite {
                callee_name: callee.to_string(),
                module_qualifier: None,
                line,
            })
            .collect();
        let refs: Vec<String> = call_sites.iter().map(|c| c.callee_name.clone()).collect();

        CodeUnit {
            id,
            file: PathBuf::from(file),
            kind: UnitKind::Function,
            name: name.to_string(),
            doc_comment: None,
            signature: format!("fn {name}()"),
            full_text: format!("fn {name}() {{}}"),
            compact_text: format!("fn {name}() {{}}"),
            skeleton_text: format!("fn {name}() {{}}"),
            start_line: 1,
            end_line: 10,
            est_tokens_full: 20,
            est_tokens_compact: 15,
            est_tokens_skeleton: 5,
            references: refs,
            call_sites,
        }
    }

    #[test]
    fn test_code_graph_true_edges() {
        let u0 = make_unit_with_calls(0, "core_util", "src/util.rs", vec![]);
        let u1 = make_unit_with_calls(1, "service_a", "src/service_a.rs", vec![("core_util", 42)]);
        let u2 = make_unit_with_calls(2, "service_b", "src/service_b.rs", vec![("core_util", 15)]);
        let units = vec![u0, u1, u2];

        let graph = CodeGraph::build(&units);
        assert_eq!(graph.get_dependents(0).len(), 2);
        assert_eq!(graph.get_dependencies(1), &[0]);

        let incoming_to_0 = graph.get_incoming_edges(0);
        assert_eq!(incoming_to_0.len(), 2);
        assert_eq!(incoming_to_0[0].caller_line, 42);
        assert_eq!(incoming_to_0[0].callee_name, "core_util");

        let c0 = graph.centrality.get(&0).copied().unwrap_or(0.0);
        let c1 = graph.centrality.get(&1).copied().unwrap_or(0.0);
        assert!(c0 > c1, "core_util centrality ({c0}) should exceed service_a ({c1})");

        let pulled = graph.pull_direct_dependencies(&[1]);
        assert!(pulled.contains(&0));
    }
}
