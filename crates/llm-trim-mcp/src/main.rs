// trim-mcp: MCP server exposing trim's codebase context minimization
// as callable tools for LLM agents (Claude Code, Cursor, Cline, etc.).
//
// Communicates over stdio using the official rmcp SDK.

use rmcp::{
    handler::server::wrapper::Parameters,
    schemars,
    tool, tool_handler, tool_router,
    transport::stdio,
    ServerHandler, ServiceExt,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use llm_trim_core::{
    cache::parse_codebase_cached_with_options, extract_units, format_scan_report,
    render_payload, scan_and_redact, select_within_budget, CodeGraph, Inclusion, Language,
    SessionStore, TrimConfig,
};
use llm_trim_rank::{derive_weak_intent, GitSignals, HeuristicRanker, Ranker, ScoreDiagnostic};

// -- Tool parameter schemas --------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[allow(dead_code)]
struct TrimParams {
    /// Absolute or relative path to the repository root directory to scan.
    path: String,

    /// Natural language description of what the caller is trying to
    /// understand or accomplish. Drives relevance ranking so that the
    /// returned payload prioritizes structurally relevant code units.
    /// Leave empty to auto-derive from repository context or explore broadly.
    #[schemars(default)]
    intent: Option<String>,

    /// Maximum number of tokens for the output payload. Defaults to 8000
    /// when not specified.
    #[schemars(default)]
    budget: Option<usize>,

    /// Automatically pull direct caller/callee dependencies of full units
    /// so the payload contains connected dependency chains.
    #[schemars(default)]
    deps: Option<bool>,

    /// Scan and redact credentials/secrets (defaults to true).
    #[schemars(default)]
    scan_secrets: Option<bool>,

    /// Disable PageRank graph centrality scoring.
    #[schemars(default)]
    no_graph: Option<bool>,

    /// PageRank centrality boost weight multiplier (defaults to 0.5).
    #[schemars(default)]
    graph_weight: Option<f32>,

    /// When true, skip reading or writing the .trim_cache file and
    /// re-parse every source file from scratch.
    #[schemars(default)]
    no_cache: Option<bool>,

    /// Custom cache file path.
    #[schemars(default)]
    cache_file: Option<String>,

    /// Additional glob patterns to ignore.
    #[schemars(default)]
    ignore: Option<Vec<String>>,

    /// Explicit path to a `trim.config.toml` or `trim.toml` file.
    #[schemars(default)]
    config: Option<String>,

    /// Include why explain diagnostics in summary header.
    #[schemars(default)]
    why: Option<bool>,

    /// Include verbose stats and degradation analysis.
    #[schemars(default)]
    stats: Option<bool>,

    /// Enable behavioral Git recency signals.
    #[schemars(default)]
    git_signals: Option<bool>,

    /// Continuous agent memory session ID.
    #[schemars(default)]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct TrimPlanParams {
    /// Absolute or relative path to the repository root directory to scan.
    path: String,

    /// Natural-language task description for the agent loop.
    task: String,

    /// Token budget for the plan. Defaults to 8000.
    #[schemars(default)]
    budget: Option<usize>,

    /// Continuous agent session ID to maintain active context.
    #[schemars(default)]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct TrimFileParams {
    /// Absolute or relative path to a single source file to parse.
    path: String,
}

// -- Server implementation ---------------------------------------------------

#[derive(Clone)]
struct TrimServer;

#[tool_router]
impl TrimServer {
    /// Scan a codebase directory, rank code units by intent relevance,
    /// and return a budget-optimized context payload suitable for LLM
    /// prompts. Supports 12 major languages with secret redaction enabled by default.
    #[tool(
        name = "trim",
        description = "Scan a codebase directory and return a budget-optimized, structurally aware context payload for LLM prompts. Extracts top-level definitions (functions, structs, classes, traits, etc.) via Tree-Sitter AST parsing, ranks them by intent relevance and graph centrality, and assembles the highest-value code units across 3 inclusion tiers (Full, Compact, Skeleton) within a token budget."
    )]
    async fn trim(&self, Parameters(params): Parameters<TrimParams>) -> String {
        let root = PathBuf::from(&params.path);
        if !root.exists() {
            return format!("Error: path '{}' does not exist.", params.path);
        }
        if !root.is_dir() {
            return format!("Error: path '{}' is not a directory. Use trim_file for single files.", params.path);
        }

        // Config discovery
        let config_opt = match &params.config {
            Some(cfg_path) => TrimConfig::load(&PathBuf::from(cfg_path)).ok().map(|c| (c, PathBuf::from(cfg_path))),
            None => TrimConfig::discover_and_load(&root),
        };

        let mut custom_ignores = params.ignore.clone().unwrap_or_default();
        let mut budget = params.budget.unwrap_or(8000);
        let mut intent = params.intent.clone().unwrap_or_default();
        let mut pull_deps = params.deps.unwrap_or(false);
        let mut scan_secrets_enabled = params.scan_secrets.unwrap_or(true);
        let mut no_graph = params.no_graph.unwrap_or(false);
        let mut graph_weight = params.graph_weight.unwrap_or(0.5);
        let mut cache_file_opt = params.cache_file.map(PathBuf::from);
        let mut git_signals_enabled = params.git_signals.unwrap_or(false);
        let mut session_id_opt = params.session_id.clone();

        if let Some((cfg, _)) = config_opt {
            if intent.is_empty() {
                if let Some(i) = cfg.intent {
                    intent = i;
                }
            }
            if budget == 8000 {
                if let Some(b) = cfg.budget {
                    budget = b;
                }
            }
            if !pull_deps && cfg.deps.unwrap_or(false) {
                pull_deps = true;
            }
            if params.scan_secrets.is_none() && cfg.scan_secrets.is_some() {
                scan_secrets_enabled = cfg.scan_secrets.unwrap();
            }
            if !no_graph && cfg.no_graph.unwrap_or(false) {
                no_graph = true;
            }
            if let Some(gw) = cfg.graph_weight {
                graph_weight = gw;
            }
            if cache_file_opt.is_none() {
                if let Some(cf) = cfg.cache_file {
                    cache_file_opt = Some(PathBuf::from(cf));
                }
            }
            if !git_signals_enabled && cfg.git_signals.unwrap_or(false) {
                git_signals_enabled = true;
            }
            if session_id_opt.is_none() {
                session_id_opt = cfg.session_id;
            }
            if let Some(cfg_ignores) = cfg.ignore {
                custom_ignores.extend(cfg_ignores);
            }
        }

        if intent.trim().is_empty() {
            if let Some(derived) = derive_weak_intent(&root) {
                intent = derived;
            }
        }

        let cache_enabled = !params.no_cache.unwrap_or(false);
        let (units, skipped_stats) = match parse_codebase_cached_with_options(
            &root,
            cache_file_opt.as_deref(),
            cache_enabled,
            &custom_ignores,
            true,
        ) {
            Ok(res) => res,
            Err(e) => return format!("Error scanning codebase: {e}"),
        };

        if units.is_empty() {
            return format!("No supported source files found under '{}'.", params.path);
        }

        let graph = CodeGraph::build(&units);
        let ranker = HeuristicRanker::new();
        let lexical_scores = ranker.score(&intent, &units);
        let effective_weight = if no_graph { 0.0 } else { graph_weight };
        let mut scores = graph.apply_centrality_boost(&lexical_scores, &units, effective_weight);

        if git_signals_enabled {
            let git_signals = GitSignals::from_repo(&root);
            git_signals.apply_git_boost(&root, &units, &mut scores, 1.0);
        }

        let session_store = session_id_opt
            .as_ref()
            .map(|s| SessionStore::load_or_create(&root, s));

        if let Some(store) = &session_store {
            store.apply_session_boost(&units, &mut scores, 1.5);
        }

        let mut plan = select_within_budget(&units, &scores, budget);

        if pull_deps {
            let full_ids: Vec<usize> = plan
                .included
                .iter()
                .filter(|p| matches!(p.inclusion, Inclusion::Full))
                .map(|p| p.unit_id)
                .collect();
            let direct_deps = graph.pull_direct_dependencies(&full_ids);
            if !direct_deps.is_empty() {
                for &dep_id in &direct_deps {
                    if let Some(s) = scores.get_mut(&dep_id) {
                        *s += 2.5;
                    }
                }
                plan = select_within_budget(&units, &scores, budget);
            }
        }

        let mut payload = render_payload(&units, &plan);

        let mut detections_count = 0;
        let mut scan_report_str = String::new();
        if scan_secrets_enabled {
            let (redacted, detections) = scan_and_redact(&payload);
            payload = redacted;
            detections_count = detections.len();
            if !detections.is_empty() {
                scan_report_str = format!("\n\n{}", format_scan_report(&detections));
            }
        }

        if let Some(mut store) = session_store {
            store.record_plan(&units, &plan);
            let _ = store.save(&root);
        }

        let full_count = plan.included.iter()
            .filter(|p| matches!(p.inclusion, Inclusion::Full))
            .count();
        let compact_count = plan.included.iter()
            .filter(|p| matches!(p.inclusion, Inclusion::Compact))
            .count();
        let skel_count = plan.included.len() - full_count - compact_count;

        let total_raw_tokens: usize = units.iter().map(|u| u.est_tokens_full).sum();
        let compression_pct = if total_raw_tokens > 0 {
            (1.0 - (plan.used_tokens as f64 / total_raw_tokens as f64)) * 100.0
        } else {
            0.0
        };

        let mut header = format!(
            "# trim summary: {} units found, {} included ({} full, {} compact, {} skeleton), {} excluded, {}/{} tokens used ({:.1}% compression from {} raw tokens)\n# Security: Secret scanning active ({} redacted), {} ignored files skipped, {} binary files skipped",
            units.len(),
            plan.included.len(),
            full_count,
            compact_count,
            skel_count,
            plan.excluded_unit_ids.len(),
            plan.used_tokens,
            plan.budget_tokens,
            compression_pct,
            total_raw_tokens,
            detections_count,
            skipped_stats.ignored_files_count,
            skipped_stats.binary_files_count,
        );

        if !plan.cannibalization_warnings.is_empty() {
            for warn in &plan.cannibalization_warnings {
                header.push_str(&format!(
                    "\n# Warning: Unit '{}' consumes {} tokens ({:.1}% of budget)",
                    warn.unit_name, warn.tokens_used, warn.pct_of_budget
                ));
            }
        }

        format!("{}\n{}\n\n{}", header, scan_report_str, payload)
    }

    /// Natural-language task context planner for agent loops.
    /// Analyzes a prompt/task, performs intent ranking and call graph analysis,
    /// and returns structured metadata with pre-selected context.
    #[tool(
        name = "trim_plan",
        description = "Natural-language task context planner for agent loops. Accepts a natural-language task description, extracts and ranks relevant structural code units, and returns structured metadata (selected units, file ranges, why chosen) along with the minimized context payload."
    )]
    async fn trim_plan(&self, Parameters(params): Parameters<TrimPlanParams>) -> String {
        let root = PathBuf::from(&params.path);
        if !root.exists() || !root.is_dir() {
            return format!("Error: '{}' is not a valid directory.", params.path);
        }

        let budget = params.budget.unwrap_or(8000);
        let (units, _) = match parse_codebase_cached_with_options(&root, None, true, &[], true) {
            Ok(res) => res,
            Err(e) => return format!("Error scanning codebase: {e}"),
        };

        if units.is_empty() {
            return format!("No supported source files found under '{}'.", params.path);
        }

        let graph = CodeGraph::build(&units);
        let ranker = HeuristicRanker::new();
        let lexical_scores = ranker.score(&params.task, &units);
        let diagnostics = ranker.score_diagnostics(&params.task, &units);
        let diag_by_id: HashMap<usize, ScoreDiagnostic> = diagnostics.into_iter().map(|d| (d.unit_id, d)).collect();

        let mut scores = graph.apply_centrality_boost(&lexical_scores, &units, 0.5);

        let session_store = params.session_id.as_ref().map(|s| SessionStore::load_or_create(&root, s));
        if let Some(store) = &session_store {
            store.apply_session_boost(&units, &mut scores, 1.5);
        }

        let mut plan = select_within_budget(&units, &scores, budget);

        // Pull deps
        let full_ids: Vec<usize> = plan
            .included
            .iter()
            .filter(|p| matches!(p.inclusion, Inclusion::Full))
            .map(|p| p.unit_id)
            .collect();
        let direct_deps = graph.pull_direct_dependencies(&full_ids);
        if !direct_deps.is_empty() {
            for &dep_id in &direct_deps {
                if let Some(s) = scores.get_mut(&dep_id) {
                    *s += 2.5;
                }
            }
            plan = select_within_budget(&units, &scores, budget);
        }

        let (payload, _) = scan_and_redact(&render_payload(&units, &plan));

        if let Some(mut store) = session_store {
            store.record_plan(&units, &plan);
            let _ = store.save(&root);
        }

        #[derive(Serialize)]
        struct UnitPlanInfo {
            name: String,
            kind: String,
            file: String,
            lines: String,
            inclusion: String,
            score: f32,
            matched_terms: Vec<String>,
        }

        #[derive(Serialize)]
        struct PlanResponse {
            task: String,
            budget_tokens: usize,
            used_tokens: usize,
            included_units: Vec<UnitPlanInfo>,
            context_payload: String,
        }

        let unit_by_id: HashMap<usize, &llm_trim_core::CodeUnit> = units.iter().map(|u| (u.id, u)).collect();
        let mut included_units = Vec::new();

        for p in &plan.included {
            if let Some(u) = unit_by_id.get(&p.unit_id) {
                let diag = diag_by_id.get(&p.unit_id);
                included_units.push(UnitPlanInfo {
                    name: u.name.clone(),
                    kind: u.kind.as_str().to_string(),
                    file: u.file.display().to_string().replace('\\', "/"),
                    lines: format!("{}-{}", u.start_line, u.end_line),
                    inclusion: match p.inclusion {
                        Inclusion::Full => "Full".to_string(),
                        Inclusion::Compact => "Compact".to_string(),
                        Inclusion::Skeleton => "Skeleton".to_string(),
                    },
                    score: p.score,
                    matched_terms: diag.map(|d| d.matched_terms.clone()).unwrap_or_default(),
                });
            }
        }

        let resp = PlanResponse {
            task: params.task,
            budget_tokens: plan.budget_tokens,
            used_tokens: plan.used_tokens,
            included_units,
            context_payload: payload,
        };

        serde_json::to_string_pretty(&resp).unwrap_or_else(|e| format!("Serialization error: {e}"))
    }

    /// Parse a single source file and return its extracted structural
    /// units (functions, structs, classes, etc.) with signatures,
    /// token estimates, and line ranges.
    #[tool(
        name = "trim_file",
        description = "Parse a single source file using Tree-Sitter and return its structural code units (functions, structs, classes, traits, methods, enums, etc.) with their names, signatures, line ranges, and token estimates. Useful for inspecting what trim would extract from a specific file."
    )]
    async fn trim_file(&self, Parameters(params): Parameters<TrimFileParams>) -> String {
        let path = PathBuf::from(&params.path);
        if !path.exists() {
            return format!("Error: file '{}' does not exist.", params.path);
        }
        if !path.is_file() {
            return format!("Error: '{}' is not a file. Use trim for directories.", params.path);
        }

        let lang = match Language::from_path(&path) {
            Some(l) => l,
            None => return format!(
                "Error: unsupported file extension for '{}'. Supported: .rs, .py, .pyi, .js, .jsx, .mjs, .cjs, .ts, .mts, .cts, .tsx, .go, .c, .h, .cpp, .cc, .cxx, .hpp, .java, .cs, .rb, .php",
                params.path
            ),
        };

        let source = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => return format!("Error reading file: {e}"),
        };

        let mut next_id = 0;
        let units = match extract_units(&path, lang, &source, &mut next_id) {
            Ok(u) => u,
            Err(e) => return format!("Error parsing file: {e}"),
        };

        if units.is_empty() {
            return format!("No extractable definitions found in '{}'.", params.path);
        }

        #[derive(Serialize)]
        struct UnitSummary {
            name: String,
            kind: String,
            signature: String,
            lines: String,
            tokens_full: usize,
            tokens_compact: usize,
            tokens_skeleton: usize,
            references: Vec<String>,
            calls_count: usize,
        }

        let summaries: Vec<UnitSummary> = units.iter().map(|u| UnitSummary {
            name: u.name.clone(),
            kind: u.kind.as_str().to_string(),
            signature: u.signature.clone(),
            lines: format!("{}-{}", u.start_line, u.end_line),
            tokens_full: u.est_tokens_full,
            tokens_compact: u.est_tokens_compact,
            tokens_skeleton: u.est_tokens_skeleton,
            references: u.references.clone(),
            calls_count: u.call_sites.len(),
        }).collect();

        match serde_json::to_string_pretty(&summaries) {
            Ok(json) => format!("Found {} definitions in '{}':\n\n{}", summaries.len(), params.path, json),
            Err(e) => format!("Error serializing results: {e}"),
        }
    }

    /// Return the list of programming languages and file extensions
    /// that trim supports for AST-based structural extraction.
    #[tool(
        name = "list_languages",
        description = "Return the list of programming languages and their associated file extensions that trim supports for Tree-Sitter based structural extraction."
    )]
    async fn list_languages(&self) -> String {
        let languages = [
            ("Rust", ".rs"),
            ("Python", ".py, .pyi"),
            ("JavaScript", ".js, .jsx, .mjs, .cjs"),
            ("TypeScript", ".ts, .mts, .cts"),
            ("TSX", ".tsx"),
            ("Go", ".go"),
            ("C", ".c, .h"),
            ("C++", ".cpp, .cc, .cxx, .hpp, .hxx, .hh"),
            ("Java", ".java"),
            ("C#", ".cs"),
            ("Ruby", ".rb, .rake, .gemspec"),
            ("PHP", ".php, .phtml"),
        ];

        let mut out = String::from("Supported languages (12):\n\n");
        out.push_str("| Language     | File Extensions                                |\n");
        out.push_str("|-------------|------------------------------------------------|\n");
        for (lang, exts) in &languages {
            out.push_str(&format!("| {:<11} | {:<46} |\n", lang, exts));
        }
        out
    }
}

#[tool_handler(
    name = "trim-mcp",
    version = "0.1.1",
    instructions = "MCP server for trim, a zero-config semantic context minimizer for LLM prompts. Exposes four tools: 'trim' to generate optimized codebase context payloads across 3 inclusion tiers, 'trim_plan' for agent loop planning, 'trim_file' to inspect structural units in a single file, and 'list_languages' to show supported languages."
)]
impl ServerHandler for TrimServer {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_default_env()
        .target(env_logger::Target::Stderr)
        .init();

    let server = TrimServer;
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
