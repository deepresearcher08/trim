//! llm-trim-rank — Tier 2: intent-based semantic ranking.
//!
//! Default ranker: `HeuristicRanker`, an advanced dependency-free lexical
//! ranker with compound identifier splitting (camelCase/snake_case), rule-based
//! stemming, synonym/intent concept expansion, docstring-first weighting,
//! and prefix/root matching.
//!
//! Opt-in `onnx` feature: `OnnxCrossEncoderRanker`, using ONNX Runtime
//! (`ort`) bindings for neural cross-encoder re-ranking. See `MODELS.md`.

pub mod git;

use llm_trim_core::CodeUnit;
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[cfg(feature = "onnx")]
pub mod onnx;

pub use git::GitSignals;

/// Detailed score diagnostics for explain mode (`--why`).
#[derive(Debug, Clone, Default)]
pub struct ScoreDiagnostic {
    pub unit_id: usize,
    pub total_score: f32,
    pub lexical_score: f32,
    pub centrality_score: f32,
    pub dep_boost: f32,
    pub git_boost: f32,
    pub structural_score: f32,
    pub name_score: f32,
    pub doc_score: f32,
    pub sig_score: f32,
    pub body_score: f32,
    pub matched_terms: Vec<String>,
    pub why_explanation: Option<String>,
}

/// Common interface for relevance scoring implementations.
pub trait Ranker {
    /// Score extracted code units against a query intent string. Higher scores
    /// indicate greater relevance. Returned map is keyed by `CodeUnit::id`.
    fn score(&self, intent: &str, units: &[CodeUnit]) -> HashMap<usize, f32>;
}

/// Derive a weak intent string from repository context (manifests, README).
pub fn derive_weak_intent(root: &Path) -> Option<String> {
    // 1. Cargo.toml
    let cargo_path = root.join("Cargo.toml");
    if cargo_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&cargo_path) {
            let mut name = None;
            let mut desc = None;
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("name =") && name.is_none() {
                    name = trimmed.split('=').nth(1).map(|s| s.trim().trim_matches('"').to_string());
                } else if trimmed.starts_with("description =") && desc.is_none() {
                    desc = trimmed.split('=').nth(1).map(|s| s.trim().trim_matches('"').to_string());
                }
            }
            if let Some(n) = name {
                return Some(format!("{} {}", n, desc.unwrap_or_default()).trim().to_string());
            }
        }
    }

    // 2. package.json
    let pkg_path = root.join("package.json");
    if pkg_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&pkg_path) {
            let mut name = None;
            let mut desc = None;
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("\"name\":") && name.is_none() {
                    name = trimmed.split(':').nth(1).map(|s| s.trim().trim_matches(|c| c == '"' || c == ',' || c == ' ').to_string());
                } else if trimmed.starts_with("\"description\":") && desc.is_none() {
                    desc = trimmed.split(':').nth(1).map(|s| s.trim().trim_matches(|c| c == '"' || c == ',' || c == ' ').to_string());
                }
            }
            if let Some(n) = name {
                return Some(format!("{} {}", n, desc.unwrap_or_default()).trim().to_string());
            }
        }
    }

    // 3. pyproject.toml
    let pyproj_path = root.join("pyproject.toml");
    if pyproj_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&pyproj_path) {
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("name =") {
                    if let Some(n) = trimmed.split('=').nth(1).map(|s| s.trim().trim_matches('"').to_string()) {
                        return Some(n);
                    }
                }
            }
        }
    }

    // 4. go.mod
    let gomod_path = root.join("go.mod");
    if gomod_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&gomod_path) {
            if let Some(first) = content.lines().next() {
                if first.starts_with("module ") {
                    return Some(first["module ".len()..].trim().to_string());
                }
            }
        }
    }

    // 5. README.md
    for name in &["README.md", "readme.md", "README"] {
        let readme_path = root.join(name);
        if readme_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&readme_path) {
                for line in content.lines() {
                    let trimmed = line.trim().trim_start_matches('#').trim();
                    if !trimmed.is_empty() && trimmed.len() <= 120 {
                        return Some(trimmed.to_string());
                    }
                }
            }
        }
    }

    root.file_name().and_then(|n| n.to_str()).map(|s| s.to_string())
}

/// Advanced dependency-free lexical ranker featuring compound splitting,
/// stemming, synonym expansion, prefix-matching, and docstring weighting.
pub struct HeuristicRanker;

impl HeuristicRanker {
    pub fn new() -> Self {
        Self
    }

    /// Split compound identifiers (camelCase, snake_case, PascalCase, kebab-case)
    /// into individual lowercase constituent terms.
    pub fn split_identifier(s: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut cur = String::new();
        let chars: Vec<char> = s.chars().collect();

        for i in 0..chars.len() {
            let c = chars[i];
            if c == '_' || c == '-' || c == '.' || c == ':' || c == '/' || c == '\\' || !c.is_alphanumeric() {
                if !cur.is_empty() {
                    tokens.push(cur.to_lowercase());
                    cur.clear();
                }
                continue;
            }

            if c.is_uppercase() {
                let prev_is_lower = i > 0 && chars[i - 1].is_lowercase();
                let next_is_lower = i + 1 < chars.len() && chars[i + 1].is_lowercase();

                if (prev_is_lower || (i > 0 && next_is_lower && chars[i - 1].is_uppercase()))
                    && !cur.is_empty()
                {
                    tokens.push(cur.to_lowercase());
                    cur.clear();
                }
            }

            cur.push(c);
        }

        if !cur.is_empty() {
            tokens.push(cur.to_lowercase());
        }

        tokens
    }

    /// Suffix-stripping and normalization rule-based stemmer.
    pub fn stem(term: &str) -> String {
        let t = term.to_lowercase();
        if t.len() <= 3 {
            return t;
        }

        if let Some(stripped) = t.strip_suffix("tions") {
            return format!("{stripped}t");
        }
        if let Some(stripped) = t.strip_suffix("tion") {
            return format!("{stripped}t");
        }
        if let Some(stripped) = t.strip_suffix("ments") {
            return stripped.to_string();
        }
        if let Some(stripped) = t.strip_suffix("ment") {
            return stripped.to_string();
        }
        if let Some(stripped) = t.strip_suffix("ities") {
            return stripped.to_string();
        }
        if let Some(stripped) = t.strip_suffix("ity") {
            return stripped.to_string();
        }
        if let Some(stripped) = t.strip_suffix("ings") {
            return stripped.to_string();
        }
        if let Some(stripped) = t.strip_suffix("ing") {
            if stripped.len() >= 3 {
                return stripped.to_string();
            }
        }
        if let Some(stripped) = t.strip_suffix("ies") {
            return format!("{stripped}y");
        }
        if let Some(stripped) = t.strip_suffix("ers") {
            if stripped.len() >= 3 {
                return stripped.to_string();
            }
        }
        if let Some(stripped) = t.strip_suffix("er") {
            if stripped.len() >= 3 {
                return stripped.to_string();
            }
        }
        if let Some(stripped) = t.strip_suffix("ors") {
            if stripped.len() >= 3 {
                return stripped.to_string();
            }
        }
        if let Some(stripped) = t.strip_suffix("or") {
            if stripped.len() >= 3 {
                return stripped.to_string();
            }
        }
        if let Some(stripped) = t.strip_suffix("izes") {
            return stripped.to_string();
        }
        if let Some(stripped) = t.strip_suffix("ize") {
            return stripped.to_string();
        }
        if let Some(stripped) = t.strip_suffix("ated") {
            return format!("{stripped}at");
        }
        if let Some(stripped) = t.strip_suffix("ate") {
            if stripped.len() >= 4 {
                return format!("{stripped}at");
            }
        }
        if let Some(stripped) = t.strip_suffix("ed") {
            if stripped.len() >= 3 {
                return stripped.to_string();
            }
        }
        if let Some(stripped) = t.strip_suffix("es") {
            if stripped.len() >= 3 {
                return stripped.to_string();
            }
        }
        if let Some(stripped) = t.strip_suffix('s') {
            if stripped.len() >= 3 && !stripped.ends_with('s') {
                return stripped.to_string();
            }
        }

        t
    }

    /// Tokenize text into stemmed, deduplicated word terms and compound identifier components.
    pub fn tokenize(s: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        for raw in s.split_whitespace() {
            for sub in Self::split_identifier(raw) {
                let clean: String = sub.chars().filter(|c| c.is_alphanumeric()).collect();
                if clean.len() >= 2 {
                    let stemmed = Self::stem(&clean);
                    tokens.push(clean.to_lowercase());
                    if stemmed != clean && stemmed.len() >= 2 {
                        tokens.push(stemmed);
                    }
                }
            }
        }
        tokens.sort();
        tokens.dedup();
        tokens
    }

    /// Match a single query term against a target token list, returning match weight multiplier.
    fn match_term(term: &str, target_terms: &[String]) -> f32 {
        if target_terms.iter().any(|t| t == term) {
            return 1.0;
        }

        // Check root / prefix similarity for terms >= 4 chars
        if term.len() >= 4 {
            for target in target_terms {
                if target.len() >= 4 {
                    if target.starts_with(term) || term.starts_with(target) {
                        return 0.85;
                    }
                }
            }
        }

        0.0
    }

    /// Expand intent terms with synonymous keywords and related conceptual identifiers.
    pub fn expand_intent(intent: &str) -> Vec<(String, f32)> {
        let base_tokens = Self::tokenize(intent);
        let mut expanded: HashMap<String, f32> = HashMap::new();

        for t in &base_tokens {
            expanded.insert(t.clone(), 1.0);
        }

        // Domain concept mappings: (term -> list of synonyms, relative weight)
        let synonyms: &[(&[&str], &[&str])] = &[
            (
                &["leak", "leaking", "leakage"],
                &["dispose", "free", "close", "drop", "cleanup", "release", "drain", "flush", "destroy", "dealloc"],
            ),
            (
                &["auth", "authenticate", "authenticat", "authorization", "authoriz"],
                &["jwt", "token", "session", "login", "logout", "credential", "permission", "oauth", "bearer", "user"],
            ),
            (
                &["conn", "connection", "connect"],
                &["pool", "socket", "client", "session", "handshake", "channel", "stream", "transport"],
            ),
            (
                &["db", "database", "sql"],
                &["query", "table", "pool", "store", "storage", "repo", "persist", "entity", "migration", "tx"],
            ),
            (
                &["cache", "caching"],
                &["memoize", "lru", "store", "lookup", "hit", "miss", "evict", "ttl", "entry", "expire"],
            ),
            (
                &["config", "configure", "configuration"],
                &["setting", "option", "env", "param", "preference", "toml", "yaml", "json", "flag"],
            ),
            (
                &["fix", "bug", "patch"],
                &["repair", "resolve", "correct", "handle", "error", "panic", "fault", "recover"],
            ),
            (
                &["find", "search"],
                &["lookup", "query", "get", "locate", "discover", "fetch", "scan", "index"],
            ),
            (
                &["init", "initialize", "setup"],
                &["create", "construct", "new", "build", "start", "instantiate", "load", "open"],
            ),
            (
                &["budget", "limit"],
                &["cost", "quota", "token", "capacity", "trim", "allocate", "threshold", "ceiling"],
            ),
            (
                &["parse", "parsing"],
                &["ast", "syntax", "grammar", "tree", "token", "lex", "extract", "traverse", "visitor"],
            ),
            (
                &["rank", "ranking"],
                &["score", "relevance", "weight", "priority", "sort", "order", "heuristic", "bm25"],
            ),
            (
                &["dedupe", "deduplicate", "duplicate", "twice", "repeat", "idempotent", "throttle", "debounce"],
                &["dedupe", "unique", "distinct", "filter", "throttle", "debounce", "cache", "singleton", "idempotent", "once", "replay", "request"],
            ),
        ];

        for &(triggers, syns) in synonyms {
            let matched = base_tokens.iter().any(|t| {
                let st = Self::stem(t);
                triggers.iter().any(|&tr| tr == t || Self::stem(tr) == st || tr.starts_with(&st) || st.starts_with(tr))
            });

            if matched {
                for &syn in syns {
                    let stemmed_syn = Self::stem(syn);
                    expanded.entry(syn.to_string()).or_insert(0.75);
                    if stemmed_syn != syn {
                        expanded.entry(stemmed_syn).or_insert(0.75);
                    }
                }
            }
        }

        expanded.into_iter().collect()
    }

    /// Score units with detailed diagnostics for `--why`.
    pub fn score_diagnostics(&self, intent: &str, units: &[CodeUnit]) -> Vec<ScoreDiagnostic> {
        let intent_terms = Self::expand_intent(intent);
        if intent_terms.is_empty() {
            return units
                .iter()
                .map(|u| ScoreDiagnostic {
                    unit_id: u.id,
                    total_score: 0.0,
                    lexical_score: 0.0,
                    centrality_score: 0.0,
                    dep_boost: 0.0,
                    git_boost: 0.0,
                    structural_score: 0.0,
                    name_score: 0.0,
                    doc_score: 0.0,
                    sig_score: 0.0,
                    body_score: 0.0,
                    matched_terms: vec![],
                    why_explanation: None,
                })
                .collect();
        }

        let mut diagnostics = Vec::with_capacity(units.len());

        for u in units {
            let name_terms = Self::tokenize(&u.name);
            let doc_terms = Self::tokenize(u.doc_comment.as_deref().unwrap_or(""));
            let sig_terms = Self::tokenize(&u.signature);
            let body_terms = Self::tokenize(&u.full_text);

            let mut name_score = 0.0f32;
            let mut doc_score = 0.0f32;
            let mut sig_score = 0.0f32;
            let mut body_score = 0.0f32;
            let mut matched_terms = HashSet::new();

            for (term, term_weight) in &intent_terms {
                let name_match = Self::match_term(term, &name_terms);
                if name_match > 0.0 {
                    name_score += 4.0 * term_weight * name_match;
                    matched_terms.insert(term.clone());
                }

                let doc_match = Self::match_term(term, &doc_terms);
                if doc_match > 0.0 {
                    doc_score += 3.5 * term_weight * doc_match;
                    matched_terms.insert(term.clone());
                }

                let sig_match = Self::match_term(term, &sig_terms);
                if sig_match > 0.0 {
                    sig_score += 1.5 * term_weight * sig_match;
                    matched_terms.insert(term.clone());
                }

                if name_match == 0.0 && doc_match == 0.0 && sig_match == 0.0 {
                    let body_match = Self::match_term(term, &body_terms);
                    if body_match > 0.0 {
                        body_score += 0.8 * term_weight * body_match;
                        matched_terms.insert(term.clone());
                    }
                }
            }

            let len_penalty = 1.0 + (sig_terms.len() as f32 / 50.0);
            let lexical_total = (name_score + doc_score + sig_score + body_score) / len_penalty;

            let mut matched_list: Vec<String> = matched_terms.into_iter().collect();
            matched_list.sort();

            diagnostics.push(ScoreDiagnostic {
                unit_id: u.id,
                total_score: lexical_total,
                lexical_score: lexical_total,
                centrality_score: 0.0,
                dep_boost: 0.0,
                git_boost: 0.0,
                structural_score: 0.0,
                name_score,
                doc_score,
                sig_score,
                body_score,
                matched_terms: matched_list,
                why_explanation: None,
            });
        }

        diagnostics
    }
}

impl Default for HeuristicRanker {
    fn default() -> Self {
        Self::new()
    }
}

impl Ranker for HeuristicRanker {
    fn score(&self, intent: &str, units: &[CodeUnit]) -> HashMap<usize, f32> {
        let diagnostics = self.score_diagnostics(intent, units);
        diagnostics
            .into_iter()
            .map(|d| (d.unit_id, d.total_score))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_trim_core::unit::UnitKind;
    use std::path::PathBuf;

    fn make_unit(
        id: usize,
        name: &str,
        doc: Option<&str>,
        sig: &str,
        full: &str,
    ) -> CodeUnit {
        CodeUnit {
            id,
            file: PathBuf::from("src/lib.rs"),
            kind: UnitKind::Function,
            name: name.to_string(),
            doc_comment: doc.map(|s| s.to_string()),
            signature: sig.to_string(),
            full_text: full.to_string(),
            compact_text: full.to_string(),
            skeleton_text: sig.to_string(),
            start_line: 1,
            end_line: 10,
            est_tokens_full: 30,
            est_tokens_compact: 20,
            est_tokens_skeleton: 10,
            references: vec![],
            call_sites: vec![],
        }
    }

    #[test]
    fn test_synonym_and_stem_expansion() {
        let ranker = HeuristicRanker::new();

        // Unit 1: dispose_handle (no explicit word "leak" in name/doc, but "dispose")
        let u1 = make_unit(
            1,
            "dispose_handle",
            Some("Release resource connection back to pool"),
            "pub fn dispose_handle(h: Handle)",
            "pub fn dispose_handle(h: Handle) { h.close(); }",
        );

        // Unit 2: irrelevant_func
        let u2 = make_unit(
            2,
            "calculate_tax",
            None,
            "pub fn calculate_tax(amount: f64)",
            "pub fn calculate_tax(amount: f64) { amount * 0.15; }",
        );

        let units = vec![u1, u2];
        let scores = ranker.score("fix connection leak", &units);

        let score1 = scores.get(&1).copied().unwrap_or(0.0);
        let score2 = scores.get(&2).copied().unwrap_or(0.0);

        assert!(
            score1 > score2 && score1 > 0.0,
            "u1 score ({score1}) should beat u2 score ({score2}) via synonym matching"
        );
    }

    #[test]
    fn test_docstring_priority_weighting() {
        let ranker = HeuristicRanker::new();

        // helper function whose docstring explicitly states "validates JWT signatures"
        let u1 = make_unit(
            1,
            "helper",
            Some("Validates incoming JWT signatures against public keys"),
            "fn helper(token: &str) -> bool",
            "fn helper(token: &str) -> bool { true }",
        );

        // arbitrary helper function without docstring
        let u2 = make_unit(
            2,
            "helper",
            Some("Miscellaneous string formatting utilities"),
            "fn helper(val: &str) -> String",
            "fn helper(val: &str) -> String { val.to_string() }",
        );

        let units = vec![u1, u2];
        let scores = ranker.score("validates JWT signatures", &units);

        let score1 = scores.get(&1).copied().unwrap_or(0.0);
        let score2 = scores.get(&2).copied().unwrap_or(0.0);

        assert!(
            score1 > score2,
            "docstring match on u1 ({score1}) should beat u2 ({score2})"
        );
    }

    #[test]
    fn test_adversarial_queries() {
        let ranker = HeuristicRanker::new();

        let u1 = make_unit(
            1,
            "authenticate_user_session",
            Some("Log in user and issue credential tokens"),
            "pub fn authenticate_user_session(req: Request)",
            "pub fn authenticate_user_session(req: Request) {}",
        );

        let scores = ranker.score("users authentications logining", &[u1]);
        assert!(scores.get(&1).copied().unwrap_or(0.0) > 0.0);
    }
}