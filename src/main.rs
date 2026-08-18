use anyhow::Result;
use clap::Parser;
use llm_trim_core::{parse_codebase_cached, render_payload, select_within_budget};
use llm_trim_rank::{HeuristicRanker, Ranker};
use std::path::PathBuf;

/// trim: reduce a local codebase into a high-density LLM prompt payload.
///
/// Three-tier pipeline: Tree-Sitter structural parsing -> intent-based
/// semantic ranking -> budget-driven skeletonization. Zero-config: point
/// it at a directory and a token budget and it does the rest.
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

    /// Print a summary (files scanned, units found, tokens used) to stderr.
    #[arg(long)]
    stats: bool,

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
    let cli = Cli::parse();

    let cache_enabled = !cli.no_cache;
    let cache_path = cli.cache_file.as_deref();
    let units = parse_codebase_cached(&cli.path, cache_path, cache_enabled)?;

    if units.is_empty() {
        eprintln!("trim: no supported source files found under {}", cli.path.display());
        return Ok(());
    }

    let scores = build_scores(&cli, &units)?;

    let plan = select_within_budget(&units, &scores, cli.budget);
    let payload = render_payload(&units, &plan);

    if cli.stats {
        let full_count = plan
            .included
            .iter()
            .filter(|p| matches!(p.inclusion, llm_trim_core::budget::Inclusion::Full))
            .count();
        let skeleton_count = plan.included.len() - full_count;
        let total_raw_tokens: usize = units.iter().map(|u| u.est_tokens_full).sum();
        let reduction = if total_raw_tokens > 0 {
            (100.0 * (1.0 - (plan.used_tokens as f64 / total_raw_tokens as f64))).max(0.0)
        } else {
            0.0
        };
        eprintln!(
            "trim: {} units found, {} included ({} full, {} skeleton), {} excluded, {}/{} tokens used ({:.1}% compression from {} raw tokens)",
            units.len(),
            plan.included.len(),
            full_count,
            skeleton_count,
            plan.excluded_unit_ids.len(),
            plan.used_tokens,
            plan.budget_tokens,
            reduction,
            total_raw_tokens,
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
) -> Result<std::collections::HashMap<usize, f32>> {
    match cli.ranker.as_str() {
        "heuristic" => Ok(HeuristicRanker::new().score(&cli.intent, units)),
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