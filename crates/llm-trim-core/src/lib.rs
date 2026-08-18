//! llm-trim-core
//!
//! Tier 1: Tree-Sitter based structural parsing + skeletonization.
//! Tier 3: budget-driven selection engine.
//!
//! Incremental caching (.trim_cache) avoids redundant AST re-parsing
//! across invocations by tracking file metadata and content hashes.
//!
//! Tier 2 (semantic ranking) lives in the sibling `llm-trim-rank` crate so
//! that ONNX Runtime stays an optional, swappable dependency — the core
//! parsing/skeletonization pipeline works with zero ML dependencies.

pub mod budget;
pub mod cache;
pub mod lang;
pub mod skeleton;
pub mod unit;

pub use budget::{render_payload, select_within_budget, BudgetPlan};
pub use cache::parse_codebase_cached;
pub use lang::Language;
pub use skeleton::extract_units;
pub use unit::{CodeUnit, UnitKind};

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// Walk `root`, respecting .gitignore, and return every source file whose
/// extension maps to a supported Language.
pub fn discover_source_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let walker = WalkBuilder::new(root)
        .hidden(false) // .gitignore already strips build artifacts; don't hide dotfiles like .github configs
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();

    for entry in walker {
        let entry = entry.context("walking directory tree")?;
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            let path = entry.path();
            if Language::from_path(path).is_some() {
                files.push(path.to_path_buf());
            }
        }
    }
    Ok(files)
}

use rayon::prelude::*;

/// End-to-end Tier 1 pass: discover files, parse each with Tree-Sitter in parallel,
/// and extract CodeUnits (skeletonized + full text) for every top-level definition found.
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