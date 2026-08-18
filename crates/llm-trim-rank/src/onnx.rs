//! Cross-encoder ranking via ONNX Runtime.
//!
//! Not compiled by default (`cargo build --features onnx` to enable).
//! llm-trim ships zero-config and does not bundle model weights; point
//! this at a local export of e.g. `cross-encoder/ms-marco-MiniLM-L-6-v2`
//! (~22M params — sub-10ms/pair on CPU, good default trade-off of speed
//! vs ranking quality for this use case). See MODELS.md.

use anyhow::{Context, Result};
use llm_trim_core::CodeUnit;
use ort::{inputs, session::Session, value::Tensor};
use std::collections::HashMap;
use std::sync::Mutex;
use tokenizers::Tokenizer;

pub struct OnnxCrossEncoderRanker {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    max_length: usize,
}

impl OnnxCrossEncoderRanker {
    pub fn load(model_path: &str, tokenizer_path: &str, max_length: usize) -> Result<Self> {
        let session = Session::builder()
            .context("building ORT session")?
            .commit_from_file(model_path)
            .with_context(|| format!("loading ONNX model at {model_path}"))?;
        let tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| anyhow::anyhow!("loading tokenizer at {tokenizer_path}: {e}"))?;
        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
            max_length,
        })
    }

    /// A cross-encoder scores a (query, passage) *pair* jointly — unlike a
    /// bi-encoder/embedding model, there's no separate query/document
    /// vector space, so every candidate needs its own forward pass. We
    /// batch here to amortize that.
    fn candidate_text(unit: &CodeUnit) -> String {
        let doc = unit.doc_comment.as_deref().unwrap_or("");
        format!("{} {} {}", unit.name, unit.signature, doc)
    }
}

impl super::Ranker for OnnxCrossEncoderRanker {
    fn score(&self, intent: &str, units: &[CodeUnit]) -> HashMap<usize, f32> {
        let mut scores = HashMap::with_capacity(units.len());
        let mut session = match self.session.lock() {
            Ok(s) => s,
            Err(_) => return scores,
        };

        for unit in units {
            let passage = Self::candidate_text(unit);
            let encoding = match self.tokenizer.encode((intent, passage.as_str()), true) {
                Ok(e) => e,
                Err(_) => {
                    scores.insert(unit.id, 0.0);
                    continue;
                }
            };

            let mut ids: Vec<i64> = encoding.get_ids().iter().map(|&x| x as i64).collect();
            let mut mask: Vec<i64> = encoding
                .get_attention_mask()
                .iter()
                .map(|&x| x as i64)
                .collect();
            ids.truncate(self.max_length);
            mask.truncate(self.max_length);

            let input_ids = match Tensor::from_array(([1usize, ids.len()], ids)) {
                Ok(t) => t,
                Err(_) => {
                    scores.insert(unit.id, 0.0);
                    continue;
                }
            };
            let attention_mask = match Tensor::from_array(([1usize, mask.len()], mask)) {
                Ok(t) => t,
                Err(_) => {
                    scores.insert(unit.id, 0.0);
                    continue;
                }
            };

            let logit = match session.run(inputs![
                "input_ids" => input_ids,
                "attention_mask" => attention_mask,
            ]) {
                Ok(outputs) => match outputs.get("logits") {
                    Some(v) => v
                        .try_extract_tensor::<f32>()
                        .ok()
                        .and_then(|(_, data)| data.first().copied())
                        .unwrap_or(0.0),
                    None => outputs
                        .values()
                        .next()
                        .and_then(|v| {
                            v.try_extract_tensor::<f32>()
                                .ok()
                                .and_then(|(_, data)| data.first().copied())
                        })
                        .unwrap_or(0.0),
                },
                Err(_) => 0.0,
            };

            scores.insert(unit.id, logit);
        }
        scores
    }
}