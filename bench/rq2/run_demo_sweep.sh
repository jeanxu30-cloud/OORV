#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tool_dir="$(cd "${script_dir}/../.." && pwd)"
results_root="${tool_dir}/bench/results/rq2"
aggregate="${results_root}/demo_manifest.tsv"
build_log="${results_root}/demo_build.log"

mkdir -p "${results_root}"

(
    cd "${tool_dir}"
    cargo build --offline --locked --release > "${build_log}" 2>&1
)

{
    printf 'label\tobjects\tconstraints\tevents_requested\tevents_processed\tavg_ms\tduration_s\tevents_per_s\tmemory_mode\tmemory_value\tmemory_note\n'
} > "${aggregate}"

run_point() {
    local label="$1"
    shift
    bash "${script_dir}/run_single_workload.sh" "$@" --label "${label}" --skip-build
    tail -n +2 "${results_root}/${label}/metrics.tsv" >> "${aggregate}"
}

run_point "demo_object_001" --objects 1 --constraints 4 --events 500
run_point "demo_object_010" --objects 10 --constraints 4 --events 500
run_point "demo_object_100" --objects 100 --constraints 4 --events 500

run_point "demo_constraint_001" --objects 50 --constraints 1 --events 500
run_point "demo_constraint_004" --objects 50 --constraints 4 --events 500
run_point "demo_constraint_008" --objects 50 --constraints 8 --events 500

"${script_dir}/render_manifest_summary.sh" "${aggregate}"

printf 'wrote %s\n' "${build_log}"
printf 'wrote %s\n' "${aggregate}"
