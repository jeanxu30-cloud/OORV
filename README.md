# OORV — Replication Package

It contains the complete source code of the OORV prototype, all test specifications,
benchmark scripts, and supporting data needed to reproduce every experimental result
reported in the paper.

---

## Table of Contents

- [OORV — Replication Package](#oorv--replication-package)
  - [Table of Contents](#table-of-contents)
  - [Repository Structure](#repository-structure)
  - [Build](#build)
    - [Running a specification manually](#running-a-specification-manually)
  - [Quick Verification](#quick-verification)
    - [`bench/run_smoke_stats.sh` — RQ1 timing smoke test](#benchrun_smoke_statssh--rq1-timing-smoke-test)
    - [`bench/verify_artifact.sh` — end-to-end artifact check](#benchverify_artifactsh--end-to-end-artifact-check)
  - [Reproducing RQ2 — Performance Sweeps](#reproducing-rq2--performance-sweeps)
    - [Step 1 — Run the benchmark suite](#step-1--run-the-benchmark-suite)
    - [Step 2 — Prepare plot data](#step-2--prepare-plot-data)
    - [Step 3 — Validate completeness](#step-3--validate-completeness)
    - [Running a single workload point](#running-a-single-workload-point)
  - [Rail-Transit Case Study](#rail-transit-case-study)
  - [Shared-Fragment Comparison (RTLola Baseline)](#shared-fragment-comparison-rtlola-baseline)
  - [Windows Peak-Memory Measurement](#windows-peak-memory-measurement)

---

## Repository Structure

| Path | Description |
|---|---|
| `core/` | Core library: parser (`oorv.pest`), AST, IR, static analyses (identifier resolution, pacing/value check, dependency order, storage bounds), runtime scheduler and expression evaluator |
| `oorv/` | Command-line monitor driver — accepts `.oorv` specifications and CSV event traces |
| `oorv2c/` | Experimental C-backend |
| `test/` | Specifications and CSV traces for all paper experiments: `test01`, `test02`, `rail_transit`, and the `TrafficLight` running example |
| `bench/` | Benchmark and verification harness: smoke-test script, end-to-end verifier, RQ2 sweep suite, Windows memory scripts, and shared-fragment comparison assets |

---

## Build

Prerequisites: [Rust stable toolchain](https://rustup.rs) (edition 2021, tested on 1.76+), Python 3.9+ (standard library only, needed for benchmark scripts), Bash 4+ (Linux/macOS/WSL).

From the repository root:

```bash
cargo build --release
```

This produces the main monitor executable:

```
target/release/oorv        # Linux / macOS
target\release\oorv.exe    # Windows
```

### Running a specification manually

The general invocation pattern is:

```bash
# Linux / macOS
./target/release/oorv <spec.oorv> --offline relative --csv-in <trace.csv> --verbosity <level>

# Windows
.\target\release\oorv.exe <spec.oorv> --offline relative --csv-in <trace.csv> --verbosity <level>
```

---

## Quick Verification

Two entry-point scripts cover the baseline reproducibility check:

### `bench/run_smoke_stats.sh` — RQ1 timing smoke test

Builds the project (if needed), runs `test01` and `test02`, and writes
`@@`-prefixed timing summary lines to `bench/results/`:

```bash
bash bench/run_smoke_stats.sh
```

Expected output files:

| File | Contents |
|---|---|
| `bench/results/test01_stats_summary.txt` | Timing lines (`@@TIME_PROC_AVG`, `@@TIME_THROUGHPUT`) for `test01` |
| `bench/results/test02_stats_summary.txt` | Timing lines for `test02` |

Pre-generated reference outputs are committed in `bench/results/` for comparison.

### `bench/verify_artifact.sh` — end-to-end artifact check

The fastest reviewer-facing validation path. It checks:

- the Cargo offline build and warning-clean test suite
- the shipped example monitors including the rail-transit integration smoke
- smoke statistics for `test01` and `test02`
- a small RQ2 workload run
- RQ2 manifest and plot-input validity
- Windows memory metric files (if present)
- the optional RTLola shared-fragment trigger smoke (when `rtlola-cli` is on `PATH`)
- manuscript build (when `latexmk` is available)

```bash
bash bench/verify_artifact.sh
```

Use `--require-windows-memory` to fail when no Windows memory CSVs are present,
or `--skip-paper` to skip LaTeX checks.

---

## Reproducing RQ2 — Performance Sweeps

RQ2 measures how per-event latency and throughput scale across nine synthetic workload families:

- **object-cardinality sweep** — fixed event-driven pairwise constraints, varying object count
- **constraint-count sweep** — fixed monitored object count, varying constraint count
- **history-depth sweep** — fixed world-level workload, varying `last`/`prev` depth
- **periodic-mix sweep** — fixed event-driven workload, varying periodic constraint count
- **mixed history+periodic sweep** — simultaneous temporal features under stress
- **bursty-trace sweep** — clustered timestamps, fixed hot-set size
- **rotating hot-set sweep** — expanding active-object window under fixed burst structure
- **long-run soak sweep** — trace lengths up to 25,600 events
- **object-by-rule matrix** — 49-cell interaction grid (7 object levels × 7 constraint levels)

### Step 1 — Run the benchmark suite

**Full paper sweep** :

```bash
bash bench/rq2/run_showcase_suite.sh --profile paper_longhaul
```

**Lightweight demo sweep** :

```bash
bash bench/rq2/run_demo_sweep.sh
```

Both scripts populate `bench/results/rq2/showcase_manifest.tsv` with one row per workload run, and write per-family plot TSVs consumed directly by the manuscript-side `pgfplots` figures.

### Step 2 — Prepare plot data

```bash
python3 bench/rq2/prepare_plot_data.py bench/results/rq2/showcase_manifest.tsv
```

### Step 3 — Validate completeness

```bash
python3 bench/rq2/verify_rq2_outputs.py bench/results/rq2/showcase_manifest.tsv
```

```bash
python3 bench/rq2/verify_rq2_outputs.py bench/results/rq2/showcase_manifest.tsv \
  --write-summary bench/results/rq2/showcase_manifest_verification.json
```

### Running a single workload point

```bash
bash bench/rq2/run_single_workload.sh --objects 48 --constraints 12 --events 1600 --history-depth 8 --periodic-constraints 8 --burst-size 8 --hotset-size 12 --phase-length 128 --label demo_o048_c012_complex
```

---

## Rail-Transit Case Study

`test/rail_transit.oorv` is an integration sanity check that combines all language mechanisms in one recognizable CPS specification: class-based structure, dynamic object collections, quantified cross-object constraints, mixed activation, bounded history, and graded alarms.

```bash
# Linux / macOS
./target/release/oorv test/rail_transit.oorv --offline relative --csv-in test/rail_transit.csv

# Windows
.\target\release\oorv.exe .\test\rail_transit.oorv --offline relative --csv-in .\test\rail_transit.csv
```

A clean run exits with code 0 and prints alarm summaries to stdout. No separate latency or memory claim is attached to this case.

---

## Shared-Fragment Comparison (RTLola Baseline)

`bench/shared_fragment/` contains a single semantically aligned shared fragment (a dynamic pairwise-distance check) encoded in both OORV and RTLola. The comparison is intentionally narrow: it documents identity handling, history access, activation, and pair flattening differences between the two systems on the same logical scenario, and is used as a fairness-calibration seed rather than a broad competition benchmark.

To regenerate the Markdown and LaTeX comparison tables from `comparison_manifest.tsv`:

```bash
bash bench/shared_fragment/render_comparison_summary.sh
```

Output files written: `comparison_summary.md`, `comparison_summary.tex`, and `paper_fragment_fairness.tex`.

To run the cross-tool trigger smoke check (requires `rtlola-cli` on `PATH`):

```bash
bash bench/shared_fragment/verify_rtlola_fragment.sh
```

---

## Windows Peak-Memory Measurement

Single-run collection:

```powershell
powershell -ExecutionPolicy Bypass -File bench\rq2\run_windows_peak_private_memory.ps1 `
    -Spec test\test01.oorv -CsvIn test\test01.csv -Label smoke_test01
```

Suite-level collection across RQ2 workloads:

```powershell
powershell -ExecutionPolicy Bypass -File bench\rq2\run_windows_memory_suite.ps1 `
    -SuiteLabel rq2_windows_memory_representative -All
```

Validate the output:

```bash
python3 bench/rq2/verify_memory_metrics.py \
    bench/results/rq2/windows_memory/<label>/memory_metrics.tsv
```

---