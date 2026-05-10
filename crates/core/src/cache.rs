use crate::metrics::FunctionMetrics;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

const CACHE_MAGIC: &[u8; 4] = b"CHRN";
const CACHE_VERSION: u32 = 2;
pub const GIT_ALGORITHM_VERSION: u32 = 2;

#[derive(Serialize, Deserialize, Default)]
pub struct AnalysisCache {
    pub files: HashMap<String, FileCacheEntry>,
    pub git_cache: HashMap<String, GitCacheEntry>,
    pub last_commit_oid: Option<String>,
    pub git_metadata: Option<GitCacheMetadata>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct FileCacheEntry {
    pub content_hash: String,
    pub functions: Vec<FunctionMetrics>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct GitCacheEntry {
    pub times_modified: usize,
    pub bug_fix_commits: usize,
    pub authors: HashSet<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct GitCacheMetadata {
    pub repository_path: String,
    pub branch: String,
    pub head_oid: String,
    pub algorithm_version: u32,
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
    pub fn new(repo_path: &Path) -> Result<Self> {
        let cache_dir = repo_path.join(".churnlens");
        fs::create_dir_all(&cache_dir)?;
        Ok(Self {
            cache_path: cache_dir.join("cache.bin"),
        })
    }

    pub fn load(&self) -> Result<AnalysisCache> {
        let data = match fs::read(&self.cache_path) {
            Ok(data) => data,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(AnalysisCache::default()),
            Err(err) => return Err(err.into()),
        };

        let versioned = bincode::deserialize::<VersionedCache>(&data)?;
        if &versioned.magic != CACHE_MAGIC || versioned.version != CACHE_VERSION {
            return Err(anyhow!("Unsupported cache format"));
        }

        Ok(versioned.data)
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
