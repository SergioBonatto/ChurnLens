# Uchikomi

Uchikomi is a high-performance, deterministic telemetry engine written in Rust. It serves as the **ground truth** for autonomous AI refactoring agents by correlating Abstract Syntax Tree (AST) complexity with historical change patterns (Git).

Unlike traditional human-centric linters, Uchikomi produces structured, normalized, machine-first JSON designed to drive iterative automation loops, allowing agents to prioritize, execute, and validate code improvements.

---

## Scope

Uchikomi processes codebases to generate function-level telemetry, featuring:

* **Multi-Language AST Analysis:** Extensible parsing for Rust, TypeScript/JavaScript, and C via `tree-sitter`.
* **Historical Churn:** Line-level attribution, volatility tracking, and bug-fix density via `libgit2`.
* **Global Normalization & Outlier Protection:** Metrics scaled [0.0, 1.0] with logarithmic scaling and cap thresholds to handle "God Functions".
* **Risk Scoring:** Multi-factored risk assessment amplified by non-recursive nesting depth and fan-in multipliers.
* **Telemetry Convergence:** Fast feedback loops verified by stable function body hashing.

---

## Installation

### From crates.io

```bash
cargo install uchikomi

```

### From source

Build the optimized binary from the workspace root:

```bash
cargo build --release

```

The binary will be available at `./target/release/uchikomi`.

---

## Usage

```bash
uchikomi [PATH] --sort risk --limit 100 > report.json

```

* `PATH`: Root of the Git repository to analyze (defaults to `.`).
* `--sort`: `file`, `risk`, `churn_score`, `cognitive`, `cyclomatic`, or `loc`.
* `--limit`: Optional maximum number of functions in the report.

---

## Core Architecture & Module Breakdown

### 1. Static Analysis Layer: AST Engine (`ast/`)

Uchikomi employs a stack-based, non-recursive tree traversal engine (`ComplexityEngine`) that manages an isolated `function_stack` to decouple metrics of nested functions from their parents.

#### Language Targets & Complexity Rules

* **Rust (`rust.rs`)**: Targets `function_item` and `method_declaration`. Evaluates complexity via `if_expression`, `match_expression`, `for_expression`, `while_expression`, `match_arm`, and the `?` operator.
* **TypeScript / JavaScript (`typescript.rs`)**: Targets `function_declaration`, `arrow_function`, and `method_definition`. Evaluates complexity via `if`, `for`, `while`, `do`, `case`, `catch`, `&&`, `||`, and `?`. Implements name-resolution heuristics for anonymous functions assigned to variables.
* **C (`c.rs`)**: Targets `function_definition` and evaluates standard C control flow structures.

#### Body Quality Indicators

* **`body_hash`**: A stable, 128-bit `XXHash3` execution hash of the exact function body. Used by autonomous reviewers to verify state transformation post-refactor.
* **Executable Statements**: Accurate statement counting (returns, declarations, expressions, assignments, calls, and control flow). Detects *Hollow Functions* (`none`, `empty`, or `comment_only`).
* **Documentation & Identifiers**: Analyzes leading RustDoc/JSDoc structures to score `documentation_quality` (`missing`, `sparse`, or `adequate`), and computes average identifier lengths (`identifier_verbosity`).

### 2. Historical Analysis Layer: Git Analyzer (`git/`)

Leverages `libgit2` for high-fidelity repository mining with precise line-to-AST mapping via `DiffHunk` validation.

* **Incremental Traversal:** Uses local cached OIDs inside `.uchikomi/cache.bin` to process only new commits since the last analysis run.
* **Merge and Rename Tracking:** Compares merge commits against all parents to preserve attribution. Historical metrics are explicitly propagated across file renames.
* **Refined Churn Formula:**

$$\text{churn\_score} = (\text{modifications} + (\text{bug\_fixes} \times 2)) \times \log_{10}(\text{authors} + 1)$$


* **Velocity Metrics:** Tracks code acceleration by comparing the 7-day modification rate against the 90-day baseline, classifying files into `accelerating` (ratio > 1.25), `cooling` (ratio < 0.75), or `stable`.

### 3. Persistence Layer (`cache.rs`)

Ensures rapid feedback loops for iterative agents. The cache binary starts with magic bytes `CHRN` (`0x4348524E`) + `CACHE_VERSION`. It is automatically invalidated on shifts in repository root, branch head OID, file hash mismatches, or modifications to the `bug_fix_patterns` configuration.

### 4. Global Normalization & Outlier Protection

To prevent extreme outliers ("God Functions") from compressing the rest of the repository's metrics, Uchikomi applies strict outlier capping:

* If a maximum value exceeds $3\times$ the 95th percentile ($p95$), the denominator is capped at the 99th percentile ($p99$).
* All metrics are then globally scaled between $[0.0, 1.0]$ using logarithmic smoothing:

$$\text{normalized} = \frac{\ln(1 + \text{value})}{\ln(1 + \text{cap})}$$



### 5. Risk Scoring Framework

The engine derives a multi-factored risk profile:


$$\text{FinalRisk} = \text{BaseScore} \times \text{NestingPenalty} \times \text{FanInMultiplier}$$

* **Base Score Weights:** Cognitive Complexity (35%), Historical Churn (30%), Cyclomatic Complexity (15%), Lines of Code (10%), Unique Authors (10%).
* **Nesting Penalty:** $\displaystyle 1.0 + \left(\frac{\text{max\_depth}}{4}\right)^2 \times 0.20$
* **Fan-In Multiplier:** $1.0 + (\text{normalized\_fan\_in} \times 0.25)$
* **Primary Driver:** The telemetry object identifies the exact metric that contributed most heavily to the `BaseScore` to give downstream agents a clear target.

---

## Output Contract (JSON Schema Top-Level)

Uchikomi produces a single, machine-consumable JSON document optimized for pipeline ingestion.

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
    "project_stats": {
      "total_unique_authors": integer,
      "bus_factor": integer,
      "tech_debt_density": number,
      "top_hotspots": [],
      "dead_code": {
        "unreachable_private": integer,
        "unreachable_export": integer,
        "functions": [
          {
            "id": "string",
            "name": "string",
            "file": "string",
            "line": integer,
            "lines_of_code": integer,
            "kind": "string"
          }
        ]
      }
    },
    "coverage": null,
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
  "functions": [
    {
      "id": "string",
      "name": "string",
      "file": "string",
      "line": 42,
      "end_line": 88,
      "body_hash": "string (xxhash3_128)",
      "executable_statements": 24,
      "is_hollow": false,
      "hollow_kind": "none",
      "comment_ratio": 0.15,
      "placeholder_count": 0,
      "has_docstring": true,
      "documentation_quality": "adequate",
      "identifier_verbosity": 12.4,
      "churn": { "score": 4.2, "velocity": "stable" },
      "coupling": { "fan_in": 3, "fan_out": 5, "instability": 0.625 },
      "reachability": { "kind": "exported" },
      "churn_score": 4.2,
      "normalized": { "cognitive": 0.32, "churn": 0.45 },
      "risk": { "base_score": 0.38, "nesting_penalty": 1.05, "final_score": 0.40, "primary_driver": "cognitive" },
      "percentile": { "risk": 84.5, "churn": 72.1, "cognitive": 91.0 }
    }
  ]
}

```

---

## Configuration (`uchikomi.toml`)

Configure custom analysis properties at the repository root:

```toml
[git]
# Custom regex for bug-fix identification
bug_fix_patterns = ["(?i)\\bfix(?:e[sd])?\\b", "JIRA-[0-9]+"]

```

When `bug_fix_patterns` is omitted, Uchikomi defaults to its core built-in tokens: `fix`, `bug`, `issue`, `close`, and `resolve`.

---

## Characteristics & Operational Constraints

* **Autonomous Refactoring Ecosystem Ecosystem-Ready:** Designed specifically to power the *Analyze $\rightarrow$ Augment $\rightarrow$ Refactor $\rightarrow$ Validate $\rightarrow$ Converge* automation pipeline loop.
* **Deterministic:** Output is strictly identical for the same input repository state, execution configuration, and engine version.
* **Performance-First Engine:** Uses `Rayon` for safe multi-threaded parsing traversal and `memmap2` for zero-copy file reads of source files $\ge$ 1 MiB.
* **Machine-First Constraint:** No human-readable text tables or Markdown layouts are exposed; output is strictly JSON data payloads for pipeline automation.
* **Best-Effort Limits:** Static reachability, fan-in mappings, and coupling bounds are evaluated purely on supported AST nodes; dynamic dispatch, runtime reflections, or meta-programming elements are intentionally not resolved.

---

## License

MIT License. See `LICENSE` for details.
