//! Behavioral relevance signals extracted from Git commit history and working tree state.
//!
//! Provides freshness recency scoring and co-edit clustering so that active or recently
//! touched code paths rank higher during context minimization.

use llm_trim_core::CodeUnit;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Default)]
pub struct GitSignals {
    /// Normalized recency score [0.0, 1.0] per relative file path
    pub file_recency: HashMap<String, f32>,
    /// Uncommitted modified / staged files
    pub uncommitted_files: HashMap<String, f32>,
    /// Co-edit frequency between file pairs
    pub co_edits: HashMap<(String, String), usize>,
}

impl GitSignals {
    /// Extract Git signals from repository at `root`. Fails gracefully (returns empty signals)
    /// if `root` is not a git repository or git binary is unavailable.
    pub fn from_repo(root: &Path) -> Self {
        let mut signals = GitSignals::default();

        // 1. Check uncommitted changes from git status
        if let Ok(output) = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(root)
            .output()
        {
            if output.status.success() {
                let status_str = String::from_utf8_lossy(&output.stdout);
                for line in status_str.lines() {
                    let trimmed = line.trim();
                    if trimmed.len() > 3 {
                        let path_part = &trimmed[3..].trim();
                        let clean_path = path_part.replace('\\', "/");
                        signals.uncommitted_files.insert(clean_path.clone(), 1.2);
                        signals.file_recency.insert(clean_path, 1.2);
                    }
                }
            }
        }

        // 2. Parse recent commits from git log
        if let Ok(output) = Command::new("git")
            .args(["log", "-n", "30", "--name-only", "--pretty=format:COMMIT_SEP"])
            .current_dir(root)
            .output()
        {
            if output.status.success() {
                let log_str = String::from_utf8_lossy(&output.stdout);
                let commits = log_str.split("COMMIT_SEP");

                for (commit_idx, commit_text) in commits.enumerate() {
                    let files: Vec<String> = commit_text
                        .lines()
                        .map(|l| l.trim().replace('\\', "/"))
                        .filter(|l| !l.is_empty())
                        .collect();

                    if files.is_empty() {
                        continue;
                    }

                    // Decay recency weight by commit age: commit 0 gets 1.0, commit 20 gets ~0.15
                    let recency_weight = (1.0 / (1.0 + (commit_idx as f32) * 0.15)).max(0.1);

                    for file in &files {
                        let entry = signals.file_recency.entry(file.clone()).or_insert(0.0);
                        *entry = entry.max(recency_weight);
                    }

                    // Track co-edit pairs
                    for i in 0..files.len() {
                        for j in (i + 1)..files.len() {
                            let pair = if files[i] < files[j] {
                                (files[i].clone(), files[j].clone())
                            } else {
                                (files[j].clone(), files[i].clone())
                            };
                            *signals.co_edits.entry(pair).or_insert(0) += 1;
                        }
                    }
                }
            }
        }

        signals
    }

    /// Calculate recency score for a file path.
    pub fn get_file_recency(&self, root: &Path, file_path: &Path) -> f32 {
        let rel_path = match file_path.strip_prefix(root) {
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
            Err(_) => file_path.to_string_lossy().replace('\\', "/"),
        };

        if let Some(&score) = self.file_recency.get(&rel_path) {
            return score;
        }

        for (k, &v) in &self.file_recency {
            if rel_path.ends_with(k) || k.ends_with(&rel_path) {
                return v;
            }
        }

        0.0
    }

    /// Apply git freshness / recency boost to unit scores.
    pub fn apply_git_boost(
        &self,
        root: &Path,
        units: &[CodeUnit],
        scores: &mut HashMap<usize, f32>,
        weight: f32,
    ) {
        if weight <= 0.0 || self.file_recency.is_empty() {
            return;
        }

        for unit in units {
            let recency = self.get_file_recency(root, &unit.file);
            if recency > 0.0 {
                let boost = (weight * recency * 1.5).min(3.0);
                if let Some(s) = scores.get_mut(&unit.id) {
                    *s += boost;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_git_signals_safe_fallback_non_git() {
        let temp_dir = std::env::temp_dir().join("llm_trim_test_non_git_signals");
        let _ = std::fs::create_dir_all(&temp_dir);
        let signals = GitSignals::from_repo(&temp_dir);
        let recency = signals.get_file_recency(&temp_dir, &PathBuf::from("nonexistent.rs"));
        assert_eq!(recency, 0.0);
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
