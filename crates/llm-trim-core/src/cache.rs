use crate::lang::Language;
use crate::secrets::scan_and_redact;
use crate::skeleton::extract_units;
use crate::unit::CodeUnit;
use crate::SkippedStats;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::SystemTime;

pub const DEFAULT_CACHE_FILENAME: &str = ".trim_cache";
pub const CURRENT_CACHE_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileCacheEntry {
    pub relative_path: String,
    pub mtime_secs: u64,
    pub mtime_nanos: u32,
    pub file_size: u64,
    pub content_hash: String,
    pub units: Vec<CodeUnit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheStore {
    pub version: u32,
    #[serde(default)]
    pub checksum: String,
    pub entries: HashMap<String, FileCacheEntry>,
}

impl Default for CacheStore {
    fn default() -> Self {
        Self {
            version: CURRENT_CACHE_VERSION,
            checksum: String::new(),
            entries: HashMap::new(),
        }
    }
}

impl CacheStore {
    pub fn compute_checksum(entries: &HashMap<String, FileCacheEntry>) -> String {
        let mut keys: Vec<&String> = entries.keys().collect();
        keys.sort();
        let mut hasher = Sha256::new();
        for k in keys {
            if let Some(entry) = entries.get(k) {
                hasher.update(entry.relative_path.as_bytes());
                hasher.update(&entry.mtime_secs.to_le_bytes());
                hasher.update(&entry.file_size.to_le_bytes());
                hasher.update(entry.content_hash.as_bytes());
                for u in &entry.units {
                    hasher.update(u.name.as_bytes());
                    hasher.update(u.full_text.as_bytes());
                }
            }
        }
        format!("{:x}", hasher.finalize())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => return Err(e.into()),
        };
        let store: CacheStore = match serde_json::from_str(&content) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("corrupt cache file JSON at {}, self-healing with clean cache: {e}", path.display());
                return Ok(CacheStore::default());
            }
        };

        if store.version != CURRENT_CACHE_VERSION {
            log::info!(
                "cache version mismatch (found {}, expected {}), re-indexing",
                store.version,
                CURRENT_CACHE_VERSION
            );
            return Ok(CacheStore::default());
        }

        if !store.checksum.is_empty() {
            let expected = Self::compute_checksum(&store.entries);
            if store.checksum != expected {
                log::warn!("cache checksum mismatch at {}, self-healing with clean cache", path.display());
                return Ok(CacheStore::default());
            }
        }

        Ok(store)
    }

    pub fn save(&mut self, path: &Path) -> Result<()> {
        self.version = CURRENT_CACHE_VERSION;
        self.checksum = Self::compute_checksum(&self.entries);
        let json = serde_json::to_string_pretty(self).context("serializing cache")?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                fs::create_dir_all(parent)?;
            }
        }
        let tmp_path = path.with_extension("tmp");
        fs::write(&tmp_path, json).context("writing temp cache file")?;
        fs::rename(&tmp_path, path).context("renaming temp cache file to target")?;
        Ok(())
    }
}

fn hash_content(source: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// High-performance incremental scanner: parses supported source files under `root`,
/// reusing cached `CodeUnit`s from `.trim_cache` when file mtime, size, and content hash
/// remain unchanged. Pre-redacts credentials prior to writing cache for zero leak risk.
pub fn parse_codebase_cached(
    root: &Path,
    cache_file_path: Option<&Path>,
    enabled: bool,
) -> Result<Vec<CodeUnit>> {
    let (units, _) = parse_codebase_cached_with_options(root, cache_file_path, enabled, &[], true)?;
    Ok(units)
}

/// Incremental scanner with custom ignore options and skipped stats.
pub fn parse_codebase_cached_with_options(
    root: &Path,
    cache_file_path: Option<&Path>,
    enabled: bool,
    custom_ignores: &[String],
    respect_trimignore: bool,
) -> Result<(Vec<CodeUnit>, SkippedStats)> {
    let (discovered_files, skipped_stats) =
        crate::discover_source_files_with_stats(root, custom_ignores, respect_trimignore)?;

    if !enabled {
        let units = crate::parse_codebase(root)?;
        return Ok((units, skipped_stats));
    }

    let default_cache_path = root.join(DEFAULT_CACHE_FILENAME);
    let cache_path = cache_file_path.unwrap_or(&default_cache_path);

    let mut cache_store = CacheStore::load(cache_path).unwrap_or_default();

    let mut all_units = Vec::new();
    let mut updated_cache = false;
    let mut discovered_rel_paths = HashSet::new();
    let mut dummy_id = 0usize;

    for file in &discovered_files {
        let rel_path = match file.strip_prefix(root) {
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
            Err(_) => file.to_string_lossy().replace('\\', "/"),
        };
        discovered_rel_paths.insert(rel_path.clone());

        let lang = match Language::from_path(file) {
            Some(l) => l,
            None => continue,
        };

        let metadata = match fs::metadata(file) {
            Ok(m) => m,
            Err(_) => continue,
        };

        let (mtime_secs, mtime_nanos) = match metadata.modified() {
            Ok(t) => match t.duration_since(SystemTime::UNIX_EPOCH) {
                Ok(d) => (d.as_secs(), d.subsec_nanos()),
                Err(_) => (0, 0),
            },
            Err(_) => (0, 0),
        };
        let file_size = metadata.len();

        // 1. Fast Cache Hit check: mtime + size match
        if let Some(entry) = cache_store.entries.get(&rel_path) {
            if entry.mtime_secs == mtime_secs
                && entry.mtime_nanos == mtime_nanos
                && entry.file_size == file_size
            {
                for mut unit in entry.units.clone() {
                    unit.file = file.clone();
                    all_units.push(unit);
                }
                continue;
            }
        }

        // Cache MISS: read file content
        let source = match fs::read_to_string(file) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let content_hash = hash_content(&source);

        // 2. Secondary Cache Hit check: content hash match (e.g. touch/git checkout without edits)
        if let Some(entry) = cache_store.entries.get(&rel_path) {
            if entry.content_hash == content_hash {
                let cached_units = entry.units.clone();
                let mut updated_entry = entry.clone();
                updated_entry.mtime_secs = mtime_secs;
                updated_entry.mtime_nanos = mtime_nanos;
                updated_entry.file_size = file_size;
                cache_store.entries.insert(rel_path.clone(), updated_entry);
                updated_cache = true;

                for mut unit in cached_units {
                    unit.file = file.clone();
                    all_units.push(unit);
                }
                continue;
            }
        }

        // 3. Full parse required
        match extract_units(file, lang, &source, &mut dummy_id) {
            Ok(mut units) => {
                // Pre-redact secrets in unit texts before saving to cache to prevent secret leaks
                for unit in &mut units {
                    let (clean_full, _) = scan_and_redact(&unit.full_text);
                    let (clean_compact, _) = scan_and_redact(&unit.compact_text);
                    let (clean_skel, _) = scan_and_redact(&unit.skeleton_text);
                    unit.full_text = clean_full;
                    unit.compact_text = clean_compact;
                    unit.skeleton_text = clean_skel;
                }

                let cache_entry = FileCacheEntry {
                    relative_path: rel_path.clone(),
                    mtime_secs,
                    mtime_nanos,
                    file_size,
                    content_hash,
                    units: units.clone(),
                };
                cache_store.entries.insert(rel_path, cache_entry);
                updated_cache = true;

                for mut unit in units {
                    unit.file = file.clone();
                    all_units.push(unit);
                }
            }
            Err(e) => log::warn!("failed to parse {}: {e}", file.display()),
        }
    }

    // Purge deleted files from cache
    let keys_to_remove: Vec<String> = cache_store
        .entries
        .keys()
        .filter(|k| !discovered_rel_paths.contains(*k))
        .cloned()
        .collect();

    if !keys_to_remove.is_empty() {
        for key in keys_to_remove {
            cache_store.entries.remove(&key);
        }
        updated_cache = true;
    }

    if updated_cache {
        if let Err(e) = cache_store.save(cache_path) {
            log::warn!("failed to save trim cache to {}: {e}", cache_path.display());
        }
    }

    // Reassign contiguous IDs
    for (idx, unit) in all_units.iter_mut().enumerate() {
        unit.id = idx;
    }

    Ok((all_units, skipped_stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_store_save_load() -> Result<()> {
        let temp_dir = std::env::temp_dir().join("llm_trim_test_cache_save_load_v3");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir)?;

        let cache_path = temp_dir.join(".trim_cache");
        let mut store = CacheStore::default();
        store.entries.insert(
            "src/main.rs".to_string(),
            FileCacheEntry {
                relative_path: "src/main.rs".to_string(),
                mtime_secs: 100,
                mtime_nanos: 200,
                file_size: 50,
                content_hash: "abcd".to_string(),
                units: vec![],
            },
        );

        store.save(&cache_path)?;
        assert!(cache_path.exists());

        let loaded = CacheStore::load(&cache_path)?;
        assert_eq!(loaded.version, CURRENT_CACHE_VERSION);
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries["src/main.rs"].content_hash, "abcd");
        assert!(!loaded.checksum.is_empty());

        let _ = fs::remove_dir_all(&temp_dir);
        Ok(())
    }

    #[test]
    fn test_cache_secret_pre_redaction() -> Result<()> {
        let temp_dir = std::env::temp_dir().join("llm_trim_test_cache_secret_redaction");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir)?;

        let secret_file = temp_dir.join("secrets.rs");
        fs::write(
            &secret_file,
            "pub fn get_key() -> &'static str {\n    const KEY = \"AIzaSyD-1234567890abcdefghijklmnopqrstuv\";\n    KEY\n}\n",
        )?;

        let cache_file = temp_dir.join(".trim_cache");
        let units = parse_codebase_cached(&temp_dir, Some(&cache_file), true)?;
        assert_eq!(units.len(), 1);

        let cache_content = fs::read_to_string(&cache_file)?;
        assert!(
            !cache_content.contains("AIzaSyD-1234567890abcdefghijklmnopqrstuv"),
            "Cache file must never store plaintext secrets"
        );
        assert!(cache_content.contains("[REDACTED: Google API Key]"));

        let _ = fs::remove_dir_all(&temp_dir);
        Ok(())
    }

    #[test]
    fn test_cache_corruption_tolerance() -> Result<()> {
        let temp_dir = std::env::temp_dir().join("llm_trim_test_corruption_v3");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir)?;

        let file = temp_dir.join("lib.rs");
        fs::write(&file, "pub fn check() {}")?;

        let cache_file = temp_dir.join(".trim_cache");
        fs::write(&cache_file, "{ invalid json garbage }")?;

        // Loading should fall back to default cache cleanly and not panic
        let units = parse_codebase_cached(&temp_dir, Some(&cache_file), true)?;
        assert_eq!(units.len(), 1);
        assert!(cache_file.exists());

        let loaded = CacheStore::load(&cache_file)?;
        assert_eq!(loaded.entries.len(), 1);

        let _ = fs::remove_dir_all(&temp_dir);
        Ok(())
    }
}
