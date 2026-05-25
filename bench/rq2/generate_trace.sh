#!/usr/bin/env bash

set -euo pipefail

if [[ $# -lt 3 ]]; then
    echo "usage: $0 <output-csv> <object-count> <event-count> [--burst-size <n>] [--hotset-size <n>] [--phase-length <n>] [--time-step-ms <n>] [--burst-gap-ms <n>]" >&2
    exit 1
fi

output_csv="$1"
object_count="$2"
event_count="$3"
shift 3

burst_size="1"
hotset_size="0"
phase_length="0"
time_step_ms="1.0"
burst_gap_ms="10.0"

while [[ $# -gt 0 ]]; do
    case "$1" in
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
        *)
            echo "unknown argument: $1" >&2
            exit 1
            ;;
    esac
done

if ! [[ "${object_count}" =~ ^[0-9]+$ ]] || [[ "${object_count}" -lt 1 ]]; then
    echo "object-count must be a positive integer" >&2
    exit 1
fi

if ! [[ "${event_count}" =~ ^[0-9]+$ ]] || [[ "${event_count}" -lt 1 ]]; then
    echo "event-count must be a positive integer" >&2
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

if [[ "${hotset_size}" -gt "${object_count}" ]]; then
    echo "hotset-size cannot exceed object-count" >&2
    exit 1
fi

mkdir -p "$(dirname "${output_csv}")"

awk \
    -v objects="${object_count}" \
    -v events="${event_count}" \
    -v burst_size="${burst_size}" \
    -v hotset_size="${hotset_size}" \
    -v phase_length="${phase_length}" \
    -v time_step_ms="${time_step_ms}" \
    -v burst_gap_ms="${burst_gap_ms}" '
BEGIN {
    base_step = time_step_ms / 1000.0
    burst_gap = burst_gap_ms / 1000.0
    active_set = hotset_size
    if (active_set < 1 || active_set > objects) {
        active_set = objects
    }
    if (phase_length < 1) {
        phase_length = events + 1
    }

    print "Benchmark::Car::uid,Benchmark::Car::lat,Benchmark::Car::lon,Benchmark::Car::speed,time"

    current_time = 0.0
    for (i = 1; i <= events; i++) {
        phase_index = int((i - 1) / phase_length)
        phase_slot = (i - 1) % phase_length
        burst_slot = (i - 1) % burst_size

        if (active_set == objects) {
            uid = ((i - 1) % objects) + 1
        } else {
            hotset_offset = (phase_index * active_set) % objects
            uid = ((phase_slot + burst_slot) % active_set + hotset_offset) % objects + 1
        }

        # The synthetic geometry stays non-violating while introducing mild
        # burst-local and phase-local variation, which lets the workload stress
        # object churn and periodic scheduling without changing the semantics.
        lat = uid * 1.000 + (phase_index % 9) * 0.021 + burst_slot * 0.004
        lon = (uid % 7) * 0.500 + (phase_slot % 11) * 0.013
        speed = 20.0 + (uid % 11) + (phase_slot % 13) * 0.140 + burst_slot * 0.050

        printf "%d,%.3f,%.3f,%.3f,%.6f\n", uid, lat, lon, speed, current_time

        current_time += base_step
        if (burst_size > 1 && burst_slot == burst_size - 1) {
            current_time += burst_gap
        }
    }
}
' > "${output_csv}"
