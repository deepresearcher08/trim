//! End-to-end integration and verification test suite for all new features from new_features.md:
//! 1. Secret scanning & redaction
//! 2. Honest AST dependency graph with edge attribution
//! 3. Gitignore / .trimignore / binary skipping
//! 4. Score transparency & honest ranking
//! 5. Budget degradation sanity at high graph_weight
//! 6. Intent recall without intent (weak intent + round-robin coverage)
//! 7. Continuous agent session memory
//! 8. Behavioral Git signals
//! 9. Cache integrity checksumming & pre-write secret redaction

use llm_trim_core::{
    discover_source_files_with_stats, format_scan_report, is_binary_file,
    parse_codebase_cached, scan_and_redact_file, select_within_budget, CacheStore,
    CodeGraph, SessionStore,
};
use llm_trim_rank::{derive_weak_intent, HeuristicRanker, Ranker};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

struct TempTestDir {
    pub dir: PathBuf,
}

impl TempTestDir {
    pub fn new(prefix: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "llm_trim_new_features_{}_{}_{}",
            prefix,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }
}

impl Drop for TempTestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

// -----------------------------------------------------------------------------
// P0 - 1: Secret Scanning
// -----------------------------------------------------------------------------
#[test]
fn test_secret_scanning_default_and_planted_keys() {
    let raw_source = r#"
// Planted API keys and tokens that must be redacted
const GROQ_KEY = "gsk_1234567890abcdefghijklmnopqrstuvwxyz1234567890";
const GEMINI_KEY_1 = "AIzaSyD-1234567890abcdefghijklmnopqrstuv";
const GEMINI_KEY_2 = "AIzaSyA_abcdef1234567890ABCDEF1234567890";
const OPENAI_KEY = "sk-proj-1234567890abcdefghijklmnopqrstuvwxyzABCDEF";
const GITHUB_PAT = "ghp_1234567890abcdefghijklmnopqrstuvwx";
const AWS_ACCESS = "AKIA1234567890ABCDEF";
const AWS_SECRET = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
const PEM_HEADER = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA0...";
const CONFIG_KEY = "api_key = \"secret_config_token_123456789\"";
let password_val = "P@ssw0rd-SuperSecret-12345";

// Innocent code that must NOT trigger false positives
const buffer_size = 4096;
let secret = 'default';
let token = 0;
function getToken() { return "token"; }
let apiKeyFromName = "innocent_var_name";
"#;

    let (redacted, detections) = scan_and_redact_file(Some("src/credentials.rs"), raw_source);

    // 1. Zero raw matches in redacted text
    assert!(!redacted.contains("gsk_1234567890abcdefghijklmnopqrstuvwxyz1234567890"));
    assert!(!redacted.contains("AIzaSyD-1234567890abcdefghijklmnopqrstuv"));
    assert!(!redacted.contains("AIzaSyA_abcdef1234567890ABCDEF1234567890"));
    assert!(!redacted.contains("sk-proj-1234567890abcdefghijklmnopqrstuvwxyzABCDEF"));
    assert!(!redacted.contains("ghp_1234567890abcdefghijklmnopqrstuvwx"));
    assert!(!redacted.contains("AKIA1234567890ABCDEF"));
    assert!(!redacted.contains("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"));
    assert!(!redacted.contains("secret_config_token_123456789"));
    assert!(!redacted.contains("P@ssw0rd-SuperSecret-12345"));

    // 2. Redacted placeholders present
    assert!(redacted.contains("[REDACTED: Groq API Key]"));
    assert!(redacted.contains("[REDACTED: Google API Key]"));
    assert!(redacted.contains("[REDACTED: OpenAI / Anthropic API Key]"));
    assert!(redacted.contains("[REDACTED: GitHub Personal Access Token]"));
    assert!(redacted.contains("[REDACTED: AWS Access Key]"));

    // 3. Innocent code kept intact
    assert!(redacted.contains("const buffer_size = 4096;"));
    assert!(redacted.contains("secret = 'default'"));
    assert!(redacted.contains("token = 0"));
    assert!(redacted.contains("function getToken()"));

    // 4. Scan report formatting
    let report = format_scan_report(&detections);
    assert!(report.contains("src/credentials.rs:"));
    assert!(detections.len() >= 9);
}

// -----------------------------------------------------------------------------
// P0 - 2: Honest Dependency Call Graph
// -----------------------------------------------------------------------------
#[test]
fn test_honest_dependency_call_graph_and_edges() {
    let temp = TempTestDir::new("call_graph");

    let file_a = temp.dir.join("caller.py");
    fs::write(
        &file_a,
        r#"
from callee import target_worker

def execute_pipeline():
    # Calling target_worker at line 6
    result = target_worker(42)
    # Generic identifier mentions that should NOT create call edges to unrelated functions
    unrelated = "cuda.memory_allocated()"
    return result
"#,
    )
    .unwrap();

    let file_b = temp.dir.join("callee.py");
    fs::write(
        &file_b,
        r#"
def target_worker(val: int) -> int:
    return val * 2

def memory_allocated() -> int:
    # Unrelated function with coincident token name
    return 1024
"#,
    )
    .unwrap();

    let units = parse_codebase_cached(&temp.dir, None, false).unwrap();
    assert_eq!(units.len(), 3);

    let graph = CodeGraph::build(&units);

    let caller_unit = units.iter().find(|u| u.name == "execute_pipeline").unwrap();
    let target_unit = units.iter().find(|u| u.name == "target_worker").unwrap();
    let mem_unit = units.iter().find(|u| u.name == "memory_allocated").unwrap();

    // 1. Verify true AST call edge from execute_pipeline -> target_worker
    let target_incoming = graph.get_incoming_edges(target_unit.id);
    assert_eq!(target_incoming.len(), 1, "target_worker should have exactly 1 incoming edge from caller");
    assert_eq!(target_incoming[0].caller_id, caller_unit.id);
    assert_eq!(target_incoming[0].callee_name, "target_worker");
    assert_eq!(target_incoming[0].caller_line, 6, "Edge line number must match call site line 6");

    // 2. Verify token overlap did NOT create a bogus edge to memory_allocated
    let mem_incoming = graph.get_incoming_edges(mem_unit.id);
    assert_eq!(mem_incoming.len(), 0, "memory_allocated must NOT have incoming edges from token overlap");

    // 3. Verify dependency pulling
    let pulled = graph.pull_direct_dependencies(&[caller_unit.id]);
    assert!(pulled.contains(&target_unit.id));
    assert!(!pulled.contains(&mem_unit.id));
}

// -----------------------------------------------------------------------------
// P1 - 3: Gitignore, .trimignore, and Binary Skipping
// -----------------------------------------------------------------------------
#[test]
fn test_gitignore_trimignore_and_binary_skipping() {
    let temp = TempTestDir::new("ignore_binary");

    // Create source files
    let src = temp.dir.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("main.rs"), "fn main() {}\n").unwrap();

    // Create default ignored directory: node_modules
    let node_modules = temp.dir.join("node_modules").join("pkg");
    fs::create_dir_all(&node_modules).unwrap();
    fs::write(node_modules.join("index.js"), "module.exports = {};\n").unwrap();

    // Create .trimignore
    fs::write(temp.dir.join(".trimignore"), "custom_build/\n").unwrap();
    let custom_build = temp.dir.join("custom_build");
    fs::create_dir_all(&custom_build).unwrap();
    fs::write(custom_build.join("out.rs"), "fn out() {}\n").unwrap();

    // Create minified js
    fs::write(src.join("bundle.min.js"), "var a=1;var b=2;").unwrap();

    // Create binary file with GGUF header and null bytes
    let binary_path = src.join("model.rs"); // named .rs to test magic byte binary skipping
    let mut bin_data = vec![0x47, 0x47, 0x55, 0x46]; // GGUF header
    bin_data.extend(vec![0u8; 1000]);
    fs::write(&binary_path, &bin_data).unwrap();

    assert!(is_binary_file(&binary_path));

    let (discovered, stats) = discover_source_files_with_stats(&temp.dir, &[], true).unwrap();

    // Only src/main.rs should be admitted
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].file_name().unwrap(), "main.rs");

    // Verify stats
    assert!(stats.binary_files_count >= 1);
    assert!(stats.binary_bytes_skipped >= 1000);
}

// -----------------------------------------------------------------------------
// P1 - 4: Score Transparency & Honest Ranking
// -----------------------------------------------------------------------------
#[test]
fn test_score_transparency_and_honest_ranking() {
    let temp = TempTestDir::new("honest_scores");

    // Create a function and a class with equal centrality (0 calls) in python
    let file = temp.dir.join("entities.py");
    fs::write(
        &file,
        r#"
class ConfigRegistry:
    """Central configuration store and options registry."""
    def __init__(self):
        self.settings = {}

def config_registry():
    """Central configuration store and options registry."""
    return {}
"#,
    )
    .unwrap();

    let units = parse_codebase_cached(&temp.dir, None, false).unwrap();
    assert_eq!(units.len(), 3);

    let class_unit = units.iter().find(|u| u.name == "ConfigRegistry").unwrap();
    let func_unit = units.iter().find(|u| u.name == "config_registry").unwrap();

    let ranker = HeuristicRanker::new();
    let diagnostics = ranker.score_diagnostics("configuration store options registry", &units);
    let diag_map: HashMap<usize, _> = diagnostics.into_iter().map(|d| (d.unit_id, d)).collect();

    let class_diag = diag_map.get(&class_unit.id).unwrap();
    let func_diag = diag_map.get(&func_unit.id).unwrap();

    // Math check: exact sum
    let class_expected = (class_diag.name_score + class_diag.doc_score + class_diag.sig_score + class_diag.body_score) / (1.0 + (HeuristicRanker::tokenize(&class_unit.signature).len() as f32 / 50.0));
    assert!((class_diag.total_score - class_expected).abs() < 1e-4);

    // No hidden kind bonus: class and function with identical relevance & centrality produce near-equal scores
    assert!(
        (class_diag.total_score - func_diag.total_score).abs() < 2.0,
        "Class score ({}) and function score ({}) should be near-equal under identical intent matches",
        class_diag.total_score, func_diag.total_score
    );
}

// -----------------------------------------------------------------------------
// P1 - 5: Budget Degradation Sanity at High Graph Weight
// -----------------------------------------------------------------------------
#[test]
fn test_budget_degradation_sanity_high_graph_weight() {
    let temp = TempTestDir::new("budget_sanity");

    let file = temp.dir.join("large_system.py");
    fs::write(
        &file,
        r#"
def giant_central_core():
    # A large core function with 40 lines
    a = 1
    b = 2
    c = 3
    d = 4
    e = 5
    f = 6
    g = 7
    h = 8
    i = 9
    j = 10
    return a + b + c + d + e + f + g + h + i + j

def auth_service():
    """Validates user sessions and tokens."""
    return True

def db_service():
    """Manages database connection pool and transactions."""
    return True

def cache_service():
    """Handles in-memory key-value cache caching."""
    return True
"#,
    )
    .unwrap();

    let units = parse_codebase_cached(&temp.dir, None, false).unwrap();
    assert_eq!(units.len(), 4);

    let graph = CodeGraph::build(&units);
    let ranker = HeuristicRanker::new();
    let lexical = ranker.score("auth database cache", &units);

    // Run with high graph weight = 3.0 and moderate budget (200 tokens)
    let scores = graph.apply_centrality_boost(&lexical, &units, 3.0);
    let plan = select_within_budget(&units, &scores, 200);

    // High centrality must NOT cannibalize 100% of budget leaving only 1 unit!
    assert!(
        plan.included.len() >= 3,
        "Plan should include multiple units under budget 200, but only included {}: {:?}",
        plan.included.len(),
        plan.included
    );
}

// -----------------------------------------------------------------------------
// P1 - 6: Intent Recall Without Intent & Weak Intent Derivation
// -----------------------------------------------------------------------------
#[test]
fn test_intent_recall_without_intent_and_manifest_weak_intent() {
    let temp = TempTestDir::new("weak_intent");

    // Create Cargo.toml
    fs::write(
        temp.dir.join("Cargo.toml"),
        r#"
[package]
name = "hyper-auth"
description = "Fast OAuth2 and JWT authentication engine"
version = "0.1.0"
"#,
    )
    .unwrap();

    // Verify weak intent derived from Cargo.toml
    let weak_intent = derive_weak_intent(&temp.dir);
    assert!(weak_intent.is_some());
    let intent_str = weak_intent.unwrap();
    assert!(intent_str.contains("hyper-auth"));
    assert!(intent_str.contains("OAuth2 and JWT authentication"));

    // Create multi-module repo
    let mod_a = temp.dir.join("auth.rs");
    fs::write(&mod_a, "pub struct AuthConfig { pub port: u16 }\npub fn login() {}\n").unwrap();
    let mod_b = temp.dir.join("db.rs");
    fs::write(&mod_b, "pub struct DbPool { pub pool_size: usize }\npub fn query() {}\n").unwrap();

    let units = parse_codebase_cached(&temp.dir, None, false).unwrap();
    assert_eq!(units.len(), 4);

    // In empty intent mode, round-robin coverage selects across both files
    let empty_scores = HashMap::new();
    let plan = select_within_budget(&units, &empty_scores, 80);

    let included_files: std::collections::HashSet<_> = plan
        .included
        .iter()
        .map(|p| units.iter().find(|u| u.id == p.unit_id).unwrap().file.clone())
        .collect();

    assert_eq!(included_files.len(), 2, "Round-robin explore mode must include units from both files");
}

// -----------------------------------------------------------------------------
// P2 - 7 & 8: Session Hot Set Memory
// -----------------------------------------------------------------------------
#[test]
fn test_session_hot_set_agent_memory() {
    let temp = TempTestDir::new("session_memory");

    let file = temp.dir.join("service.rs");
    fs::write(&file, "pub fn active_handler() {}\npub fn background_task() {}\n").unwrap();

    let units = parse_codebase_cached(&temp.dir, None, false).unwrap();
    let active_unit = units.iter().find(|u| u.name == "active_handler").unwrap();

    let mut session = SessionStore::new("test-session-123");
    let plan = select_within_budget(&units, &HashMap::new(), 100);
    session.record_plan(&units, &plan);
    session.save(&temp.dir).unwrap();

    let loaded = SessionStore::load_or_create(&temp.dir, "test-session-123");
    let mut scores: HashMap<usize, f32> = units.iter().map(|u| (u.id, 1.0)).collect();
    loaded.apply_session_boost(&units, &mut scores, 2.0);

    assert!(scores[&active_unit.id] > 1.0, "Active handler in session hot set should be boosted");
}

// -----------------------------------------------------------------------------
// P2 - 9 & 10: Cache Integrity Checksumming & Pre-Write Secret Redaction
// -----------------------------------------------------------------------------
#[test]
fn test_cache_integrity_checksum_and_pre_redaction() {
    let temp = TempTestDir::new("cache_trust");

    let secret_file = temp.dir.join("keys.rs");
    fs::write(
        &secret_file,
        "pub fn token() -> &'static str {\n    const SECRET_KEY = \"gsk_1234567890abcdefghijklmnopqrstuvwxyz1234567890\";\n    SECRET_KEY\n}\n",
    )
    .unwrap();

    let cache_file = temp.dir.join(".trim_cache");
    let units = parse_codebase_cached(&temp.dir, Some(&cache_file), true).unwrap();
    assert_eq!(units.len(), 1);

    // Verify .trim_cache on disk contains no raw Groq keys
    let cache_content = fs::read_to_string(&cache_file).unwrap();
    assert!(
        !cache_content.contains("gsk_1234567890abcdefghijklmnopqrstuvwxyz1234567890"),
        "Raw secrets must never be written to .trim_cache"
    );
    assert!(cache_content.contains("[REDACTED: Groq API Key]"));

    // Verify checksum exists
    let store = CacheStore::load(&cache_file).unwrap();
    assert!(!store.checksum.is_empty());

    // Corrupt the cache file and test self-healing
    fs::write(&cache_file, "{ corrupted json payload ...").unwrap();
    let healed_units = parse_codebase_cached(&temp.dir, Some(&cache_file), true).unwrap();
    assert_eq!(healed_units.len(), 1, "Cache corruption must self-heal cleanly without failing");
}
