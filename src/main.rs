use anyhow::Result;
use clap::Parser;
use llm_trim_core::{
    cache::parse_codebase_cached_with_options, format_scan_report, render_payload,
    scan_and_redact, select_within_budget, CodeGraph, CodeUnit, Inclusion, SessionStore,
    SkeletonReason, TrimConfig,
};
use llm_trim_rank::{derive_weak_intent, GitSignals, HeuristicRanker, Ranker, ScoreDiagnostic};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

/// trim: reduce a local codebase into a high-density LLM prompt payload.
///
/// Multi-tier pipeline: Tree-Sitter structural parsing -> intent & graph
/// ranking -> budget-driven 3-tier selection (Full/Compact/Skeleton) with
/// graceful degradation. Zero-config with persistent trim.config.toml support.
#[derive(Parser, Debug)]
#[command(name = "trim", version, about)]
struct Cli {
    /// Root directory to scan (defaults to current directory).
    #[arg(default_value = ".")]
    path: PathBuf,

    /// What you're trying to do / understand -- drives Tier 2 ranking.
    /// If omitted, all units are treated as equally relevant or weak intent is auto-derived.
    #[arg(short, long, default_value = "")]
    intent: String,

    /// Token budget for the final payload.
    #[arg(short, long, default_value_t = 8000)]
    budget: usize,

    /// Interactive TUI wizard mode: step-by-step interactive directory, intent, budget, and feature selection.
    #[arg(short = 'I', long)]
    interactive: bool,

    /// Write payload to a file instead of stdout.
    #[arg(short, long)]
    out: Option<PathBuf>,

    /// Print a summary (files scanned, units found, token usage, degradation stats, skipped files) to stderr.
    #[arg(long)]
    stats: bool,

    /// Explain mode: print detailed diagnostics per unit (exact score breakdown, real call edges, budget decision) to stderr.
    #[arg(long)]
    why: bool,

    /// Pull in direct caller/callee dependencies for full units to avoid disconnected payload fragments.
    #[arg(long)]
    deps: bool,

    /// Scan and redact secrets/credentials (API keys, tokens, private keys). Enabled by default.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    scan_secrets: bool,

    /// Explicitly disable secret scanning.
    #[arg(long)]
    no_scan_secrets: bool,

    /// Disable PageRank graph centrality scoring.
    #[arg(long)]
    no_graph: bool,

    /// PageRank centrality boost weight multiplier.
    #[arg(long, default_value_t = 0.5)]
    graph_weight: f32,

    /// Custom glob patterns to ignore in addition to .gitignore and .trimignore.
    #[arg(long = "ignore", value_name = "PATTERN")]
    ignore: Vec<String>,

    /// Enable behavioral Git recency signals.
    #[arg(long)]
    git_signals: bool,

    /// Disable behavioral Git recency signals.
    #[arg(long)]
    no_git_signals: bool,

    /// Continuous agent memory session ID.
    #[arg(long)]
    session: Option<String>,

    /// Continuous watch mode: watch repository for file modifications and update cache incrementally.
    #[arg(long)]
    watch: bool,

    /// Explicit path to a `trim.config.toml` or `trim.toml` file.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Which Tier 2 ranker to use ("heuristic" or "onnx").
    #[arg(long, default_value = "heuristic")]
    ranker: String,

    /// Path to an ONNX cross-encoder model.onnx (only used with --ranker onnx).
    #[arg(long)]
    model: Option<PathBuf>,

    /// Path to the matching tokenizer.json (only used with --ranker onnx).
    #[arg(long)]
    tokenizer: Option<PathBuf>,

    /// Max sequence length for the cross-encoder (only used with --ranker onnx).
    #[arg(long, default_value_t = 256)]
    max_length: usize,

    /// Disable incremental caching.
    #[arg(long)]
    no_cache: bool,

    /// Path to the cache file. Defaults to `.trim_cache` inside the scanned root directory.
    #[arg(long)]
    cache_file: Option<PathBuf>,

    /// Create a .trim_index.json file on disk with all units (metadata + text).
    /// Subsequent --grep and --extract calls read from this index.
    #[arg(long)]
    index: bool,

    /// Search the index file for a pattern (case-insensitive substring match on name, signature, file, kind).
    /// Returns matching units with snippets. Requires --index to have been run first.
    #[arg(long)]
    grep: Option<String>,

    /// Extract a specific unit from the index by name (exact match).
    /// Returns the full text of the unit. Requires --index to have been run first.
    #[arg(long)]
    extract: Option<String>,
}

/// Index file structure for --index, --grep, --extract workflow.
#[derive(Debug, Serialize, Deserialize)]
struct TrimIndex {
    version: u32,
    root: PathBuf,
    created: String,
    units: Vec<IndexUnit>,
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexUnit {
    id: usize,
    file: PathBuf,
    kind: String,
    name: String,
    signature: String,
    doc_comment: Option<String>,
    start_line: usize,
    end_line: usize,
    tokens_full: usize,
    tokens_compact: usize,
    tokens_skeleton: usize,
    references: Vec<String>,
    full_text: String,
    compact_text: String,
    skeleton_text: String,
}

impl From<&CodeUnit> for IndexUnit {
    fn from(u: &CodeUnit) -> Self {
        Self {
            id: u.id,
            file: u.file.clone(),
            kind: u.kind.as_str().to_string(),
            name: u.name.clone(),
            signature: u.signature.clone(),
            doc_comment: u.doc_comment.clone(),
            start_line: u.start_line,
            end_line: u.end_line,
            tokens_full: u.est_tokens_full,
            tokens_compact: u.est_tokens_compact,
            tokens_skeleton: u.est_tokens_skeleton,
            references: u.references.clone(),
            full_text: u.full_text.clone(),
            compact_text: u.compact_text.clone(),
            skeleton_text: u.skeleton_text.clone(),
        }
    }
}

const INDEX_VERSION: u32 = 1;
const INDEX_FILE_NAME: &str = ".trim_index.json";

fn index_path_for(root: &Path) -> PathBuf {
    root.join(INDEX_FILE_NAME)
}

fn create_index(root: &Path, units: &[CodeUnit]) -> Result<PathBuf> {
    let index = TrimIndex {
        version: INDEX_VERSION,
        root: root.to_path_buf(),
        created: chrono_now(),
        units: units.iter().map(IndexUnit::from).collect(),
    };
    let path = index_path_for(root);
    let json = serde_json::to_string_pretty(&index)?;
    std::fs::write(&path, json)?;
    eprintln!("trim: index created at {} ({} units)", path.display(), index.units.len());
    Ok(path)
}

fn load_index(root: &Path) -> Result<TrimIndex> {
    let path = index_path_for(root);
    if !path.exists() {
        anyhow::bail!(
            "index not found at {}. Run `trim --index {}` first.",
            path.display(),
            root.display()
        );
    }
    let json = std::fs::read_to_string(&path)?;
    let index: TrimIndex = serde_json::from_str(&json)?;
    if index.version != INDEX_VERSION {
        anyhow::bail!(
            "index version mismatch: expected {}, got {}. Re-run --index.",
            INDEX_VERSION,
            index.version
        );
    }
    Ok(index)
}

fn grep_index(root: &Path, pattern: &str) -> Result<()> {
    let index = load_index(root)?;
    let pat = pattern.to_lowercase();
    let mut matches: Vec<&IndexUnit> = index
        .units
        .iter()
        .filter(|u| {
            u.name.to_lowercase().contains(&pat)
                || u.signature.to_lowercase().contains(&pat)
                || u.kind.to_lowercase().contains(&pat)
                || u.file.display().to_string().to_lowercase().contains(&pat)
                || u.doc_comment
                    .as_ref()
                    .map(|d| d.to_lowercase().contains(&pat))
                    .unwrap_or(false)
                || u.references.iter().any(|r| r.to_lowercase().contains(&pat))
        })
        .collect();

    matches.sort_by(|a, b| b.tokens_full.cmp(&a.tokens_full));

    if matches.is_empty() {
        eprintln!("trim grep: no matches for '{pattern}' in index ({} units)", index.units.len());
        return Ok(());
    }

    eprintln!(
        "trim grep: {} matches for '{}' (showing snippets)\n",
        matches.len(),
        pattern
    );

    for u in &matches {
        let file_display = u.file.display().to_string().replace('\\', "/");
        // Show compact text as snippet (truncated to first 5 lines)
        let snippet: String = u
            .compact_text
            .lines()
            .take(5)
            .collect::<Vec<_>>()
            .join("\n");
        println!("--- {} ({}:{})", u.name, file_display, u.start_line);
        println!("    kind: {}, tokens: {}", u.kind, u.tokens_full);
        println!("{snippet}");
        println!();
    }

    eprintln!(
        "trim grep: {} matches found. Use --extract <name> to get full text.",
        matches.len()
    );
    Ok(())
}

fn extract_from_index(root: &Path, unit_name: &str) -> Result<()> {
    let index = load_index(root)?;
    let matches: Vec<&IndexUnit> = index
        .units
        .iter()
        .filter(|u| u.name == unit_name)
        .collect();

    if matches.is_empty() {
        anyhow::bail!(
            "no unit named '{}' in index. Use --grep to search.",
            unit_name
        );
    }

    if matches.len() > 1 {
        eprintln!(
            "trim extract: {} units named '{}'. Showing all:\n",
            matches.len(),
            unit_name
        );
        for u in &matches {
            let file_display = u.file.display().to_string().replace('\\', "/");
            println!("// === {}:{}-{} ===", file_display, u.start_line, u.end_line);
            println!("// kind: {}, tokens: {}", u.kind, u.tokens_full);
            if let Some(ref doc) = u.doc_comment {
                println!("// doc: {doc}");
            }
            println!("{}\n", u.full_text);
        }
        return Ok(());
    }

    let u = matches[0];
    let file_display = u.file.display().to_string().replace('\\', "/");
    println!("// === {}:{}-{} ===", file_display, u.start_line, u.end_line);
    println!("// kind: {}, tokens_full: {}, compact: {}, skeleton: {}", u.kind, u.tokens_full, u.tokens_compact, u.tokens_skeleton);
    if let Some(ref doc) = u.doc_comment {
        println!("// doc: {doc}");
    }
    println!();
    println!("{}", u.full_text);

    Ok(())
}

fn chrono_now() -> String {
    // Simple timestamp without adding chrono dependency
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{now}")
}

fn run_interactive_wizard(mut cli: Cli) -> Result<Cli> {
    eprintln!("\n=== trim Interactive Context Minimizer Wizard ===");
    eprintln!("Interactive step-by-step setup. Press Enter to accept defaults.\n");

    let path_str = inquire::Text::new("Target repository directory to scan:")
        .with_default(cli.path.to_str().unwrap_or("."))
        .prompt()?;
    cli.path = PathBuf::from(path_str);

    let intent = inquire::Text::new("Task intent or query (e.g. 'auth verification', or blank for overview):")
        .with_default(&cli.intent)
        .prompt()?;
    cli.intent = intent;

    let budget_options = vec![
        "2000  (Compact - fast context, small edits)",
        "4000  (Medium - standard function analysis)",
        "8000  (Default - broad architectural coverage)",
        "16000 (Large - multi-module refactoring)",
        "32000 (Extra Large - monorepo whole-project context)",
        "Custom...",
    ];

    let budget_selection = inquire::Select::new("Target Token Budget:", budget_options).prompt()?;
    if budget_selection.starts_with("2000") {
        cli.budget = 2000;
    } else if budget_selection.starts_with("4000") {
        cli.budget = 4000;
    } else if budget_selection.starts_with("8000") {
        cli.budget = 8000;
    } else if budget_selection.starts_with("16000") {
        cli.budget = 16000;
    } else if budget_selection.starts_with("32000") {
        cli.budget = 32000;
    } else {
        let custom = inquire::CustomType::<usize>::new("Enter custom token budget:").prompt()?;
        cli.budget = custom;
    }

    let feature_options = vec![
        "Pull Dependencies (--deps)",
        "Scan & Redact Secrets (--scan-secrets)",
        "Git Freshness Signals (--git-signals)",
        "Summary Statistics (--stats)",
        "Explain Mode Diagnostics (--why)",
    ];

    let selected_features = inquire::MultiSelect::new("Enable Features:", feature_options)
        .with_default(&[1, 3]) // secrets & stats default
        .prompt()?;

    for feat in selected_features {
        if feat.contains("--deps") {
            cli.deps = true;
        }
        if feat.contains("--scan-secrets") {
            cli.scan_secrets = true;
        }
        if feat.contains("--git-signals") {
            cli.git_signals = true;
        }
        if feat.contains("--stats") {
            cli.stats = true;
        }
        if feat.contains("--why") {
            cli.why = true;
        }
    }

    eprintln!("\nStarting trim minimization pipeline...\n");
    Ok(cli)
}

fn execute_trim_pipeline(cli: &Cli) -> Result<()> {
    let mut custom_ignores = cli.ignore.clone();

    // Discover and merge persistent configuration
    let config_opt = match &cli.config {
        Some(cfg_path) => TrimConfig::load(cfg_path).ok().map(|c| (c, cfg_path.clone())),
        None => TrimConfig::discover_and_load(&cli.path),
    };

    let mut effective_intent = cli.intent.clone();
    let mut effective_budget = cli.budget;
    let mut effective_deps = cli.deps;
    let mut effective_scan_secrets = if cli.no_scan_secrets {
        false
    } else {
        cli.scan_secrets
    };
    let mut effective_no_graph = cli.no_graph;
    let mut effective_graph_weight = cli.graph_weight;
    let mut effective_cache_file = cli.cache_file.clone();
    let mut effective_git_signals = cli.git_signals && !cli.no_git_signals;
    let mut effective_session = cli.session.clone();

    if let Some((cfg, _cfg_path)) = config_opt {
        if effective_intent.is_empty() {
            if let Some(i) = cfg.intent {
                effective_intent = i;
            }
        }
        if effective_budget == 8000 {
            if let Some(b) = cfg.budget {
                effective_budget = b;
            }
        }
        if !effective_deps && cfg.deps.unwrap_or(false) {
            effective_deps = true;
        }
        if !cli.no_scan_secrets && cfg.scan_secrets.is_some() {
            effective_scan_secrets = cfg.scan_secrets.unwrap();
        }
        if !effective_no_graph && cfg.no_graph.unwrap_or(false) {
            effective_no_graph = true;
        }
        if let Some(gw) = cfg.graph_weight {
            effective_graph_weight = gw;
        }
        if effective_cache_file.is_none() {
            if let Some(cf) = cfg.cache_file {
                effective_cache_file = Some(PathBuf::from(cf));
            }
        }
        if !effective_git_signals && cfg.git_signals.unwrap_or(false) {
            effective_git_signals = true;
        }
        if effective_session.is_none() {
            effective_session = cfg.session_id;
        }
        if let Some(cfg_ignores) = cfg.ignore {
            custom_ignores.extend(cfg_ignores);
        }
    }

    // Weak intent auto-derivation fallback when intent is empty
    let mut weak_intent_derived = false;
    if effective_intent.trim().is_empty() {
        if let Some(derived) = derive_weak_intent(&cli.path) {
            effective_intent = derived;
            weak_intent_derived = true;
        }
    }

    let cache_enabled = !cli.no_cache;
    let cache_path = effective_cache_file.as_deref();
    let (units, skipped_stats) = parse_codebase_cached_with_options(
        &cli.path,
        cache_path,
        cache_enabled,
        &custom_ignores,
        true,
    )?;

    if units.is_empty() {
        eprintln!("trim: no supported source files found under {}", cli.path.display());
        return Ok(());
    }

    // Build true AST call graph
    let graph = CodeGraph::build(&units);

    // Compute lexical and semantic scores
    let heuristic = HeuristicRanker::new();
    let lexical_scores = build_scores(cli, &effective_intent, &units, &heuristic)?;
    let diagnostics = heuristic.score_diagnostics(&effective_intent, &units);
    let mut diag_map: HashMap<usize, ScoreDiagnostic> = diagnostics
        .into_iter()
        .map(|d| (d.unit_id, d))
        .collect();

    // Centrality boost
    let effective_weight = if effective_no_graph { 0.0 } else { effective_graph_weight };
    let mut scores = graph.apply_centrality_boost(&lexical_scores, &units, effective_weight);

    // Update diagnostics with centrality
    for u in &units {
        let cent = graph.centrality.get(&u.id).copied().unwrap_or(0.0);
        let cent_boost = (effective_weight * cent * 2.0).min(4.0);
        if let Some(d) = diag_map.get_mut(&u.id) {
            d.centrality_score = cent_boost;
            d.total_score = d.lexical_score + cent_boost;
        }
    }

    // Optional behavioral Git freshness signals
    if effective_git_signals {
        let git_signals = GitSignals::from_repo(&cli.path);
        for u in &units {
            let recency = git_signals.get_file_recency(&cli.path, &u.file);
            if recency > 0.0 {
                let boost = (recency * 1.5).min(3.0);
                if let Some(s) = scores.get_mut(&u.id) {
                    *s += boost;
                }
                if let Some(d) = diag_map.get_mut(&u.id) {
                    d.git_boost = boost;
                    d.total_score += boost;
                }
            }
        }
    }

    // Optional continuous session hot-set memory
    let session_store = effective_session
        .as_ref()
        .map(|s| SessionStore::load_or_create(&cli.path, s));

    if let Some(store) = &session_store {
        store.apply_session_boost(&units, &mut scores, 1.5);
    }

    // Initial budget selection pass
    let mut plan = select_within_budget(&units, &scores, effective_budget);

    // If --deps is enabled, pull direct dependencies of Full units via true AST edges
    let mut pulled_deps_reasons: HashMap<usize, String> = HashMap::new();
    if effective_deps {
        let full_ids: Vec<usize> = plan
            .included
            .iter()
            .filter(|p| matches!(p.inclusion, Inclusion::Full))
            .map(|p| p.unit_id)
            .collect();
        let direct_deps = graph.pull_direct_dependencies(&full_ids);
        if !direct_deps.is_empty() {
            let unit_map: HashMap<usize, &llm_trim_core::CodeUnit> = units.iter().map(|u| (u.id, u)).collect();
            for &dep_id in &direct_deps {
                let incoming = graph.get_incoming_edges(dep_id);
                if let Some(edge) = incoming.first() {
                    let caller_name = unit_map.get(&edge.caller_id).map(|u| u.name.as_str()).unwrap_or("caller");
                    let reason = format!(
                        "pulled because {} calls it at {}:{}",
                        caller_name,
                        edge.caller_file.display().to_string().replace('\\', "/"),
                        edge.caller_line
                    );
                    pulled_deps_reasons.insert(dep_id, reason);
                }

                if let Some(s) = scores.get_mut(&dep_id) {
                    *s += 2.5; // priority boost to pull in dependency
                }
                if let Some(d) = diag_map.get_mut(&dep_id) {
                    d.dep_boost = 2.5;
                    d.total_score += 2.5;
                }
            }
            plan = select_within_budget(&units, &scores, effective_budget);
        }
    }

    // Explain mode diagnostics (--why)
    if cli.why {
        let plan_by_id: HashMap<usize, _> = plan.included.iter().map(|p| (p.unit_id, p)).collect();

        eprintln!("\n=== trim Explain Mode (--why) ===");
        eprintln!("Scanned root: {}, Token Budget: {}", cli.path.display(), effective_budget);
        if !effective_intent.is_empty() {
            let suffix = if weak_intent_derived { " [auto-derived]" } else { "" };
            eprintln!("Intent query: \"{}\"{}", effective_intent, suffix);
        }
        eprintln!("--------------------------------------------------------------------------------");

        let mut sorted_units: Vec<&llm_trim_core::CodeUnit> = units.iter().collect();
        sorted_units.sort_by(|a, b| {
            let sa = scores.get(&a.id).copied().unwrap_or(0.0);
            let sb = scores.get(&b.id).copied().unwrap_or(0.0);
            sb.partial_cmp(&sa)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.file.cmp(&b.file))
                .then_with(|| a.start_line.cmp(&b.start_line))
                .then_with(|| a.name.cmp(&b.name))
        });

        for u in sorted_units {
            let diag = diag_map.get(&u.id);
            let planned = plan_by_id.get(&u.id);
            let incoming = graph.get_incoming_edges(u.id);

            let status_str = match planned {
                Some(p) => match p.inclusion {
                    Inclusion::Full => {
                        if let Some(reason) = pulled_deps_reasons.get(&u.id) {
                            format!("[FULL] (tokens: {}, {})", p.tokens, reason)
                        } else {
                            format!("[FULL] (tokens: {})", p.tokens)
                        }
                    }
                    Inclusion::Compact => format!("[COMPACT] (tokens: {})", p.tokens),
                    Inclusion::Skeleton => {
                        let reason = match p.skeleton_reason {
                            Some(SkeletonReason::BudgetExhausted) => "budget exhausted near threshold",
                            Some(SkeletonReason::LowRelevance) => "low relevance",
                            None => "baseline signature",
                        };
                        format!("[SKELETON] (tokens: {}, reason: {})", p.tokens, reason)
                    }
                },
                None => "[EXCLUDED] (budget exceeded)".to_string(),
            };

            eprintln!(
                "{:<26} {} (kind: {}, file: {}:{})",
                status_str,
                u.name,
                u.kind.as_str(),
                u.file.display().to_string().replace('\\', "/"),
                u.start_line
            );

            if let Some(d) = diag {
                let match_str = if d.matched_terms.is_empty() {
                    "none".to_string()
                } else {
                    d.matched_terms.join(", ")
                };
                eprintln!(
                    "   └─ Score: {:.2} (lexical: {:.2}, centrality: {:.2}, deps: {:.2}, git: {:.2}) | Matched terms: [{}]",
                    d.total_score, d.lexical_score, d.centrality_score, d.dep_boost, d.git_boost, match_str
                );
            }

            if !incoming.is_empty() && incoming.len() <= 3 {
                for edge in incoming {
                    eprintln!(
                        "      ├─ Edge: called by unit #{} at {}:{}",
                        edge.caller_id,
                        edge.caller_file.display().to_string().replace('\\', "/"),
                        edge.caller_line
                    );
                }
            }
        }
        eprintln!("--------------------------------------------------------------------------------\n");
    }

    let mut payload = render_payload(&units, &plan);

    // Secret scanning & redaction (default ON)
    let mut detections_count = 0;
    if effective_scan_secrets {
        let (redacted, detections) = scan_and_redact(&payload);
        payload = redacted;
        detections_count = detections.len();
        if !detections.is_empty() {
            eprintln!("\n{}", format_scan_report(&detections));
        }
    }

    if cli.stats {
        let full_count = plan
            .included
            .iter()
            .filter(|p| matches!(p.inclusion, Inclusion::Full))
            .count();
        let compact_count = plan
            .included
            .iter()
            .filter(|p| matches!(p.inclusion, Inclusion::Compact))
            .count();
        let skel_count = plan.included.len() - full_count - compact_count;

        let total_raw_tokens: usize = units.iter().map(|u| u.est_tokens_full).sum();
        let compression_pct = if total_raw_tokens > 0 {
            (1.0 - (plan.used_tokens as f64 / total_raw_tokens as f64)) * 100.0
        } else {
            0.0
        };

        let budget_exhausted_count = plan
            .included
            .iter()
            .filter(|p| matches!(p.skeleton_reason, Some(SkeletonReason::BudgetExhausted)))
            .count();
        let low_relevance_count = plan
            .included
            .iter()
            .filter(|p| matches!(p.skeleton_reason, Some(SkeletonReason::LowRelevance)))
            .count();

        eprintln!(
            "trim: {} units found across {} files, {} included ({} full, {} compact, {} skeleton), {} excluded, {}/{} tokens used ({:.1}% compression from {} raw tokens)",
            units.len(),
            units.iter().map(|u| &u.file).collect::<std::collections::HashSet<_>>().len(),
            plan.included.len(),
            full_count,
            compact_count,
            skel_count,
            plan.excluded_unit_ids.len(),
            plan.used_tokens,
            plan.budget_tokens,
            compression_pct,
            total_raw_tokens
        );

        if budget_exhausted_count > 0 || low_relevance_count > 0 {
            eprintln!(
                "      Budget boundary: {} degraded to skeleton due to budget exhaustion, {} due to low relevance.",
                budget_exhausted_count,
                low_relevance_count
            );
        }

        if !plan.cannibalization_warnings.is_empty() {
            for warn in &plan.cannibalization_warnings {
                eprintln!(
                    "      Warning: Unit '{}' consumes {} tokens ({:.1}% of budget {})",
                    warn.unit_name, warn.tokens_used, warn.pct_of_budget, plan.budget_tokens
                );
            }
        }

        if skipped_stats.ignored_files_count > 0 || skipped_stats.binary_files_count > 0 {
            let mb_saved = skipped_stats.binary_bytes_skipped as f64 / (1024.0 * 1024.0);
            eprintln!(
                "      Skipped: {} ignored files by .gitignore/.trimignore, {} binary files ({:.2} MB saved without parsing)",
                skipped_stats.ignored_files_count,
                skipped_stats.binary_files_count,
                mb_saved
            );
        }

        if effective_scan_secrets {
            eprintln!(
                "      Security: Secret scanning active ({} credentials redacted).",
                detections_count
            );
        }
    }

    // Save session memory if active
    if let Some(mut store) = session_store {
        store.record_plan(&units, &plan);
        let _ = store.save(&cli.path);
    }

    match &cli.out {
        Some(path) => std::fs::write(path, payload)?,
        None => print!("{payload}"),
    }

    Ok(())
}

fn main() -> Result<()> {
    env_logger::init();
    let mut cli = Cli::parse();

    // Handle --grep and --extract before pipeline (read from existing index)
    if let Some(ref pattern) = cli.grep {
        let root = cli.path.canonicalize().unwrap_or_else(|_| cli.path.clone());
        return grep_index(&root, pattern);
    }
    if let Some(ref name) = cli.extract {
        let root = cli.path.canonicalize().unwrap_or_else(|_| cli.path.clone());
        return extract_from_index(&root, name);
    }

    if cli.interactive {
        cli = run_interactive_wizard(cli)?;
    }

    if cli.watch {
        eprintln!("trim: entering continuous watch mode on '{}'...", cli.path.display());
        loop {
            if let Err(e) = execute_trim_pipeline(&cli) {
                eprintln!("trim: watch cycle error: {e}");
            }
            sleep(Duration::from_secs(2));
        }
    } else {
        execute_trim_pipeline(&cli)?;
    }

    // Handle --index: create index file after pipeline runs
    if cli.index {
        let root = cli.path.canonicalize().unwrap_or_else(|_| cli.path.clone());
        let cache_enabled = !cli.no_cache;
        let cache_path = cli.cache_file.as_deref();
        let custom_ignores = cli.ignore.clone();
        let (units, _) = parse_codebase_cached_with_options(
            &cli.path,
            cache_path,
            cache_enabled,
            &custom_ignores,
            true,
        )?;
        create_index(&root, &units)?;
    }

    Ok(())
}

fn build_scores(
    cli: &Cli,
    intent: &str,
    units: &[llm_trim_core::CodeUnit],
    heuristic: &HeuristicRanker,
) -> Result<std::collections::HashMap<usize, f32>> {
    match cli.ranker.as_str() {
        "heuristic" => Ok(heuristic.score(intent, units)),
        "onnx" => {
            #[cfg(feature = "onnx")]
            {
                let model = cli
                    .model
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("--ranker onnx requires --model <path to model.onnx>"))?;
                let tokenizer = cli.tokenizer.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("--ranker onnx requires --tokenizer <path to tokenizer.json>")
                })?;
                let ranker = llm_trim_rank::onnx::OnnxCrossEncoderRanker::load(
                    model.to_str().unwrap(),
                    tokenizer.to_str().unwrap(),
                    cli.max_length,
                )?;
                Ok(ranker.score(intent, units))
            }
            #[cfg(not(feature = "onnx"))]
            {
                anyhow::bail!(
                    "--ranker onnx requires building with `cargo build --features onnx` (see MODELS.md)"
                );
            }
        }
        other => anyhow::bail!("unknown --ranker '{other}', expected 'heuristic' or 'onnx'"),
    }
}