//! llm-trim-core
//!
//! Tier 1: Tree-Sitter based structural parsing + skeletonization (with 3 inclusion tiers).
//! Tier 2 (graph): In-memory reference graph & PageRank centrality.
//! Tier 3: budget-driven selection engine with graceful degradation.
//!
//! Incremental caching (.trim_cache) avoids redundant AST re-parsing
//! across invocations by tracking file metadata and content hashes.

pub mod budget;
pub mod cache;
pub mod config;
pub mod graph;
pub mod lang;
pub mod secrets;
pub mod session;
pub mod skeleton;
pub mod unit;

pub use budget::{render_payload, select_within_budget, BudgetPlan, Inclusion, PlannedUnit, SkeletonReason};
pub use cache::{parse_codebase_cached, CacheStore};
pub use config::TrimConfig;
pub use graph::{CodeGraph, GraphEdge};
pub use lang::Language;
pub use secrets::{format_scan_report, scan_and_redact, scan_and_redact_file, SecretDetection};
pub use session::SessionStore;
pub use skeleton::extract_units;
pub use unit::{estimate_tokens, CallSite, CodeUnit, UnitKind};

use anyhow::Result;
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use rayon::prelude::*;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct SkippedStats {
    pub ignored_files_count: usize,
    pub binary_files_count: usize,
    pub binary_bytes_skipped: u64,
    pub skipped_samples: Vec<String>,
}

const DEFAULT_IGNORED_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "dist",
    "build",
    "venv",
    ".venv",
    "__pycache__",
    ".pytest_cache",
    "target",
    "vendor",
    ".next",
    ".turbo",
    ".cache",
    ".output",
    "out",
    "coverage",
    ".idea",
    ".vscode",
];

const KNOWN_BINARY_EXTENSIONS: &[&str] = &[
    "gguf", "bin", "exe", "dll", "so", "dylib", "wasm", "zip", "tar", "gz", "7z", "iso", "png",
    "jpg", "jpeg", "gif", "webp", "ico", "mp4", "mp3", "wav", "pdf", "docx", "xlsx", "parquet",
    "arrow", "pkl", "h5", "onnx", "pt", "pth", "db", "sqlite", "pyc", "class", "o", "a",
];

/// Fast check for binary files using file extension, GGUF headers, and null-byte sampling in the first 8KB.
pub fn is_binary_file(path: &Path) -> bool {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ext_lower = ext.to_ascii_lowercase();
        if KNOWN_BINARY_EXTENSIONS.contains(&ext_lower.as_str()) {
            return true;
        }
    }

    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };

    let mut buffer = [0u8; 8192];
    let n = match file.read(&mut buffer) {
        Ok(n) => n,
        Err(_) => return false,
    };

    if n >= 4 && &buffer[..4] == b"GGUF" {
        return true;
    }

    buffer[..n].contains(&0)
}

/// Walk `root`, respecting .gitignore, .trimignore, and default ignores, and return every source file
/// whose extension maps to a supported Language.
pub fn discover_source_files(root: &Path) -> Result<Vec<PathBuf>> {
    let (files, _) = discover_source_files_with_stats(root, &[], true)?;
    Ok(files)
}

/// Walk `root` with custom ignore patterns and return discovered source files along with SkippedStats.
pub fn discover_source_files_with_stats(
    root: &Path,
    custom_ignores: &[String],
    respect_trimignore: bool,
) -> Result<(Vec<PathBuf>, SkippedStats)> {
    let mut files = Vec::new();
    let mut stats = SkippedStats::default();

    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true);

    if respect_trimignore {
        builder.add_custom_ignore_filename(".trimignore");
    }

    let mut override_builder = OverrideBuilder::new(root);
    for dir in DEFAULT_IGNORED_DIRS {
        let _ = override_builder.add(&format!("!**/{dir}/**"));
        let _ = override_builder.add(&format!("!{dir}"));
    }
    let _ = override_builder.add("!**/*.min.js");

    for pat in custom_ignores {
        let clean = pat.trim();
        if !clean.is_empty() {
            if clean.starts_with('!') {
                let _ = override_builder.add(clean);
            } else {
                let _ = override_builder.add(&format!("!{clean}"));
            }
        }
    }

    if let Ok(overrides) = override_builder.build() {
        builder.overrides(overrides);
    }

    let walker = builder.build();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                log::debug!("ignore walker skipped entry: {e}");
                stats.ignored_files_count += 1;
                continue;
            }
        };

        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            let path = entry.path();
            let path_str = path.to_string_lossy();

            if path_str.ends_with(".min.js") {
                stats.ignored_files_count += 1;
                continue;
            }

            if Language::from_path(path).is_some() {
                if is_binary_file(path) {
                    stats.binary_files_count += 1;
                    if let Ok(m) = entry.metadata() {
                        stats.binary_bytes_skipped += m.len();
                    }
                    if stats.skipped_samples.len() < 5 {
                        stats.skipped_samples.push(path.display().to_string());
                    }
                    continue;
                }
                files.push(path.to_path_buf());
            }
        }
    }

    Ok((files, stats))
}

/// End-to-end Tier 1 pass: discover files, parse each with Tree-Sitter in parallel,
/// and extract CodeUnits (skeletonized + compact + full text) for every top-level definition found.
pub fn parse_codebase(root: &Path) -> Result<Vec<CodeUnit>> {
    let files = discover_source_files(root)?;

    let mut all_units: Vec<CodeUnit> = files
        .into_par_iter()
        .filter_map(|file| {
            let lang = Language::from_path(&file)?;
            let source = std::fs::read_to_string(&file).ok()?;
            let mut dummy_id = 0usize;
            match extract_units(&file, lang, &source, &mut dummy_id) {
                Ok(units) => Some(units),
                Err(e) => {
                    log::warn!("failed to parse {}: {e}", file.display());
                    None
                }
            }
        })
        .flatten()
        .collect();

    // Sort files to preserve deterministic unit ordering across parallel workers
    all_units.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.start_line.cmp(&b.start_line)));

    // Reassign contiguous global unit IDs
    for (idx, unit) in all_units.iter_mut().enumerate() {
        unit.id = idx;
    }

    Ok(all_units)
}