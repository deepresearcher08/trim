use crate::lang::Language;
use crate::skeleton::extract_units;
use crate::unit::CodeUnit;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::SystemTime;

pub const DEFAULT_CACHE_FILENAME: &str = ".trim_cache";
pub const CURRENT_CACHE_VERSION: u32 = 1;

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
    pub entries: HashMap<String, FileCacheEntry>,
}

impl Default for CacheStore {
    fn default() -> Self {
        Self {
            version: CURRENT_CACHE_VERSION,
            entries: HashMap::new(),
        }
    }
}

impl CacheStore {
    pub fn load(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path).context("reading cache file")?;
        let store: CacheStore = serde_json::from_str(&content).context("parsing cache JSON")?;
        if store.version != CURRENT_CACHE_VERSION {
            log::info!(
                "cache version mismatch (found {}, expected {}), re-indexing",
                store.version,
                CURRENT_CACHE_VERSION
            );
            return Ok(CacheStore::default());
        }
        Ok(store)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self).context("serializing cache")?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                fs::create_dir_all(parent)?;
            }
        }
        fs::write(path, json).context("writing cache file")?;
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
/// remain unchanged.
pub fn parse_codebase_cached(
    root: &Path,
    cache_file_path: Option<&Path>,
    enabled: bool,
) -> Result<Vec<CodeUnit>> {
    if !enabled {
        return crate::parse_codebase(root);
    }

    let default_cache_path = root.join(DEFAULT_CACHE_FILENAME);
    let cache_path = cache_file_path.unwrap_or(&default_cache_path);

    let mut cache_store = CacheStore::load(cache_path).unwrap_or_default();
    let discovered_files = crate::discover_source_files(root)?;

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
            Ok(units) => {
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

    Ok(all_units)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    #[test]
    fn test_cache_store_save_load() -> Result<()> {
        let temp_dir = std::env::temp_dir().join("llm_trim_test_cache_save_load");
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

        let _ = fs::remove_dir_all(&temp_dir);
        Ok(())
    }

    #[test]
    fn test_parse_codebase_cached_lifecycle() -> Result<()> {
        let temp_dir = std::env::temp_dir().join("llm_trim_test_incremental_lifecycle");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir)?;

        let file1 = temp_dir.join("lib.rs");
        let mut f1 = File::create(&file1)?;
        writeln!(f1, "pub fn hello() {{ println!(\"hello\"); }}")?;

        let file2 = temp_dir.join("main.rs");
        let mut f2 = File::create(&file2)?;
        writeln!(f2, "fn main() {{ hello(); }}")?;

        let cache_file = temp_dir.join(".trim_cache");

        // 1. First parse: creates cache
        let units1 = parse_codebase_cached(&temp_dir, Some(&cache_file), true)?;
        assert_eq!(units1.len(), 2);
        assert!(cache_file.exists());

        // Read cache file entry count
        let store1 = CacheStore::load(&cache_file)?;
        assert_eq!(store1.entries.len(), 2);

        // 2. Second parse: cache hit (no change)
        let units2 = parse_codebase_cached(&temp_dir, Some(&cache_file), true)?;
        assert_eq!(units2.len(), 2);

        // 3. Modify file2
        {
            let mut f2_mod = File::create(&file2)?;
            writeln!(f2_mod, "fn main() {{ println!(\"modified\"); }}")?;
        }
        let units3 = parse_codebase_cached(&temp_dir, Some(&cache_file), true)?;
        assert_eq!(units3.len(), 2);

        // 4. Delete file1
        fs::remove_file(&file1)?;
        let units4 = parse_codebase_cached(&temp_dir, Some(&cache_file), true)?;
        assert_eq!(units4.len(), 1);
        assert_eq!(units4[0].name, "main");

        let store4 = CacheStore::load(&cache_file)?;
        assert_eq!(store4.entries.len(), 1);

        // 5. Test disabled cache
        let units_disabled = parse_codebase_cached(&temp_dir, Some(&cache_file), false)?;
        assert_eq!(units_disabled.len(), 1);

        let _ = fs::remove_dir_all(&temp_dir);
        Ok(())
    }
}
