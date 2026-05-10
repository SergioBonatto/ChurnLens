use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionMetrics {
    pub id: String,
    pub name: String,
    pub file: String,
    pub line: u32,
    pub cyclomatic_complexity: u32,
    pub cognitive_complexity: u32,
    pub nesting_depth: u32,
    pub lines_of_code: u32,
    pub times_modified: usize,
    pub bug_fix_commits: usize,
    pub authors_count: usize,
    pub churn_score: f64,
    pub normalized: Option<NormalizedMetrics>,
    pub risk: Option<RiskMetrics>,
    pub percentile: Option<PercentileMetrics>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedMetrics {
    pub cyclomatic: f64,
    pub churn: f64,
    pub cognitive: f64,
    pub loc: f64,
    pub authors: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskMetrics {
    pub base_score: f64,
    pub nesting_penalty: f64,
    pub final_score: f64,
    pub level: String, // "low", "medium", "high", "critical"
    pub primary_driver: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PercentileMetrics {
    pub risk: f64,
    pub churn: f64,
    pub cognitive: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: String,
    pub analysis: AnalysisMetadata,
    pub summary: SummaryStats,
    pub quality: AnalysisQuality,
    pub functions: Vec<FunctionMetrics>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AnalysisMetadata {
    pub repository: String,
    pub commit: String,
    pub branch: String,
    pub timestamp: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AnalysisQuality {
    pub status: AnalysisStatus,
    pub git: GitAnalysisStatus,
    pub cache: CacheAnalysisStatus,
    pub warnings: Vec<AnalysisWarning>,
    pub skipped_files: Vec<SkippedFile>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisStatus {
    Complete,
    Partial,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitAnalysisStatus {
    pub available: bool,
    pub partial: bool,
    pub cache_reset: bool,
    pub processed_commits: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CacheAnalysisStatus {
    pub enabled: bool,
    pub loaded: bool,
    pub saved: bool,
    pub ast_hits: usize,
    pub ast_misses: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisWarning {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedFile {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SummaryStats {
    pub total_functions: usize,
    pub max_values: Option<MaxValues>,
    pub distributions: Option<Distributions>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MaxValues {
    pub cyclomatic: u32,
    pub cognitive: u32,
    pub churn: f64,
    pub loc: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Distributions {
    pub risk_p95: f64,
    pub churn_p95: f64,
    pub cognitive_p95: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChurnMetrics {
    pub times_modified: usize,
    pub bug_fix_commits: usize,
    pub authors_count: usize,
    pub churn_score: f64,
}
