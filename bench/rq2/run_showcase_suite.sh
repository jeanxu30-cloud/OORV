#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tool_dir="$(cd "${script_dir}/../.." && pwd)"
results_root="${tool_dir}/bench/results/rq2"

profile="paper_longhaul"
repetitions="4"
events="320"
matrix_repetitions="3"
matrix_events="120"
manifest="${results_root}/showcase_manifest.tsv"
build_log="${results_root}/showcase_build.log"
resume="false"
force="false"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --profile)
            profile="$2"
            shift 2
            ;;
        --repetitions)
            repetitions="$2"
            shift 2
            ;;
        --events)
            events="$2"
            shift 2
            ;;
        --matrix-repetitions)
            matrix_repetitions="$2"
            shift 2
            ;;
        --matrix-events)
            matrix_events="$2"
            shift 2
            ;;
        --manifest)
            manifest="$2"
            shift 2
            ;;
        --resume)
            resume="true"
            shift
            ;;
        --force)
            force="true"
            shift
            ;;
        *)
            echo "unknown argument: $1" >&2
            exit 1
            ;;
    esac
done

if ! [[ "${repetitions}" =~ ^[0-9]+$ ]] || [[ "${repetitions}" -lt 1 ]]; then
    echo "repetitions must be a positive integer" >&2
    exit 1
fi

if ! [[ "${events}" =~ ^[0-9]+$ ]] || [[ "${events}" -lt 1 ]]; then
    echo "events must be a positive integer" >&2
    exit 1
fi

if ! [[ "${matrix_repetitions}" =~ ^[0-9]+$ ]] || [[ "${matrix_repetitions}" -lt 1 ]]; then
    echo "matrix-repetitions must be a positive integer" >&2
    exit 1
fi

if ! [[ "${matrix_events}" =~ ^[0-9]+$ ]] || [[ "${matrix_events}" -lt 1 ]]; then
    echo "matrix-events must be a positive integer" >&2
    exit 1
fi

case "${profile}" in
    sandbox)
        object_points=(1 5 10 20 40)
        constraint_points=(1 2 4 8 12)
        history_points=(0 1 2 4 8)
        periodic_points=(0 1 2 4)
        matrix_object_points=(4 10 20 40)
        matrix_constraint_points=(2 4 8 12)
        objects_fixed="20"
        constraints_fixed="4"
        periodic_hz="100"
        mixed_points=()
        burst_points=()
        hotset_points=()
        soak_points=()
        object_repetitions="${repetitions}"
        constraint_repetitions="${repetitions}"
        history_repetitions="${repetitions}"
        periodic_repetitions="${repetitions}"
        matrix_point_repetitions="${matrix_repetitions}"
        mixed_repetitions="0"
        burst_repetitions="0"
        hotset_repetitions="0"
        soak_repetitions="0"
        object_events="${events}"
        constraint_events="${events}"
        history_events="${events}"
        periodic_events="${events}"
        matrix_point_events="${matrix_events}"
        mixed_events="${events}"
        burst_events="${events}"
        hotset_events="${events}"
        mixed_objects="${objects_fixed}"
        mixed_constraints="${constraints_fixed}"
        burst_objects="${objects_fixed}"
        burst_constraints="${constraints_fixed}"
        hotset_objects="${objects_fixed}"
        hotset_constraints="${constraints_fixed}"
        soak_objects="${objects_fixed}"
        soak_constraints="${constraints_fixed}"
        burst_history_depth="0"
        burst_periodic_constraints="0"
        hotset_history_depth="0"
        hotset_periodic_constraints="0"
        soak_history_depth="0"
        soak_periodic_constraints="0"
        burst_hotset_size="${objects_fixed}"
        burst_phase_length="0"
        burst_time_step_ms="1.0"
        hotset_gap_ms="10.0"
        hotset_burst_size="1"
        hotset_phase_length="0"
        hotset_time_step_ms="1.0"
        soak_hotset_size="${objects_fixed}"
        soak_phase_length="0"
        soak_time_step_ms="1.0"
        ;;
    paper_dense)
        object_points=(1 2 4 6 8 12 16 24 32 40)
        constraint_points=(1 2 4 6 8 10 12 14 16)
        history_points=(0 1 2 3 4 6 8 10 12)
        periodic_points=(0 1 2 3 4 6 8 10)
        matrix_object_points=(4 8 12 16 24 40)
        matrix_constraint_points=(2 4 6 8 12 16)
        objects_fixed="24"
        constraints_fixed="6"
        periodic_hz="100"
        mixed_points=()
        burst_points=()
        hotset_points=()
        soak_points=()
        object_repetitions="${repetitions}"
        constraint_repetitions="${repetitions}"
        history_repetitions="${repetitions}"
        periodic_repetitions="${repetitions}"
        matrix_point_repetitions="${matrix_repetitions}"
        mixed_repetitions="0"
        burst_repetitions="0"
        hotset_repetitions="0"
        soak_repetitions="0"
        object_events="${events}"
        constraint_events="${events}"
        history_events="${events}"
        periodic_events="${events}"
        matrix_point_events="${matrix_events}"
        mixed_events="${events}"
        burst_events="${events}"
        hotset_events="${events}"
        mixed_objects="${objects_fixed}"
        mixed_constraints="${constraints_fixed}"
        burst_objects="${objects_fixed}"
        burst_constraints="${constraints_fixed}"
        hotset_objects="${objects_fixed}"
        hotset_constraints="${constraints_fixed}"
        soak_objects="${objects_fixed}"
        soak_constraints="${constraints_fixed}"
        burst_history_depth="0"
        burst_periodic_constraints="0"
        hotset_history_depth="0"
        hotset_periodic_constraints="0"
        soak_history_depth="0"
        soak_periodic_constraints="0"
        burst_hotset_size="${objects_fixed}"
        burst_phase_length="0"
        burst_time_step_ms="1.0"
        hotset_gap_ms="10.0"
        hotset_burst_size="1"
        hotset_phase_length="0"
        hotset_time_step_ms="1.0"
        soak_hotset_size="${objects_fixed}"
        soak_phase_length="0"
        soak_time_step_ms="1.0"
        ;;
    paper_longhaul)
        object_points=(1 2 4 6 8 12 16 20 24 32 40 48)
        constraint_points=(1 2 4 6 8 10 12 14 16 18 20 24)
        history_points=(0 1 2 4 6 8 10 12 16 20)
        periodic_points=(0 1 2 4 6 8 10 12 16 20)
        matrix_object_points=(4 8 12 16 24 32 40)
        matrix_constraint_points=(2 4 6 8 12 16 20)
        mixed_points=(0 2 4 6 8 10 12 16 20)
        burst_points=(1 2 4 8 12 16)
        hotset_points=(4 8 12 16 20 24)
        soak_points=(320 640 960 1600 3200 6400 12800 25600)
        objects_fixed="24"
        constraints_fixed="6"
        periodic_hz="100"
        object_repetitions="5"
        constraint_repetitions="5"
        history_repetitions="5"
        periodic_repetitions="5"
        matrix_point_repetitions="3"
        mixed_repetitions="3"
        burst_repetitions="3"
        hotset_repetitions="3"
        soak_repetitions="2"
        object_events="640"
        constraint_events="640"
        history_events="960"
        periodic_events="960"
        matrix_point_events="240"
        mixed_events="1280"
        burst_events="2400"
        hotset_events="2400"
        mixed_objects="24"
        mixed_constraints="8"
        burst_objects="24"
        burst_constraints="8"
        hotset_objects="24"
        hotset_constraints="8"
        soak_objects="24"
        soak_constraints="8"
        burst_history_depth="8"
        burst_periodic_constraints="8"
        hotset_history_depth="8"
        hotset_periodic_constraints="8"
        soak_history_depth="8"
        soak_periodic_constraints="8"
        burst_hotset_size="8"
        burst_phase_length="96"
        burst_time_step_ms="0.8"
        hotset_gap_ms="29.0"
        hotset_burst_size="8"
        hotset_phase_length="96"
        hotset_time_step_ms="0.8"
        soak_hotset_size="8"
        soak_phase_length="128"
        soak_time_step_ms="0.8"
        ;;
    *)
        echo "unknown profile: ${profile}" >&2
        exit 1
        ;;
esac

mkdir -p "$(dirname "${manifest}")"
if [[ "${force}" == "true" ]]; then
    find "${results_root}" -mindepth 1 -maxdepth 1 -type d -name 'showcase_*' -exec rm -rf {} +
fi

(
    cd "${tool_dir}"
    cargo build --offline --locked --release > "${build_log}" 2>&1
)

if [[ "${resume}" != "true" || ! -f "${manifest}" ]]; then
    {
        printf 'label\tobjects\tconstraints\tevents_requested\tevents_processed\tavg_ms\tduration_s\tevents_per_s\tmemory_mode\tmemory_value\tmemory_note\thistory_depth\tperiodic_constraints\tperiodic_hz\tburst_size\thotset_size\tphase_length\ttime_step_ms\tburst_gap_ms\tfamily\trepetition\n'
    } > "${manifest}"
fi

manifest_has_label() {
    local label="$1"
    if [[ ! -f "${manifest}" ]]; then
        return 1
    fi
    rg -q "^${label}\t" "${manifest}"
}

append_metrics() {
    local label="$1"
    shift
    if [[ "${force}" != "true" ]] && manifest_has_label "${label}"; then
        return 0
    fi
    if [[ "${force}" != "true" ]] && [[ -f "${results_root}/${label}/metrics.tsv" ]]; then
        tail -n +2 "${results_root}/${label}/metrics.tsv" >> "${manifest}"
        return 0
    fi
    if [[ "${force}" == "true" ]]; then
        rm -rf "${results_root:?}/${label}"
    fi
    bash "${script_dir}/run_single_workload.sh" "$@" --label "${label}" --skip-build
    tail -n +2 "${results_root}/${label}/metrics.tsv" >> "${manifest}"
}

for objects in "${object_points[@]}"; do
    for ((rep = 1; rep <= object_repetitions; rep++)); do
        label="$(printf 'showcase_object_o%03d_r%02d' "${objects}" "${rep}")"
        append_metrics "${label}" \
            --objects "${objects}" \
            --constraints "${constraints_fixed}" \
            --events "${object_events}" \
            --history-depth 0 \
            --periodic-constraints 0 \
            --periodic-hz "${periodic_hz}" \
            --family object_sweep \
            --repetition "${rep}"
    done
done

for constraints in "${constraint_points[@]}"; do
    for ((rep = 1; rep <= constraint_repetitions; rep++)); do
        label="$(printf 'showcase_constraint_c%03d_r%02d' "${constraints}" "${rep}")"
        append_metrics "${label}" \
            --objects "${objects_fixed}" \
            --constraints "${constraints}" \
            --events "${constraint_events}" \
            --history-depth 0 \
            --periodic-constraints 0 \
            --periodic-hz "${periodic_hz}" \
            --family constraint_sweep \
            --repetition "${rep}"
    done
done

for history_depth in "${history_points[@]}"; do
    for ((rep = 1; rep <= history_repetitions; rep++)); do
        label="$(printf 'showcase_history_h%03d_r%02d' "${history_depth}" "${rep}")"
        append_metrics "${label}" \
            --objects "${objects_fixed}" \
            --constraints "${constraints_fixed}" \
            --events "${history_events}" \
            --history-depth "${history_depth}" \
            --periodic-constraints 0 \
            --periodic-hz "${periodic_hz}" \
            --family history_sweep \
            --repetition "${rep}"
    done
done

for periodic_constraints in "${periodic_points[@]}"; do
    for ((rep = 1; rep <= periodic_repetitions; rep++)); do
        label="$(printf 'showcase_periodic_p%03d_r%02d' "${periodic_constraints}" "${rep}")"
        append_metrics "${label}" \
            --objects "${objects_fixed}" \
            --constraints "${constraints_fixed}" \
            --events "${periodic_events}" \
            --history-depth 0 \
            --periodic-constraints "${periodic_constraints}" \
            --periodic-hz "${periodic_hz}" \
            --family periodic_sweep \
            --repetition "${rep}"
    done
done

for objects in "${matrix_object_points[@]}"; do
    for constraints in "${matrix_constraint_points[@]}"; do
        for ((rep = 1; rep <= matrix_point_repetitions; rep++)); do
            label="$(printf 'showcase_matrix_o%03d_c%03d_r%02d' "${objects}" "${constraints}" "${rep}")"
            append_metrics "${label}" \
                --objects "${objects}" \
                --constraints "${constraints}" \
                --events "${matrix_point_events}" \
                --history-depth 0 \
                --periodic-constraints 0 \
                --periodic-hz "${periodic_hz}" \
                --family matrix_sweep \
                --repetition "${rep}"
        done
    done
done

for mixed_level in "${mixed_points[@]}"; do
    for ((rep = 1; rep <= mixed_repetitions; rep++)); do
        label="$(printf 'showcase_mixed_m%03d_r%02d' "${mixed_level}" "${rep}")"
        append_metrics "${label}" \
            --objects "${mixed_objects}" \
            --constraints "${mixed_constraints}" \
            --events "${mixed_events}" \
            --history-depth "${mixed_level}" \
            --periodic-constraints "${mixed_level}" \
            --periodic-hz "${periodic_hz}" \
            --family mixed_feature_sweep \
            --repetition "${rep}"
    done
done

for burst_size in "${burst_points[@]}"; do
    burst_gap_ms="$(awk -v value="${burst_size}" 'BEGIN { printf "%.1f", 5.0 + value * 3.0 }')"
    for ((rep = 1; rep <= burst_repetitions; rep++)); do
        label="$(printf 'showcase_burst_b%03d_r%02d' "${burst_size}" "${rep}")"
        append_metrics "${label}" \
            --objects "${burst_objects}" \
            --constraints "${burst_constraints}" \
            --events "${burst_events}" \
            --history-depth "${burst_history_depth}" \
            --periodic-constraints "${burst_periodic_constraints}" \
            --periodic-hz "${periodic_hz}" \
            --burst-size "${burst_size}" \
            --hotset-size "${burst_hotset_size}" \
            --phase-length "${burst_phase_length}" \
            --time-step-ms "${burst_time_step_ms}" \
            --burst-gap-ms "${burst_gap_ms}" \
            --family burst_sweep \
            --repetition "${rep}"
    done
done

for hotset_size in "${hotset_points[@]}"; do
    for ((rep = 1; rep <= hotset_repetitions; rep++)); do
        label="$(printf 'showcase_hotset_hs%03d_r%02d' "${hotset_size}" "${rep}")"
        append_metrics "${label}" \
            --objects "${hotset_objects}" \
            --constraints "${hotset_constraints}" \
            --events "${hotset_events}" \
            --history-depth "${hotset_history_depth}" \
            --periodic-constraints "${hotset_periodic_constraints}" \
            --periodic-hz "${periodic_hz}" \
            --burst-size "${hotset_burst_size}" \
            --hotset-size "${hotset_size}" \
            --phase-length "${hotset_phase_length}" \
            --time-step-ms "${hotset_time_step_ms}" \
            --burst-gap-ms "${hotset_gap_ms}" \
            --family hotset_sweep \
            --repetition "${rep}"
    done
done

for soak_events in "${soak_points[@]}"; do
    for ((rep = 1; rep <= soak_repetitions; rep++)); do
        label="$(printf 'showcase_soak_e%05d_r%02d' "${soak_events}" "${rep}")"
        append_metrics "${label}" \
            --objects "${soak_objects}" \
            --constraints "${soak_constraints}" \
            --events "${soak_events}" \
            --history-depth "${soak_history_depth}" \
            --periodic-constraints "${soak_periodic_constraints}" \
            --periodic-hz "${periodic_hz}" \
            --burst-size 8 \
            --hotset-size "${soak_hotset_size}" \
            --phase-length "${soak_phase_length}" \
            --time-step-ms "${soak_time_step_ms}" \
            --burst-gap-ms 24.0 \
            --family soak_sweep \
            --repetition "${rep}"
    done
done

python3 "${script_dir}/prepare_plot_data.py" "${manifest}"

printf 'wrote %s\n' "${build_log}"
printf 'wrote %s\n' "${manifest}"
