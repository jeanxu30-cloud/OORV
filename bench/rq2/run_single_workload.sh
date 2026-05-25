#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tool_dir="$(cd "${script_dir}/../.." && pwd)"
results_root="${tool_dir}/bench/results/rq2"

objects=""
constraints=""
events=""
label=""
skip_build="false"
memory_mode="none"
history_depth="0"
periodic_constraints="0"
periodic_hz="100"
family="ad_hoc"
repetition="1"
burst_size="1"
hotset_size="0"
phase_length="0"
time_step_ms="1.0"
burst_gap_ms="10.0"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --objects)
            objects="$2"
            shift 2
            ;;
        --constraints)
            constraints="$2"
            shift 2
            ;;
        --events)
            events="$2"
            shift 2
            ;;
        --label)
            label="$2"
            shift 2
            ;;
        --history-depth)
            history_depth="$2"
            shift 2
            ;;
        --periodic-constraints)
            periodic_constraints="$2"
            shift 2
            ;;
        --periodic-hz)
            periodic_hz="$2"
            shift 2
            ;;
        --family)
            family="$2"
            shift 2
            ;;
        --repetition)
            repetition="$2"
            shift 2
            ;;
        --burst-size)
            burst_size="$2"
            shift 2
            ;;
        --hotset-size)
            hotset_size="$2"
            shift 2
            ;;
        --phase-length)
            phase_length="$2"
            shift 2
            ;;
        --time-step-ms)
            time_step_ms="$2"
            shift 2
            ;;
        --burst-gap-ms)
            burst_gap_ms="$2"
            shift 2
            ;;
        --skip-build)
            skip_build="true"
            shift
            ;;
        --memory-mode)
            memory_mode="$2"
            shift 2
            ;;
        *)
            echo "unknown argument: $1" >&2
            exit 1
            ;;
    esac
done

if [[ -z "${objects}" || -z "${constraints}" || -z "${events}" ]]; then
    echo "usage: $0 --objects <n> --constraints <n> --events <n> [--label <name>] [--history-depth <n>] [--periodic-constraints <n>] [--periodic-hz <n>] [--burst-size <n>] [--hotset-size <n>] [--phase-length <n>] [--time-step-ms <n>] [--burst-gap-ms <n>] [--family <name>] [--repetition <n>] [--skip-build] [--memory-mode none|macos-rss]" >&2
    exit 1
fi

if ! [[ "${objects}" =~ ^[0-9]+$ ]] || [[ "${objects}" -lt 1 ]]; then
    echo "objects must be a positive integer" >&2
    exit 1
fi

if ! [[ "${constraints}" =~ ^[0-9]+$ ]] || [[ "${constraints}" -lt 1 ]]; then
    echo "constraints must be a positive integer" >&2
    exit 1
fi

if ! [[ "${events}" =~ ^[0-9]+$ ]] || [[ "${events}" -lt 1 ]]; then
    echo "events must be a positive integer" >&2
    exit 1
fi

if [[ -z "${label}" ]]; then
    label="o${objects}_c${constraints}_e${events}"
fi

if [[ "${memory_mode}" != "none" && "${memory_mode}" != "macos-rss" ]]; then
    echo "memory-mode must be one of: none, macos-rss" >&2
    exit 1
fi

if ! [[ "${history_depth}" =~ ^[0-9]+$ ]]; then
    echo "history-depth must be a non-negative integer" >&2
    exit 1
fi

if ! [[ "${periodic_constraints}" =~ ^[0-9]+$ ]]; then
    echo "periodic-constraints must be a non-negative integer" >&2
    exit 1
fi

if ! [[ "${periodic_hz}" =~ ^[0-9]+$ ]] || [[ "${periodic_hz}" -lt 1 ]]; then
    echo "periodic-hz must be a positive integer" >&2
    exit 1
fi

if ! [[ "${repetition}" =~ ^[0-9]+$ ]] || [[ "${repetition}" -lt 1 ]]; then
    echo "repetition must be a positive integer" >&2
    exit 1
fi

if ! [[ "${burst_size}" =~ ^[0-9]+$ ]] || [[ "${burst_size}" -lt 1 ]]; then
    echo "burst-size must be a positive integer" >&2
    exit 1
fi

if ! [[ "${hotset_size}" =~ ^[0-9]+$ ]]; then
    echo "hotset-size must be a non-negative integer" >&2
    exit 1
fi

if [[ "${hotset_size}" -gt "${objects}" ]]; then
    echo "hotset-size cannot exceed objects" >&2
    exit 1
fi

if ! [[ "${phase_length}" =~ ^[0-9]+$ ]]; then
    echo "phase-length must be a non-negative integer" >&2
    exit 1
fi

if ! awk -v value="${time_step_ms}" 'BEGIN { exit !(value + 0 > 0) }'; then
    echo "time-step-ms must be a positive number" >&2
    exit 1
fi

if ! awk -v value="${burst_gap_ms}" 'BEGIN { exit !(value + 0 >= 0) }'; then
    echo "burst-gap-ms must be a non-negative number" >&2
    exit 1
fi

workdir="${results_root}/${label}"
mkdir -p "${workdir}"

spec_path="${workdir}/synthetic.oorv"
trace_path="${workdir}/synthetic.csv"
build_log="${workdir}/build.log"
raw_log="${workdir}/run.log"
clean_log="${workdir}/run_clean.log"
summary_log="${workdir}/summary.txt"
metrics_tsv="${workdir}/metrics.tsv"
memory_log="${workdir}/memory.log"

bash "${script_dir}/generate_synthetic_spec.sh" "${spec_path}" "${constraints}" \
    --history-depth "${history_depth}" \
    --periodic-constraints "${periodic_constraints}" \
    --periodic-hz "${periodic_hz}"
bash "${script_dir}/generate_trace.sh" "${trace_path}" "${objects}" "${events}" \
    --burst-size "${burst_size}" \
    --hotset-size "${hotset_size}" \
    --phase-length "${phase_length}" \
    --time-step-ms "${time_step_ms}" \
    --burst-gap-ms "${burst_gap_ms}"

if [[ "${skip_build}" != "true" ]]; then
    (
        cd "${tool_dir}"
        cargo build --offline --locked --release > "${build_log}" 2>&1
    )
else
    printf 'build skipped; using existing binary\n' > "${build_log}"
fi

binary=""
for candidate in oorv; do
    if [[ -x "${tool_dir}/target/release/${candidate}" ]]; then
        binary="${tool_dir}/target/release/${candidate}"
        break
    fi
done

if [[ -z "${binary}" ]]; then
    echo "missing executable under ${tool_dir}/target/release/ (tried oorv)" >&2
    exit 1
fi

if [[ "${memory_mode}" == "macos-rss" ]]; then
    (
        cd "${tool_dir}"
        /usr/bin/time -l "${binary}" "${spec_path}" --offline relative --csv-in "${trace_path}" --verbosity silent --statistics all
    ) > "${raw_log}" 2> "${memory_log}" || true
else
    (
        cd "${tool_dir}"
        "${binary}" "${spec_path}" --offline relative --csv-in "${trace_path}" --verbosity silent --statistics all
    ) > "${raw_log}" 2>&1
    printf 'memory measurement mode: none\n' > "${memory_log}"
fi

if command -v perl >/dev/null 2>&1; then
    perl -pe 's/\e\[[0-9;?]*[ -\/]*[@-~]//g' "${raw_log}" > "${clean_log}"
else
    cp "${raw_log}" "${clean_log}"
fi

if command -v rg >/dev/null 2>&1; then
    rg '@@' "${clean_log}" > "${summary_log}" || true
else
    grep '@@' "${clean_log}" > "${summary_log}" || true
fi

avg_ms="$(sed -n 's/.*AVG_MS:\([0-9.][0-9.]*\).*/\1/p' "${summary_log}" | head -n 1)"
duration_s="$(sed -n 's/.*DURATION_S:\([0-9.][0-9.]*\).*/\1/p' "${summary_log}" | head -n 1)"
events_per_s="$(sed -n 's/.*EVENTS_PER_S:\([0-9.][0-9.]*\).*/\1/p' "${summary_log}" | head -n 1)"
processed_count="$(sed -n 's/.*COUNT:\([0-9][0-9]*\).*/\1/p' "${summary_log}" | head -n 1)"

if [[ -z "${processed_count}" ]]; then
    processed_count="${events}"
fi

if [[ -z "${avg_ms}" && -n "${duration_s}" && -n "${processed_count}" && "${processed_count}" != "0" ]]; then
    avg_ms="$(awk -v duration_s="${duration_s}" -v processed="${processed_count}" 'BEGIN { printf "%.3f", (duration_s * 1000.0) / processed }')"
fi

peak_memory_value="NA"
peak_memory_note="not_measured"
if [[ "${memory_mode}" == "macos-rss" ]]; then
    peak_memory_value="$(awk '/maximum resident set size/ { print $1; exit }' "${memory_log}")"
    if [[ -n "${peak_memory_value}" ]]; then
        peak_memory_note="macos_max_resident_set_size_bytes"
    else
        peak_memory_value="NA"
        peak_memory_note="unavailable_or_not_permitted"
    fi
fi

{
    printf 'label\tobjects\tconstraints\tevents_requested\tevents_processed\tavg_ms\tduration_s\tevents_per_s\tmemory_mode\tmemory_value\tmemory_note\thistory_depth\tperiodic_constraints\tperiodic_hz\tburst_size\thotset_size\tphase_length\ttime_step_ms\tburst_gap_ms\tfamily\trepetition\n'
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "${label}" \
        "${objects}" \
        "${constraints}" \
        "${events}" \
        "${processed_count:-NA}" \
        "${avg_ms:-NA}" \
        "${duration_s:-NA}" \
        "${events_per_s:-NA}" \
        "${memory_mode}" \
        "${peak_memory_value}" \
        "${peak_memory_note}" \
        "${history_depth}" \
        "${periodic_constraints}" \
        "${periodic_hz}" \
        "${burst_size}" \
        "${hotset_size}" \
        "${phase_length}" \
        "${time_step_ms}" \
        "${burst_gap_ms}" \
        "${family}" \
        "${repetition}"
} > "${metrics_tsv}"

printf 'wrote %s\n' "${spec_path}"
printf 'wrote %s\n' "${trace_path}"
printf 'wrote %s\n' "${summary_log}"
printf 'wrote %s\n' "${metrics_tsv}"
