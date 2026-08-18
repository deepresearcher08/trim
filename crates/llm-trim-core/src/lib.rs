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

/// End-to-end Tier 1 pass: discover files, parse each with Tree-Sitter, and
/// extract CodeUnits (skeletonized + full text) for every top-level
/// definition found.
pub fn parse_codebase(root: &Path) -> Result<Vec<CodeUnit>> {
    let files = discover_source_files(root)?;
    let mut all_units = Vec::new();
    let mut next_id = 0usize;

    for file in files {
        let lang = match Language::from_path(&file) {
            Some(l) => l,
            None => continue,
        };
        let source = match std::fs::read_to_string(&file) {
            Ok(s) => s,
            Err(_) => continue, // skip binary / non-UTF8 files silently
        };
        match extract_units(&file, lang, &source, &mut next_id) {
            Ok(mut units) => all_units.append(&mut units),
            Err(e) => log::warn!("failed to parse {}: {e}", file.display()),
        }
    }

    Ok(all_units)
}