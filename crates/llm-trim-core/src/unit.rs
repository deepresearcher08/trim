use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnitKind {
    Function,
    Method,
    Struct,
    Class,
    Enum,
    Trait,
    Interface,
    Impl,
    TypeAlias,
    Const,
}

impl UnitKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            UnitKind::Function => "function",
            UnitKind::Method => "method",
            UnitKind::Struct => "struct",
            UnitKind::Class => "class",
            UnitKind::Enum => "enum",
            UnitKind::Trait => "trait",
            UnitKind::Interface => "interface",
            UnitKind::Impl => "impl",
            UnitKind::TypeAlias => "type",
            UnitKind::Const => "const",
        }
    }
}

/// A single top-level (or method-level) definition extracted from a source
/// file, carried through Tier 2 (ranking) and Tier 3 (budget selection).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeUnit {
    pub id: usize,
    pub file: PathBuf,
    pub kind: UnitKind,
    pub name: String,
    /// Doc comment / docstring immediately preceding the definition, if any.
    pub doc_comment: Option<String>,
    /// Signature line(s) only — used as the always-included anchor.
    pub signature: String,
    /// Full source text of the definition, verbatim.
    pub full_text: String,
    /// Signature + doc comment + an elision marker in place of the body.
    /// Always syntactically inert (never truncated mid-token) so it can be
    /// safely dropped into a prompt without misleading the model into
    /// treating elided code as absent (i.e. no hallucination risk from
    /// silently missing logic — the elision is explicit).
    pub skeleton_text: String,
    pub start_line: usize,
    pub end_line: usize,
    /// Cheap token estimate (chars / 4), used by the Tier 3 budget engine.
    pub est_tokens_full: usize,
    pub est_tokens_skeleton: usize,
}

pub fn estimate_tokens(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    // Heuristic consistent with OpenAI/Anthropic-style BPE tokenizers on
    // source code: ~4 chars/token, floor of 1 for any non-empty string.
    (s.len() as f32 / 4.0).ceil().max(1.0) as usize
}