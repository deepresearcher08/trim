//! llm-trim-rank — Tier 2: intent-based semantic ranking.
//!
//! Default ranker: `HeuristicRanker`, a dependency-free lexical ranker
//! evaluating token occurrences across unit identifiers, signatures, and docstrings.
//!
//! Opt-in `onnx` feature: `OnnxCrossEncoderRanker`, using ONNX Runtime
//! (`ort`) bindings for neural cross-encoder re-ranking. Model weights
//! are loaded from local files at runtime. See `MODELS.md`.

use llm_trim_core::CodeUnit;
use std::collections::HashMap;

#[cfg(feature = "onnx")]
pub mod onnx;

/// Common interface for relevance scoring implementations.
pub trait Ranker {
    /// Score extracted code units against a query intent string. Higher scores
    /// indicate greater relevance. Returned map is keyed by `CodeUnit::id`.
    fn score(&self, intent: &str, units: &[CodeUnit]) -> HashMap<usize, f32>;
}

/// Lightweight, dependency-free lexical ranker using BM25-lite scoring
/// over unit names, signatures, and docstrings.
pub struct HeuristicRanker;

impl HeuristicRanker {
    pub fn new() -> Self {
        Self
    }

    fn tokenize(s: &str) -> Vec<String> {
        s.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.len() > 1)
            .map(|t| t.to_string())
            .collect()
    }
}

impl Default for HeuristicRanker {
    fn default() -> Self {
        Self::new()
    }
}

impl Ranker for HeuristicRanker {
    fn score(&self, intent: &str, units: &[CodeUnit]) -> HashMap<usize, f32> {
        let intent_terms = Self::tokenize(intent);
        if intent_terms.is_empty() {
            // No intent given: every unit is equally relevant, let the
            // budget engine fall back to source order.
            return units.iter().map(|u| (u.id, 0.0)).collect();
        }

        let mut scores = HashMap::with_capacity(units.len());
        for u in units {
            let name_terms = Self::tokenize(&u.name);
            let doc_terms = Self::tokenize(u.doc_comment.as_deref().unwrap_or(""));
            let sig_terms = Self::tokenize(&u.signature);

            let mut score = 0.0f32;
            for term in &intent_terms {
                if name_terms.iter().any(|t| t == term) {
                    score += 3.0;
                }
                score += doc_terms.iter().filter(|t| *t == term).count() as f32 * 1.5;
                score += sig_terms.iter().filter(|t| *t == term).count() as f32 * 1.0;
            }
            // Mild normalization so terse, on-point signatures aren't
            // outweighed by long ones that happen to repeat a term.
            let len_penalty = 1.0 + (sig_terms.len() as f32 / 40.0);
            scores.insert(u.id, score / len_penalty);
        }
        scores
    }
}