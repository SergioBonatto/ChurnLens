use crate::metrics::FunctionMetrics;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

const CACHE_MAGIC: &[u8; 4] = b"CHRN";
const CACHE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Default)]
pub struct AnalysisCache {
    pub files: HashMap<String, (String, Vec<FunctionMetrics>)>,
    pub git_cache: HashMap<String, GitCacheEntry>,
    pub last_commit_oid: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct GitCacheEntry {
    pub times_modified: usize,
    pub bug_fix_commits: usize,
    pub authors: HashSet<String>,
}

#[derive(Serialize, Deserialize)]
struct VersionedCache {
    magic: [u8; 4],
    version: u32,
    data: AnalysisCache,
}

pub struct CacheManager {
    cache_path: PathBuf,
}

impl CacheManager {
    pub fn new(repo_path: &Path) -> Self {
        let cache_dir = repo_path.join(".churnlens");
        if !cache_dir.exists() {
            let _ = fs::create_dir_all(&cache_dir);
        }
        Self {
            cache_path: cache_dir.join("cache.bin"),
        }
    }

    pub fn load(&self) -> AnalysisCache {
        if let Ok(data) = fs::read(&self.cache_path) {
            if let Ok(versioned) = bincode::deserialize::<VersionedCache>(&data) {
                if &versioned.magic == CACHE_MAGIC && versioned.version == CACHE_VERSION {
                    return versioned.data;
                }
            }
        }
        AnalysisCache::default()
    }

    pub fn save(&self, cache: AnalysisCache) -> Result<()> {
        let versioned = VersionedCache {
            magic: *CACHE_MAGIC,
            version: CACHE_VERSION,
            data: cache,
        };
        let data = bincode::serialize(&versioned)?;
        fs::write(&self.cache_path, data)?;
        Ok(())
    }
}
