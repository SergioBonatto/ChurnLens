# ChurnLens: High-Performance Code Telemetry (WIP)

ChurnLens is a specialized static analysis engine designed to quantify technical debt and stability risks in TypeScript and JavaScript repositories. It correlates Abstract Syntax Tree (AST) complexity with historical Git metadata to identify high-risk hotspots.

## Core Architecture

The engine is implemented in Rust, prioritizing zero-copy operations and data parallelism to handle large-scale monorepos with minimal memory overhead.

### 1. Static Analysis Engine (AST)
* **Query-Based Parsing:** Utilizes `tree-sitter` with declarative S-expression queries rather than imperative visitor patterns. This reduces pointer-chasing and leverages the underlying C-engine's optimized search.
* **Zero-Copy Traversal:** Metrics extraction utilizes Rust lifetimes and `Cow<'a, str>` to reference source buffers directly, significantly reducing heap allocations during the analysis of large files.
* **Complexity Metrics:**
    * **Cyclomatic Complexity:** Measures linearly independent paths via AST decision points.
    * **Cognitive Complexity:** Implements a nesting-aware metric that penalizes deeply branched logic, providing a more accurate representation of maintainability.

### 2. Git Metadata Mining
* **Single-Pass RevWalk:** Performs a single traversal of the repository history ($O(\text{Commits} + \text{Files})$). Metadata is aggregated into a hash-mapped cache, eliminating the $O(N \times M)$ bottleneck of per-function history queries.
* **Resource Pooling:** A single `git2::Repository` handle is opened and passed by reference across worker threads to minimize I/O overhead.

### 3. I/O and Concurrency
* **Parallel Pipeline:** Uses `Rayon` for work-stealing parallelism. File-system walking, AST parsing, and Git mining are executed concurrently.
* **Intelligent Discovery:** Integrates the `ignore` crate to natively respect `.gitignore` and `.ignore` files, ensuring only relevant source files are processed.

### 4. High-Throughput Extensions (Phase 3)
* **Incremental Analysis:** Persistence layer utilizing Git OID tracking to bypass redundant processing of unchanged files.
* **Streaming Telemetry:** Implements NDJSON (Newline Delimited JSON) output, enabling real-time metric consumption and $O(1)$ memory scaling relative to repository size.
* **Hardened Error Handling:** Zero-panic architecture with exhaustive error propagation and graceful degradation for malformed source inputs.

### 5. Advanced Infrastructure (Phase 4)
* **Asynchronous Reporting:** Decoupled I/O using a channel-based NDJSON streamer with `BufWriter` for minimal syscall overhead.
* **Persistent Cache Management:** Version-aware binary cache with automated invalidation and atomic signal-aware persistence.
* **Query Memoization:** Pre-compiled AST queries shared via thread-safe contexts to eliminate redundant CPU cycles.
* **Zero-Copy Optimization:** Leverages `Rayon` scoped threads to maintain source buffer lifetimes, eliminating heap allocations for metrics.

## Installation

Build the optimized binary from the workspace root:

```bash
cargo build --release
```

## Usage

```bash
# Analyze repository and output telemetry to JSON
./target/release/churnlens [PATH] --output report.json

# Filter by churn score and limit output
./target/release/churnlens . --sort churn_score --limit 50
```

### CLI Configuration

| Argument | Description | Default |
| :--- | :--- | :--- |
| `path` | Target directory for analysis. | `.` |
| `--output` | Destination path for the report. | `stdout` |
| `--sort` | Metric: `churn_score`, `complexity`, `modifications`. | `churn_score` |
| `--limit` | Result set truncation. | `20` |

## Heuristics

The `churn_score` is a weighted metric used to identify the intersection of instability and complexity:

$$churn\_score = \frac{m \times b}{a}$$

* **m**: Modification frequency.
* **b**: Density of bug-fix commits (identified via commit message heuristics).
* **a**: Contributor count (to identify knowledge silos or fragmentation).

## License

MIT License. See `LICENSE` for details.
