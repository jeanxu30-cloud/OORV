#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"
tool_dir="${repo_root}/tool"
paper_dir="${repo_root}/paper"
results_dir="${tool_dir}/bench/results/artifact_verify"

skip_paper="false"
require_windows_memory="false"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-paper)
            skip_paper="true"
            shift
            ;;
        --require-windows-memory)
            require_windows_memory="true"
            shift
            ;;
        *)
            echo "unknown argument: $1" >&2
            echo "usage: $0 [--skip-paper] [--require-windows-memory]" >&2
            exit 1
            ;;
    esac
done

mkdir -p "${results_dir}"

note() {
    printf '[artifact-verify] %s\n' "$*"
}

if ! command -v python3 >/dev/null 2>&1; then
    echo "python3 is required for artifact verification" >&2
    exit 1
fi

extract_time_lines() {
    local input="$1"
    local output="$2"

    if command -v rg >/dev/null 2>&1; then
        rg '@@TIME_' "${input}" > "${output}"
    else
        grep '@@TIME_' "${input}" > "${output}"
    fi
}

require_time_summary() {
    local summary="$1"
    if ! grep -q '@@TIME_PROC_AVG' "${summary}"; then
        echo "missing @@TIME_PROC_AVG in ${summary}" >&2
        exit 1
    fi
    if ! grep -q '@@TIME_THROUGHPUT' "${summary}"; then
        echo "missing @@TIME_THROUGHPUT in ${summary}" >&2
        exit 1
    fi
}

note "checking vendored Cargo dependency snapshot"
if [[ ! -f "${tool_dir}/.cargo/config.toml" ]]; then
    echo "missing Cargo source replacement config: ${tool_dir}/.cargo/config.toml" >&2
    exit 1
fi
if [[ ! -d "${tool_dir}/vendor" ]]; then
    echo "missing vendored dependency directory: ${tool_dir}/vendor" >&2
    exit 1
fi
vendor_crates="$(find "${tool_dir}/vendor" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d '[:space:]')"
if [[ "${vendor_crates}" -eq 0 ]]; then
    echo "vendored dependency directory is empty: ${tool_dir}/vendor" >&2
    exit 1
fi
(
    cd "${tool_dir}"
    cargo metadata --offline --locked --format-version 1 --no-deps \
        > "${results_dir}/cargo_metadata_offline.json"
)
python3 - "${results_dir}/cargo_metadata_offline.json" <<'PY'
import json
import sys

metadata_path = sys.argv[1]
with open(metadata_path, "r", encoding="utf-8") as handle:
    metadata = json.load(handle)

packages = {package["name"] for package in metadata.get("packages", [])}
expected = {"oorv-core", "oorv", "oorv2c"}
if packages != expected:
    missing = sorted(expected - packages)
    extra = sorted(packages - expected)
    raise SystemExit(
        f"unexpected OORV workspace package set in {metadata_path}: "
        f"missing={missing}, extra={extra}"
    )
PY

note "running warning-clean Rust workspace tests"
(
    cd "${tool_dir}"
    cargo test --workspace --offline --locked --release 2>&1 | tee "${results_dir}/cargo_test_workspace_release.log"
)
if grep -q '^warning:' "${results_dir}/cargo_test_workspace_release.log"; then
    echo "Rust workspace tests emitted warnings; see ${results_dir}/cargo_test_workspace_release.log" >&2
    exit 1
fi

note "running shipped monitor sanity checks"
(
    cd "${tool_dir}"
    ./target/release/oorv test/test01.oorv \
        --offline relative \
        --csv-in test/test01.csv \
        --verbosity warnings \
        > "${results_dir}/test01.log" 2>&1
    ./target/release/oorv test/test02.oorv \
        --offline relative \
        --csv-in test/test02.csv \
        --verbosity warnings \
        > "${results_dir}/test02.log" 2>&1
    ./target/release/oorv test/rail_transit.oorv \
        --offline relative \
        --csv-in test/rail_transit.csv \
        --verbosity warnings \
        > "${results_dir}/rail_transit.log" 2>&1
)
extract_time_lines "${results_dir}/test01.log" "${results_dir}/test01_time_summary.txt"
extract_time_lines "${results_dir}/test02.log" "${results_dir}/test02_time_summary.txt"
extract_time_lines "${results_dir}/rail_transit.log" "${results_dir}/rail_transit_time_summary.txt"
require_time_summary "${results_dir}/test01_time_summary.txt"
require_time_summary "${results_dir}/test02_time_summary.txt"
require_time_summary "${results_dir}/rail_transit_time_summary.txt"

note "running smoke statistics script"
bash "${tool_dir}/bench/run_smoke_stats.sh" > "${results_dir}/run_smoke_stats.log" 2>&1
require_time_summary "${tool_dir}/bench/results/test01_stats_summary.txt"
require_time_summary "${tool_dir}/bench/results/test02_stats_summary.txt"

note "running small RQ2 workload"
bash "${tool_dir}/bench/rq2/run_single_workload.sh" \
    --objects 2 \
    --constraints 1 \
    --events 2 \
    --label artifact_verify \
    --skip-build \
    > "${results_dir}/rq2_single_workload.log" 2>&1
rq2_metrics="${tool_dir}/bench/results/rq2/artifact_verify/metrics.tsv"
if [[ ! -s "${rq2_metrics}" ]]; then
    echo "missing RQ2 metrics file: ${rq2_metrics}" >&2
    exit 1
fi
awk 'NR == 2 { found = 1 } END { exit found ? 0 : 1 }' "${rq2_metrics}" || {
    echo "RQ2 metrics file has no data row: ${rq2_metrics}" >&2
    exit 1
}
rq2_spec="${tool_dir}/bench/results/rq2/artifact_verify/synthetic.oorv"
if grep -q 'uid\.last(default:-1) !=' "${rq2_spec}"; then
    echo "RQ2 generated spec still uses uid.last for object identity: ${rq2_spec}" >&2
    exit 1
fi
if ! grep -q 'c1 != c2 and' "${rq2_spec}"; then
    echo "RQ2 generated spec does not use object-binding inequality: ${rq2_spec}" >&2
    exit 1
fi

note "verifying paper-facing RQ2 manifest and plot inputs"
python3 "${tool_dir}/bench/rq2/verify_rq2_outputs.py" \
    "${tool_dir}/bench/results/rq2/showcase_manifest.tsv" \
    --write-summary "${tool_dir}/bench/results/rq2/showcase_manifest_verification.json" \
    > "${results_dir}/rq2_output_verifier.log"

note "verifying manuscript figure inventory"
python3 - "${repo_root}" > "${results_dir}/figure_inventory_verifier.log" <<'PY'
import pathlib
import re
import sys

repo_root = pathlib.Path(sys.argv[1])
paper_dir = repo_root / "paper"
figure_dir = paper_dir / "figures"
inventory_path = figure_dir / "README.md"

if not inventory_path.is_file():
    raise SystemExit(f"missing figure source inventory: {inventory_path}")

tex_files = [paper_dir / "main.tex"]
tex_files.extend(sorted((paper_dir / "sections").glob("*.tex")))
patterns = [
    re.compile(r"\\input\{figures/([^}]+)\}"),
    re.compile(r"\\includegraphics(?:\[[^\]]*\])?\{figures/([^}]+)\}"),
]

refs = []
for tex_file in tex_files:
    text = tex_file.read_text(encoding="utf-8")
    for pattern in patterns:
        for match in pattern.finditer(text):
            refs.append(match.group(1))

if not refs:
    raise SystemExit("no active manuscript figure references found")

missing = sorted({ref for ref in refs if not (figure_dir / ref).is_file()})
if missing:
    raise SystemExit(f"active manuscript figure references are missing: {missing}")

pdf_refs = sorted({ref for ref in refs if ref.endswith(".pdf")})
if pdf_refs != ["semantics.pdf"]:
    raise SystemExit(
        "unexpected active PDF figure set; expected only semantics.pdf, "
        f"found {pdf_refs}"
    )

legacy_pdfs = {
    "dependence.pdf",
    "eval.pdf",
    "example02.pdf",
    "grammar.pdf",
    "overview.drawio.pdf",
}
active_legacy = sorted(legacy_pdfs.intersection(refs))
if active_legacy:
    raise SystemExit(f"legacy PDF snapshots are still active: {active_legacy}")

inventory = inventory_path.read_text(encoding="utf-8")
undocumented = sorted({ref for ref in refs if ref not in inventory})
if undocumented:
    raise SystemExit(f"active figure references missing from inventory: {undocumented}")
if "semantics.pdf" not in inventory or "PDF-only" not in inventory:
    raise SystemExit("figure inventory does not document the semantics.pdf PDF-only limitation")

print(f"active_figure_refs={len(refs)}")
print("active_pdf_refs=" + ",".join(pdf_refs))
PY

rtlola_bin="${tool_dir}/bench/third_party/rtlola/RTLola-Interpreter-main/crates/target/release/rtlola-cli"
if [[ -x "${rtlola_bin}" ]]; then
    note "running optional RTLola shared-fragment trigger smoke"
    bash "${tool_dir}/bench/shared_fragment/verify_rtlola_fragment.sh" \
        > "${results_dir}/rtlola_shared_fragment.log" 2>&1
else
    note "RTLola CLI is not executable; skipping optional shared-fragment trigger smoke"
fi

note "checking for Windows peak-private-memory metrics"
memory_metrics=()
while IFS= read -r -d '' metrics_file; do
    memory_metrics+=("${metrics_file}")
done < <(find "${tool_dir}/bench/results/rq2" -path '*/windows_memory/*/memory_metrics.tsv' -print0 2>/dev/null)

if [[ "${#memory_metrics[@]}" -eq 0 ]]; then
    if [[ "${require_windows_memory}" == "true" ]]; then
        echo "Windows memory metrics are required but no memory_metrics.tsv files were found under ${tool_dir}/bench/results/rq2/windows_memory" >&2
        exit 1
    fi
    note "no Windows memory metrics found; skipping memory metric validation"
else
    : > "${results_dir}/memory_metrics_verifier.log"
    for metrics_file in "${memory_metrics[@]}"; do
        suite_manifest="$(dirname "${metrics_file}")/suite_manifest.tsv"
        if [[ -f "${suite_manifest}" ]]; then
            python3 "${tool_dir}/bench/rq2/verify_memory_metrics.py" \
                --suite-manifest "${suite_manifest}" \
                "${metrics_file}" \
                >> "${results_dir}/memory_metrics_verifier.log"
        else
            python3 "${tool_dir}/bench/rq2/verify_memory_metrics.py" "${metrics_file}" \
                >> "${results_dir}/memory_metrics_verifier.log"
        fi
    done
fi

if [[ "${skip_paper}" == "true" ]]; then
    note "paper build skipped by --skip-paper"
elif command -v latexmk >/dev/null 2>&1; then
    note "building manuscript"
    (
        cd "${paper_dir}"
        latexmk -pdf -interaction=nonstopmode -halt-on-error main.tex \
            > "${results_dir}/paper_latexmk.log" 2>&1
    )
    if grep -E 'undefined references|undefined citations|Citation .* undefined|Reference .* undefined|Fatal|Emergency stop|LaTeX Error' "${paper_dir}/main.log" >/dev/null 2>&1; then
        echo "paper build log contains unresolved reference/citation or fatal LaTeX diagnostics" >&2
        exit 1
    fi
    if ! grep -q 'sections/abstract' "${paper_dir}/main.abs"; then
        echo "paper abstract file does not include sections/abstract.tex" >&2
        exit 1
    fi
    if command -v gs >/dev/null 2>&1; then
        gs -dNOSAFER -q -sDEVICE=txtwrite -o "${results_dir}/paper_text.txt" \
            "${paper_dir}/main.pdf"
        if ! grep -q 'Runtime verification of cyber-physical systems' \
            "${results_dir}/paper_text.txt"; then
            echo "paper PDF text does not contain the abstract body" >&2
            exit 1
        fi
    else
        note "Ghostscript not found; skipping PDF abstract text extraction check"
    fi
else
    note "latexmk not found; paper build skipped"
fi

note "artifact verification completed"
note "logs written under ${results_dir}"
