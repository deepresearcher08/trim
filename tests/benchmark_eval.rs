//! Benchmark evaluation suite for trim.
//!
//! Tests multi-language codebase parsing (Rust, Python, TypeScript, Go),
//! downstream task recall, compression ratios, graceful degradation, and
//! cross-file dependency pulling across varying token budgets.

use llm_trim_core::{
    parse_codebase_cached, render_payload, select_within_budget, CodeGraph, Inclusion,
};
use llm_trim_rank::{HeuristicRanker, Ranker};
use std::fs;
use std::path::PathBuf;

struct TestBenchmarkRepo {
    pub dir: PathBuf,
}

impl TestBenchmarkRepo {
    pub fn setup() -> Self {
        let dir = std::env::temp_dir().join(format!("llm_trim_benchmark_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // 1. Rust module: Connection pool & resource leak
        let src_rs = dir.join("src");
        fs::create_dir_all(&src_rs).unwrap();
        fs::write(
            src_rs.join("pool.rs"),
            r#"
pub struct ConnectionPool {
    pub max_connections: usize,
    pub active: usize,
}

/// Release idle socket connections back to the pool to prevent descriptor leaks.
pub fn dispose_idle_handle(pool: &mut ConnectionPool, handle_id: u64) {
    if pool.active > 0 {
        pool.active -= 1;
    }
}

pub fn acquire_connection(pool: &mut ConnectionPool) -> Option<u64> {
    if pool.active < pool.max_connections {
        pool.active += 1;
        Some(pool.active as u64)
    } else {
        None
    }
}
"#,
        )
        .unwrap();

        // 2. Python module: Token budget & rate limiting
        let py_dir = dir.join("python_pkg");
        fs::create_dir_all(&py_dir).unwrap();
        fs::write(
            py_dir.join("budget.py"),
            r#"
class BudgetEngine:
    """Three-pass greedy token allocation engine for prompt context minimization."""

    def __init__(self, max_tokens: int):
        self.max_tokens = max_tokens
        self.used_tokens = 0

    def allocate_units(self, candidate_units: list) -> dict:
        """Allocate structural units within budget limits, degrading gracefully."""
        admitted = []
        for unit in candidate_units:
            if self.used_tokens + unit['skeleton_tokens'] <= self.max_tokens:
                self.used_tokens += unit['skeleton_tokens']
                admitted.append(unit)
        return {'units': admitted, 'used': self.used_tokens}
"#,
        )
        .unwrap();

        // 3. TypeScript module: JWT Authentication & Signature Validation
        let ts_dir = dir.join("auth_service");
        fs::create_dir_all(&ts_dir).unwrap();
        fs::write(
            ts_dir.join("jwtValidator.ts"),
            r#"
export interface AuthToken {
    header: string;
    payload: string;
    signature: string;
}

/**
 * Validates incoming JWT signatures against trusted public keys.
 */
export function validateJwtSignature(token: AuthToken, publicKey: string): boolean {
    if (!token.signature || token.signature.length < 10) {
        return false;
    }
    return true;
}

export function extractBearerToken(authHeader: string): string | null {
    if (authHeader.startsWith("Bearer ")) {
        return authHeader.substring(7);
    }
    return null;
}
"#,
        )
        .unwrap();

        // 4. Go module: Database transaction & query dispatcher
        let go_dir = dir.join("db_layer");
        fs::create_dir_all(&go_dir).unwrap();
        fs::write(
            go_dir.join("store.go"),
            r#"
package dblayer

type Store struct {
    ConnectionString string
}

// ExecuteQuery runs a parameterized SQL query against the database cluster.
func ExecuteQuery(store *Store, query string, args ...interface{}) (int, error) {
    if store == nil {
        return 0, nil
    }
    return 1, nil
}
"#,
        )
        .unwrap();

        Self { dir }
    }
}

impl Drop for TestBenchmarkRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn test_benchmark_downstream_task_recall_and_compression() {
    let repo = TestBenchmarkRepo::setup();
    let units = parse_codebase_cached(&repo.dir, None, false).unwrap();
    assert!(units.len() >= 7, "Expected at least 7 units, got {}", units.len());

    let raw_tokens: usize = units.iter().map(|u| u.est_tokens_full).sum();
    let graph = CodeGraph::build(&units);
    let ranker = HeuristicRanker::new();

    // Downstream Task 1: Bug localization ("fix connection leak")
    {
        let lexical = ranker.score("fix connection leak in pool", &units);
        let scores = graph.apply_centrality_boost(&lexical, &units, 0.4);

        // Run with conservative budget (150 tokens)
        let plan = select_within_budget(&units, &scores, 150);
        let payload = render_payload(&units, &plan);

        // Target critical unit `dispose_idle_handle` must be included
        let dispose_unit = units.iter().find(|u| u.name == "dispose_idle_handle").unwrap();
        let is_included = plan.included.iter().any(|p| p.unit_id == dispose_unit.id);
        assert!(is_included, "dispose_idle_handle should be included in Task 1");

        // Verify token budget constraint was respected
        assert!(plan.used_tokens <= 150);
        assert!(plan.used_tokens < raw_tokens);
        assert!(payload.contains("dispose_idle_handle"));
    }

    // Downstream Task 2: Feature explanation ("three-pass budget token allocation")
    {
        let lexical = ranker.score("budget token allocation engine", &units);
        let scores = graph.apply_centrality_boost(&lexical, &units, 0.4);

        let plan = select_within_budget(&units, &scores, 200);
        let budget_unit = units.iter().find(|u| u.name == "BudgetEngine").unwrap();
        let is_included = plan.included.iter().any(|p| p.unit_id == budget_unit.id);
        assert!(is_included, "BudgetEngine should be prioritized for Task 2");
    }

    // Downstream Task 3: Security & auth ("validates JWT signatures")
    {
        let lexical = ranker.score("validates JWT signatures", &units);
        let scores = graph.apply_centrality_boost(&lexical, &units, 0.4);

        let plan = select_within_budget(&units, &scores, 200);
        let jwt_unit = units.iter().find(|u| u.name == "validateJwtSignature").unwrap();
        let is_included = plan.included.iter().any(|p| p.unit_id == jwt_unit.id);
        assert!(is_included, "validateJwtSignature should be prioritized for Task 3");

        let planned = plan.included.iter().find(|p| p.unit_id == jwt_unit.id).unwrap();
        assert!(
            matches!(planned.inclusion, Inclusion::Full | Inclusion::Compact),
            "Top task unit should be Full or Compact"
        );
    }
}

#[test]
fn test_benchmark_graceful_degradation_no_hard_cliff() {
    let repo = TestBenchmarkRepo::setup();
    let units = parse_codebase_cached(&repo.dir, None, false).unwrap();

    let ranker = HeuristicRanker::new();
    let lexical = ranker.score("JWT token signature validation", &units);

    // Test a tight budget where Full would exceed budget, but Compact fits!
    let plan = select_within_budget(&units, &lexical, 100);
    assert!(plan.used_tokens <= 100);

    let has_compact_or_full = plan
        .included
        .iter()
        .any(|p| matches!(p.inclusion, Inclusion::Compact | Inclusion::Full));
    assert!(has_compact_or_full, "Graceful degradation should admit Compact/Full without falling to bare signatures for all units");
}
