#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tool_dir="$(cd "${script_dir}/.." && pwd)"
results_dir="${tool_dir}/bench/results"
build_log="${results_dir}/build.log"

mkdir -p "${results_dir}"

cd "${tool_dir}"

cargo build --offline --locked --release > "${build_log}" 2>&1
printf 'wrote %s\n' "${build_log}"

run_case() {
    local name="$1"
    local spec="test/${name}.oorv"
    local csv="test/${name}.csv"
    local raw_log="${results_dir}/${name}_stats.log"
    local stderr_log="${results_dir}/${name}_stats_stderr.log"
    local summary_log="${results_dir}/${name}_stats_summary.txt"
    local cleaned_log="${results_dir}/${name}_stats_clean.log"

    ./target/release/oorv "${spec}" \
        --offline relative \
        --csv-in "${csv}" \
        --verbosity silent \
        --statistics all \
        > "${raw_log}" 2> "${stderr_log}"

    if command -v perl >/dev/null 2>&1; then
        perl -pe 's/\e\[[0-9;?]*[ -\/]*[@-~]//g' "${raw_log}" > "${cleaned_log}"
    else
        cp "${raw_log}" "${cleaned_log}"
    fi

    if command -v rg >/dev/null 2>&1; then
        rg '@@' "${cleaned_log}" > "${summary_log}" || true
    else
        grep '@@' "${cleaned_log}" > "${summary_log}" || true
    fi

    printf 'wrote %s, %s, %s, and %s\n' "${raw_log}" "${stderr_log}" "${cleaned_log}" "${summary_log}"
}

run_case "test01"
run_case "test02"

printf 'smoke statistics completed in %s\n' "${results_dir}"
