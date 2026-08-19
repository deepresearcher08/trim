use anyhow::Result;
use clap::Parser;
use llm_trim_core::{
    parse_codebase_cached, render_payload, scan_and_redact, select_within_budget,
    CodeGraph, Inclusion, SkeletonReason, TrimConfig,
};
use llm_trim_rank::{HeuristicRanker, Ranker};
use std::collections::HashMap;
use std::path::PathBuf;

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
    /// If omitted, all units are treated as equally relevant.
    #[arg(short, long, default_value = "")]
    intent: String,

    /// Token budget for the final payload.
    #[arg(short, long, default_value_t = 8000)]
    budget: usize,

    /// Write payload to a file instead of stdout.
    #[arg(short, long)]
    out: Option<PathBuf>,

    /// Print a summary (files scanned, units found, token usage, degradation stats) to stderr.
    #[arg(long)]
    stats: bool,

    /// Explain mode: print detailed diagnostics per unit (score breakdown, graph centrality, budget decision) to stderr.
    #[arg(long)]
    why: bool,

    /// Pull in direct caller/callee dependencies for full units to avoid disconnected payload fragments.
    #[arg(long)]
    deps: bool,

    /// Scan and redact secrets/credentials (API keys, tokens, private keys) before emitting payload.
    #[arg(long)]
    scan_secrets: bool,

    /// Explicit path to a `trim.config.toml` or `trim.toml` file.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Which Tier 2 ranker to use. "heuristic" (default, zero-config) or
    /// "onnx" (requires --model and --tokenizer, and the binary must be
    /// built with `--features onnx`).
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

    /// Disable incremental caching. Every invocation will re-parse the
    /// entire repository from scratch without reading or writing a
    /// .trim_cache file.
    #[arg(long)]
    no_cache: bool,

    /// Path to the cache file. Defaults to `.trim_cache` inside the
    /// scanned root directory.
    #[arg(long)]
    cache_file: Option<PathBuf>,
}

fn main() -> Result<()> {
    env_logger::init();
    let mut cli = Cli::parse();

    // Discover and merge persistent configuration
    let config_opt = match &cli.config {
        Some(cfg_path) => TrimConfig::load(cfg_path).ok().map(|c| (c, cfg_path.clone())),
        None => TrimConfig::discover_and_load(&cli.path),
    };

    if let Some((cfg, _cfg_path)) = config_opt {
        if cli.intent.is_empty() {
            if let Some(i) = cfg.intent {
                cli.intent = i;
            }
        }
        if cli.budget == 8000 {
            if let Some(b) = cfg.budget {
                cli.budget = b;
            }
        }
        if !cli.deps && cfg.deps.unwrap_or(false) {
            cli.deps = true;
        }
        if !cli.scan_secrets && cfg.scan_secrets.unwrap_or(false) {
            cli.scan_secrets = true;
        }
        if cli.cache_file.is_none() {
            if let Some(cf) = cfg.cache_file {
                cli.cache_file = Some(PathBuf::from(cf));
            }
        }
    }

    let cache_enabled = !cli.no_cache;
    let cache_path = cli.cache_file.as_deref();
    let units = parse_codebase_cached(&cli.path, cache_path, cache_enabled)?;

    if units.is_empty() {
        eprintln!("trim: no supported source files found under {}", cli.path.display());
        return Ok(());
    }

    // Build cross-file dependency graph and PageRank centrality
    let graph = CodeGraph::build(&units);

    // Compute lexical and semantic scores
    let heuristic = HeuristicRanker::new();
    let lexical_scores = build_scores(&cli, &units, &heuristic)?;

    // Fold graph centrality into final scores (boost foundational symbols)
    let mut scores = graph.apply_centrality_boost(&lexical_scores, &units, 0.4);

    // Initial budget selection pass
    let mut plan = select_within_budget(&units, &scores, cli.budget);

    // If --deps is enabled, boost direct dependencies of Full units and re-plan
    if cli.deps {
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
                    *s += 2.5; // priority boost to pull in dependency
                }
            }
            plan = select_within_budget(&units, &scores, cli.budget);
        }
    }

    // Explain mode diagnostics (--why)
    if cli.why {
        let diagnostics = heuristic.score_diagnostics(&cli.intent, &units);
        let diag_by_id: HashMap<usize, _> = diagnostics.into_iter().map(|d| (d.unit_id, d)).collect();
        let plan_by_id: HashMap<usize, _> = plan.included.iter().map(|p| (p.unit_id, p)).collect();

        eprintln!("\n=== trim Explain Mode (--why) ===");
        eprintln!("Scanned root: {}, Token Budget: {}", cli.path.display(), cli.budget);
        if !cli.intent.is_empty() {
            eprintln!("Intent query: \"{}\"", cli.intent);
        }
        eprintln!("--------------------------------------------------------------------------------");

        for u in &units {
            let diag = diag_by_id.get(&u.id);
            let planned = plan_by_id.get(&u.id);
            let cent = graph.centrality.get(&u.id).copied().unwrap_or(0.0);
            let final_score = scores.get(&u.id).copied().unwrap_or(0.0);

            let status_str = match planned {
                Some(p) => match p.inclusion {
                    Inclusion::Full => format!("[FULL] (tokens: {})", p.tokens),
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
                "{:<26} {} (file: {}:{})",
                status_str,
                u.name,
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
                    "   └─ Score: {:.2} (lexical: {:.2}, centrality: {:.2}) | Matched terms: [{}]",
                    final_score, d.total_score, cent, match_str
                );
            }
        }
        eprintln!("--------------------------------------------------------------------------------\n");
    }

    let mut payload = render_payload(&units, &plan);

    // Optional secret / credential scanning & redaction
    if cli.scan_secrets {
        let (sanitized, detections) = scan_and_redact(&payload);
        if !detections.is_empty() {
            eprintln!(
                "trim [SECURITY]: Redacted {} sensitive credential/secret occurrence(s) in payload.",
                detections.len()
            );
        }
        payload = sanitized;
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
        let skeleton_count = plan.included.len() - full_count - compact_count;

        let total_raw_tokens: usize = units.iter().map(|u| u.est_tokens_full).sum();
        let reduction = if total_raw_tokens > 0 {
            (100.0 * (1.0 - (plan.used_tokens as f64 / total_raw_tokens as f64))).max(0.0)
        } else {
            0.0
        };

        let budget_exhausted_count = plan.budget_exhausted_units.len();
        let low_relevance_count = plan
            .included
            .iter()
            .filter(|p| matches!(p.skeleton_reason, Some(SkeletonReason::LowRelevance)))
            .count();

        eprintln!(
            "trim: {} units found, {} included ({} full, {} compact, {} skeleton), {} excluded, {}/{} tokens used ({:.1}% compression from {} raw tokens)",
            units.len(),
            plan.included.len(),
            full_count,
            compact_count,
            skeleton_count,
            plan.excluded_unit_ids.len(),
            plan.used_tokens,
            plan.budget_tokens,
            reduction,
            total_raw_tokens,
        );
        eprintln!(
            "      Budget boundary: {} degraded to skeleton due to budget exhaustion, {} due to low relevance.",
            budget_exhausted_count,
            low_relevance_count
        );
    }

    match cli.out {
        Some(path) => std::fs::write(&path, payload)?,
        None => print!("{payload}"),
    }

    Ok(())
}

fn build_scores(
    cli: &Cli,
    units: &[llm_trim_core::CodeUnit],
    heuristic: &HeuristicRanker,
) -> Result<std::collections::HashMap<usize, f32>> {
    match cli.ranker.as_str() {
        "heuristic" => Ok(heuristic.score(&cli.intent, units)),
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
                Ok(ranker.score(&cli.intent, units))
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