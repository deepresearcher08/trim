//! Persistent per-repository configuration (`trim.config.toml` or `trim.toml`).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const CONFIG_FILENAMES: &[&str] = &["trim.config.toml", "trim.toml", ".trim.toml"];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrimConfig {
    /// Default token budget if not specified on CLI
    pub budget: Option<usize>,
    /// Default intent description if not specified on CLI
    pub intent: Option<String>,
    /// Default ranker engine ("heuristic" or "onnx")
    pub ranker: Option<String>,
    /// Whether to automatically pull in direct caller/callee dependencies
    pub deps: Option<bool>,
    /// Whether to scan and redact credentials/secrets in payloads
    pub scan_secrets: Option<bool>,
    /// Whether to disable graph centrality (PageRank) scoring
    pub no_graph: Option<bool>,
    /// PageRank centrality boost weight multiplier (defaults to 0.5)
    pub graph_weight: Option<f32>,
    /// Glob patterns to ignore in addition to .gitignore
    pub ignore: Option<Vec<String>>,
    /// Custom cache file path
    pub cache_file: Option<String>,
}

impl TrimConfig {
    /// Look for configuration file in `root` or ancestors.
    pub fn discover_and_load(root: &Path) -> Option<(Self, PathBuf)> {
        let mut cur = if root.is_file() {
            root.parent()?.to_path_buf()
        } else {
            root.to_path_buf()
        };

        loop {
            for &name in CONFIG_FILENAMES {
                let candidate = cur.join(name);
                if candidate.is_file() {
                    if let Ok(config) = Self::load(&candidate) {
                        return Some((config, candidate));
                    }
                }
            }
            if !cur.pop() {
                break;
            }
        }
        None
    }

    /// Load and parse a TOML configuration file.
    pub fn load(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path).context("reading config file")?;
        let config: TrimConfig = toml::from_str(&content).context("parsing TOML configuration")?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_toml_config() -> Result<()> {
        let toml_str = r#"
budget = 6000
intent = "refactor auth logic"
ranker = "heuristic"
deps = true
scan_secrets = true
ignore = ["**/generated/**", "**/*.min.js"]
"#;
        let cfg: TrimConfig = toml::from_str(toml_str)?;
        assert_eq!(cfg.budget, Some(6000));
        assert_eq!(cfg.intent.as_deref(), Some("refactor auth logic"));
        assert_eq!(cfg.deps, Some(true));
        assert_eq!(cfg.scan_secrets, Some(true));
        assert_eq!(cfg.ignore.as_ref().map(|v| v.len()), Some(2));
        Ok(())
    }
}
