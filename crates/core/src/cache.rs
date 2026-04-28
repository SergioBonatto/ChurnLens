use crate::metrics::FunctionMetrics;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Default)]
pub struct AnalysisCache {
    // Maps relative file path to (Git OID as string, List of FunctionMetrics)
    pub files: HashMap<String, (String, Vec<FunctionMetrics<'static>>)>,
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
            if let Ok(cache) = bincode::deserialize(&data) {
                return cache;
            }
        }
        AnalysisCache::default()
    }

    pub fn save(&self, cache: &AnalysisCache) -> Result<()> {
        let data = bincode::serialize(cache)?;
        fs::write(&self.cache_path, data)?;
        Ok(())
    }
}
