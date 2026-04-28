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
churnlens [PATH] > report.json
```

* `PATH`: Root of the Git repository to analyze (defaults to `.`).

---

## Core Architecture

### 1. AST-Based Static Analysis
* **Query-Based Parsing:** Uses `tree-sitter` declarative queries for high-performance extraction.
* **Metrics:** Cyclomatic Complexity, Cognitive Complexity, Nesting Depth, and Lines of Code (LOC).

### 2. Git Metadata Mining
* **Single-Pass Traversal:** $O(\text{commits} + \text{files})$ complexity using `git2`.
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
  "repository": "string (path)",
  "timestamp": "RFC3339",
  "summary": {
    "total_functions": integer,
    "max_values": { "cyclomatic", "cognitive", "churn", "loc" },
    "distributions": { "risk_p95", "churn_p95", "cognitive_p95" }
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

---

## Characteristics

* **Deterministic:** Output is consistent for a given repository state.
* **Performance:** Multi-threaded analysis using `Rayon`.
* **Non-Goals:** This tool does not provide interactive UI or human-readable text tables; it is strictly a JSON data provider.

---

## License

MIT License. See `LICENSE` for details.
