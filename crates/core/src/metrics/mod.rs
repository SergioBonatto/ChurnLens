use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionMetrics {
    pub id: String,
    pub name: String,
    pub file: String,
    pub line: u32,
    pub end_line: u32,
    pub body_hash: String,
    pub cyclomatic_complexity: u32,
    pub cognitive_complexity: u32,
    pub nesting_depth: u32,
    pub lines_of_code: u32,
    pub executable_statements: u32,
    pub is_hollow: bool,
    pub hollow_kind: String,
    pub comment_ratio: f64,
    pub placeholder_count: usize,
    pub has_docstring: bool,
    pub documentation_quality: String,
    pub identifier_verbosity: f64,
    pub times_modified: usize,
    pub bug_fix_commits: usize,
    pub authors_count: usize,
    pub authors: Option<Vec<String>>,
    pub churn: ChurnDetails,
    pub churn_score: f64,
    pub coverage: Option<CoverageMetrics>,
    pub coupling: CouplingMetrics,
    pub reachability: ReachabilityMetrics,
    pub normalized: Option<NormalizedMetrics>,
    pub risk: Option<RiskMetrics>,
    pub percentile: Option<PercentileMetrics>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedMetrics {
    pub cyclomatic: f64,
    pub churn: f64,
    pub churn_recent: f64,
    pub cognitive: f64,
    pub fan_in: f64,
    pub loc: f64,
    pub authors: f64,
    pub coverage_gap: Option<f64>,
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
    pub scoring_policy: ScoringPolicy,
    pub summary: SummaryStats,
    pub quality: AnalysisQuality,
    pub functions: Vec<FunctionMetrics>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ScoringPolicy {
    pub weights: Weights,
    pub thresholds: Thresholds,
    pub description: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Weights {
    pub cognitive: f64,
    pub cyclomatic: f64,
    pub churn: f64,
    pub churn_recent: f64,
    pub fan_in: f64,
    pub loc: f64,
    pub authors: f64,
    pub coverage_gap: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Thresholds {
    pub critical: f64,
    pub high: f64,
    pub medium: f64,
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
    pub project_stats: ProjectStats,
    pub coverage: Option<ProjectCoverage>,
    pub max_values: Option<MaxValues>,
    pub distributions: Option<Distributions>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectStats {
    pub total_unique_authors: usize,
    pub bus_factor: usize,
    pub tech_debt_density: f64,
    pub top_hotspots: Vec<Hotspot>,
    pub dead_code: DeadCodeStats,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeadCodeStats {
    pub unreachable_functions: usize,
    pub unreachable_loc: u32,
    pub safe_to_delete: usize,
    pub functions: Vec<DeadCodeFunction>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeadCodeFunction {
    pub id: String,
    pub name: String,
    pub file: String,
    pub line: u32,
    pub lines_of_code: u32,
    pub kind: String,
    pub safe_to_delete: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Hotspot {
    pub id: String,
    pub name: String,
    pub file: String,
    pub line: u32,
    pub risk_score: f64,
    pub churn_score: f64,
    pub cognitive_complexity: u32,
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
    pub authors: Vec<String>,
    pub churn_score: f64,
    pub last_modified: Option<String>,
    pub windows: ChurnWindows,
    pub velocity: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChurnDetails {
    pub score: f64,
    pub times_modified: usize,
    pub last_modified: Option<String>,
    pub windows: ChurnWindows,
    pub velocity: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChurnWindows {
    pub d7: ChurnWindow,
    pub d30: ChurnWindow,
    pub d90: ChurnWindow,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChurnWindow {
    pub modifications: usize,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageMetrics {
    pub available: bool,
    pub line_coverage: f64,
    pub branch_coverage: Option<f64>,
    pub covered_by: Vec<String>,
    pub risk_coverage_gap: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectCoverage {
    pub available: bool,
    pub project_line_coverage: f64,
    pub high_risk_uncovered: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CouplingMetrics {
    pub fan_in: usize,
    pub fan_out: usize,
    pub callers: Vec<String>,
    pub callees: Vec<String>,
    pub instability: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReachabilityMetrics {
    pub is_reachable: bool,
    pub kind: String,
}
