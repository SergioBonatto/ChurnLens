# ChurnLens

ChurnLens is a high-performance code risk telemetry engine implemented in Rust. It correlates Abstract Syntax Tree (AST) complexity with historical change patterns to produce structured, normalized data for TypeScript and JavaScript repositories.

This engine is designed as a **data producer** for downstream systems such as SaaS platforms, CI pipelines, and autonomous agents.

---

## Scope

ChurnLens generates function-level telemetry including:

* **Structural Complexity:** Cyclomatic and Cognitive metrics via `tree-sitter`.
* **Historical Churn:** Volatility and bug-fix density via Git metadata.
* **Global Normalization:** Metrics scaled [0.0, 1.0] relative to the repository.
* **Risk Scoring:** Composite scores with exponential nesting penalties.
* **Percentile Ranking:** Global rank (0–100) for risk, churn, and complexity.

---

## Installation

### From crates.io
```bash
cargo install churnlens
```

### From source
Build the optimized binary from the workspace root:

```bash
cargo build --release
```

The binary will be available at `./target/release/churnlens`.

---

## Usage

```bash
churnlens [PATH] --sort file --limit 100 > report.json
```

* `PATH`: Root of the Git repository to analyze (defaults to `.`).
* `--sort`: `file`, `risk`, `churn_score`, `cognitive`, `cyclomatic`, or `loc`.
* `--limit`: Optional maximum number of functions in the report.

---

## Core Architecture

### 1. AST-Based Static Analysis
* **Tree Walk Parsing:** Uses `tree-sitter` to parse TypeScript, TSX, JavaScript, and JSX, then walks the syntax tree to extract function metrics.
* **Metrics:** Cyclomatic Complexity, Cognitive Complexity, Nesting Depth, and Lines of Code (LOC).

### 2. Git Metadata Mining
* **Incremental Traversal:** Uses `git2` and a local cache. The Git cache is invalidated when repository identity, branch, algorithm version, or ancestry validation no longer match.
* **Merge and Rename Handling:** Merge commits are compared against all parents. Rename detection is enabled and historical metrics are propagated from old paths to new paths.
* **Refined Churn Formula:**
  `churn_score = (modifications + (bug_fixes * 2)) * log10(authors + 1)`

### 3. Global Normalization
* **Outlier Protection:** If a metric's maximum is an extreme outlier (>3x p95), the denominator is capped at the 99th percentile to prevent score compression across the repository.
* **Percentile Ranks:** Provides immediate context on how a function compares to the rest of the codebase.

### 4. Risk Scoring
Final risk is computed using weighted base metrics amplified by nesting depth:
`Risk = BaseScore * (1.0 + (depth / 4)^2 * 0.20)`

**Base Score Weights:** 35% Cognitive, 30% Churn, 15% Cyclomatic, 10% LOC, 10% Authors.

---

## Output Contract (JSON)

ChurnLens produces a single, machine-consumable JSON document.

### Top-Level Structure
```json
{
  "schema_version": "string",
  "analysis": {
    "repository": "string (path)",
    "commit": "string",
    "branch": "string",
    "timestamp": "RFC3339"
  },
  "summary": {
    "total_functions": integer,
    "max_values": { "cyclomatic", "cognitive", "churn", "loc" },
    "distributions": { "risk_p95", "churn_p95", "cognitive_p95" }
  },
  "quality": {
    "status": "complete | partial",
    "git": { "available": true, "partial": false, "cache_reset": false, "processed_commits": 0 },
    "cache": { "enabled": true, "loaded": true, "saved": true, "ast_hits": 0, "ast_misses": 0 },
    "warnings": [],
    "skipped_files": []
  },
  "functions": [ ... ]
}
```

### Function Object
| Field | Type | Description |
| :--- | :--- | :--- |
| `id` | `string` | Stable identifier (`file:name:line`). |
| `name` | `string` | Function identifier or `<anonymous>`. |
| `file` | `string` | Relative path from repository root. |
| `line` | `u32` | Start line number. |
| `churn_score` | `f64` | Refined historical volatility score. |
| `normalized` | `object` | Fields scaled [0.0, 1.0] with outlier protection. |
| `risk` | `object` | `base_score`, `nesting_penalty`, and `final_score`. |
| `percentile` | `object` | Global rank (0.0 to 100.0) for risk, churn, and cognitive. |

### Data Quality
The `quality.status` field is `partial` when analysis completed but some data could not be collected. Examples include parser failures, unreadable files, Git mining errors, unsupported sort fields, or cache failures.

Consumers should treat `quality.status = "partial"` as a non-authoritative report unless their workflow explicitly accepts partial telemetry.

### Metric Semantics
* AST cache invalidation uses a stable hash of the current file contents, so dirty working-tree files are reparsed.
* Git churn is file-path based. Rename detection propagates historical metrics to the new path, but complex copy/split histories are not treated as semantic code identity tracking.
* Bug-fix commits are detected from word-like commit-message tokens such as `fix`, `bug`, `issue`, `close`, and `resolve`.
* Author identity uses Git author email when available, falling back to author name.
* Normalized values are capped at `1.0`.
* Percentile ranks use `0.0` for the lowest value and `100.0` for the highest value when at least two functions are present.

---

## Characteristics

* **Deterministic:** Output is consistent for the same repository and working-tree contents.
* **Performance:** Multi-threaded analysis using `Rayon`.
* **Non-Goals:** This tool does not provide interactive UI or human-readable text tables; it is strictly a JSON data provider.

---

## License

MIT License. See `LICENSE` for details.
