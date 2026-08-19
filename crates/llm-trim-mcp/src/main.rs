// trim-mcp: MCP server exposing trim's codebase context minimization
// as callable tools for LLM agents (Claude Code, Cursor, Cline, etc.).
//
// Communicates over stdio using the official rmcp SDK.

use rmcp::{
    ServiceExt,
    ServerHandler,
    schemars,
    tool, tool_router, tool_handler,
    handler::server::wrapper::Parameters,
    transport::stdio,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use llm_trim_core::{
    parse_codebase_cached,
    extract_units,
    select_within_budget,
    render_payload,
    CodeGraph,
    Inclusion,
    Language,
};
use llm_trim_rank::{HeuristicRanker, Ranker};

// -- Tool parameter schemas --------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct TrimParams {
    /// Absolute or relative path to the repository root directory to scan.
    path: String,

    /// Natural language description of what the caller is trying to
    /// understand or accomplish. Drives relevance ranking so that the
    /// returned payload prioritizes structurally relevant code units.
    /// Leave empty to treat all units as equally relevant.
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

    /// When true, skip reading or writing the .trim_cache file and
    /// re-parse every source file from scratch.
    #[schemars(default)]
    no_cache: Option<bool>,
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
    /// prompts. Supports Rust, Python, JavaScript, TypeScript, TSX,
    /// and Go source files.
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

        let budget = params.budget.unwrap_or(8000);
        let intent = params.intent.as_deref().unwrap_or("");
        let cache_enabled = !params.no_cache.unwrap_or(false);
        let pull_deps = params.deps.unwrap_or(false);

        let units = match parse_codebase_cached(&root, None, cache_enabled) {
            Ok(u) => u,
            Err(e) => return format!("Error scanning codebase: {e}"),
        };

        if units.is_empty() {
            return format!("No supported source files found under '{}'.", params.path);
        }

        let graph = CodeGraph::build(&units);
        let ranker = HeuristicRanker::new();
        let lexical_scores = ranker.score(intent, &units);
        let mut scores = graph.apply_centrality_boost(&lexical_scores, &units, 0.4);

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

        let payload = render_payload(&units, &plan);

        let full_count = plan.included.iter()
            .filter(|p| matches!(p.inclusion, Inclusion::Full))
            .count();
        let compact_count = plan.included.iter()
            .filter(|p| matches!(p.inclusion, Inclusion::Compact))
            .count();
        let skel_count = plan.included.len() - full_count - compact_count;

        format!(
            "# trim summary: {} units found, {} included ({} full, {} compact, {} skeleton), {} excluded, {}/{} tokens used\n\n{}",
            units.len(),
            plan.included.len(),
            full_count,
            compact_count,
            skel_count,
            plan.excluded_unit_ids.len(),
            plan.used_tokens,
            plan.budget_tokens,
            payload,
        )
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
    version = "0.1.0",
    instructions = "MCP server for trim, a zero-config semantic context minimizer for LLM prompts. Exposes three tools: 'trim' to generate optimized codebase context payloads across 3 inclusion tiers, 'trim_file' to inspect structural units in a single file, and 'list_languages' to show supported languages."
)]
impl ServerHandler for TrimServer {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Redirect log output to stderr so it does not interfere with the
    // JSON-RPC protocol on stdout.
    env_logger::Builder::from_default_env()
        .target(env_logger::Target::Stderr)
        .init();

    let server = TrimServer;
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
