//! Continuous / incremental selection session manager ("Agent Memory" model).
//!
//! Preserves a budgeted "hot set" of recently accessed, active, or pinned code units
//! across multi-turn agent conversation steps.

use crate::budget::{BudgetPlan, Inclusion};
use crate::unit::CodeUnit;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_SESSION_FILENAME_PREFIX: &str = ".trim_session_";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub unit_name: String,
    pub file_path: String,
    pub access_count: usize,
    pub last_inclusion: String,
    pub last_accessed_timestamp: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionStore {
    pub session_id: String,
    /// unit_key ("relative_file::unit_name") -> SessionEntry
    pub hot_units: HashMap<String, SessionEntry>,
}

impl SessionStore {
    pub fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            hot_units: HashMap::new(),
        }
    }

    pub fn session_file_path(base_dir: &Path, session_id: &str) -> PathBuf {
        base_dir.join(format!("{}{}.json", DEFAULT_SESSION_FILENAME_PREFIX, session_id))
    }

    /// Load existing session state or create new empty session.
    pub fn load_or_create(base_dir: &Path, session_id: &str) -> Self {
        let path = Self::session_file_path(base_dir, session_id);
        if path.exists() {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(store) = serde_json::from_str::<SessionStore>(&content) {
                    return store;
                }
            }
        }
        Self::new(session_id)
    }

    /// Save current session hot-set state to disk.
    pub fn save(&self, base_dir: &Path) -> Result<()> {
        let path = Self::session_file_path(base_dir, &self.session_id);
        let json = serde_json::to_string_pretty(self).context("serializing session store")?;
        fs::write(&path, json).context("writing session store file")?;
        Ok(())
    }

    /// Helper to generate unique key for a code unit.
    pub fn unit_key(file: &Path, name: &str) -> String {
        format!("{}::{}", file.display().to_string().replace('\\', "/"), name)
    }

    /// Record units selected in the current budget plan into the session hot set.
    pub fn record_plan(&mut self, units: &[CodeUnit], plan: &BudgetPlan) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let unit_by_id: HashMap<usize, &CodeUnit> = units.iter().map(|u| (u.id, u)).collect();

        for planned in &plan.included {
            if let Some(unit) = unit_by_id.get(&planned.unit_id) {
                let key = Self::unit_key(&unit.file, &unit.name);
                let entry = self.hot_units.entry(key).or_insert_with(|| SessionEntry {
                    unit_name: unit.name.clone(),
                    file_path: unit.file.display().to_string().replace('\\', "/"),
                    access_count: 0,
                    last_inclusion: match planned.inclusion {
                        Inclusion::Full => "full".to_string(),
                        Inclusion::Compact => "compact".to_string(),
                        Inclusion::Skeleton => "skeleton".to_string(),
                    },
                    last_accessed_timestamp: now,
                });
                entry.access_count += 1;
                entry.last_accessed_timestamp = now;
            }
        }
    }

    /// Apply a relevance boost to units that are in the session hot set.
    pub fn apply_session_boost(
        &self,
        units: &[CodeUnit],
        scores: &mut HashMap<usize, f32>,
        boost_weight: f32,
    ) {
        if self.hot_units.is_empty() || boost_weight <= 0.0 {
            return;
        }

        for unit in units {
            let key = Self::unit_key(&unit.file, &unit.name);
            if let Some(entry) = self.hot_units.get(&key) {
                let boost = (boost_weight * (1.0 + (entry.access_count as f32).min(3.0) * 0.25)).min(3.0);
                if let Some(score) = scores.get_mut(&unit.id) {
                    *score += boost;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::UnitKind;
    use std::path::PathBuf;

    fn make_test_unit(id: usize, name: &str, file: &str) -> CodeUnit {
        CodeUnit {
            id,
            file: PathBuf::from(file),
            kind: UnitKind::Function,
            name: name.to_string(),
            doc_comment: None,
            signature: format!("fn {name}()"),
            full_text: format!("fn {name}() {{}}"),
            compact_text: format!("fn {name}() {{}}"),
            skeleton_text: format!("fn {name}() {{}}"),
            start_line: 1,
            end_line: 5,
            est_tokens_full: 20,
            est_tokens_compact: 15,
            est_tokens_skeleton: 5,
            references: vec![],
            call_sites: vec![],
        }
    }

    #[test]
    fn test_session_hot_set_persistence() -> Result<()> {
        let temp_dir = std::env::temp_dir().join("llm_trim_test_session_store");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir)?;

        let mut store = SessionStore::new("agent-turn-1");
        let units = vec![
            make_test_unit(0, "handleAuth", "src/auth.ts"),
            make_test_unit(1, "queryDb", "src/db.ts"),
        ];

        let plan = BudgetPlan {
            budget_tokens: 4000,
            used_tokens: 35,
            included: vec![crate::budget::PlannedUnit {
                unit_id: 0,
                inclusion: Inclusion::Full,
                tokens: 20,
                score: 5.0,
                skeleton_reason: None,
            }],
            excluded_unit_ids: vec![1],
            budget_exhausted_units: vec![],
            cannibalization_warnings: vec![],
        };

        store.record_plan(&units, &plan);
        store.save(&temp_dir)?;

        let loaded = SessionStore::load_or_create(&temp_dir, "agent-turn-1");
        assert_eq!(loaded.hot_units.len(), 1);

        let mut scores: HashMap<usize, f32> = [(0, 2.0), (1, 2.0)].into_iter().collect();
        loaded.apply_session_boost(&units, &mut scores, 1.5);

        assert!(scores[&0] > scores[&1], "Unit 0 in hot set should be boosted");

        let _ = fs::remove_dir_all(&temp_dir);
        Ok(())
    }
}
