#!/usr/bin/env bash

set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <manifest.tsv>" >&2
    exit 1
fi

manifest="$1"
if [[ ! -f "${manifest}" ]]; then
    echo "manifest not found: ${manifest}" >&2
    exit 1
fi

out_dir="$(cd "$(dirname "${manifest}")" && pwd)"
base_name="$(basename "${manifest}" .tsv)"
summary_tsv="${out_dir}/${base_name}_summary.tsv"
summary_md="${out_dir}/${base_name}_summary.md"
summary_tex="${out_dir}/${base_name}_summary.tex"

awk -F'\t' '
NR == 1 { next }
{
    label = $1
    objects = $2 + 0
    constraints = $3 + 0
    avg_ms = $6 + 0
    throughput = $8 + 0
    memory_mode = $9

    if (label ~ /object_/) {
        group = "object_sweep"
    } else if (label ~ /constraint_/) {
        group = "constraint_sweep"
    } else {
        next
    }

    count[group]++

    if (!(group in objects_min) || objects < objects_min[group]) objects_min[group] = objects
    if (!(group in objects_max) || objects > objects_max[group]) objects_max[group] = objects
    if (!(group in constraints_min) || constraints < constraints_min[group]) constraints_min[group] = constraints
    if (!(group in constraints_max) || constraints > constraints_max[group]) constraints_max[group] = constraints
    if (!(group in avg_min) || avg_ms < avg_min[group]) avg_min[group] = avg_ms
    if (!(group in avg_max) || avg_ms > avg_max[group]) avg_max[group] = avg_ms
    if (!(group in thr_min) || throughput < thr_min[group]) thr_min[group] = throughput
    if (!(group in thr_max) || throughput > thr_max[group]) thr_max[group] = throughput
    if (!(group in memory_modes)) {
        memory_modes[group] = memory_mode
    } else if (memory_modes[group] != memory_mode) {
        memory_modes[group] = "mixed"
    }
}
END {
    print "sweep\tpoints\tobjects_range\tconstraints_range\tavg_ms_range\tthroughput_range\tmemory_mode"

    if (count["object_sweep"] > 0) {
        printf "object_sweep\t%d\t%d-%d\t%d-%d\t%.3f-%.3f\t%.3f-%.3f\t%s\n",
            count["object_sweep"],
            objects_min["object_sweep"], objects_max["object_sweep"],
            constraints_min["object_sweep"], constraints_max["object_sweep"],
            avg_min["object_sweep"], avg_max["object_sweep"],
            thr_min["object_sweep"], thr_max["object_sweep"],
            memory_modes["object_sweep"]
    }

    if (count["constraint_sweep"] > 0) {
        printf "constraint_sweep\t%d\t%d-%d\t%d-%d\t%.3f-%.3f\t%.3f-%.3f\t%s\n",
            count["constraint_sweep"],
            objects_min["constraint_sweep"], objects_max["constraint_sweep"],
            constraints_min["constraint_sweep"], constraints_max["constraint_sweep"],
            avg_min["constraint_sweep"], avg_max["constraint_sweep"],
            thr_min["constraint_sweep"], thr_max["constraint_sweep"],
            memory_modes["constraint_sweep"]
    }
}
' "${manifest}" > "${summary_tsv}"

{
    echo "# RQ2 Manifest Summary"
    echo
    echo "Source manifest: \`${manifest}\`"
    echo
    echo "| Sweep | Points | Objects | Constraints | Avg latency (ms) | Throughput (events/s) | Memory mode |"
    echo "| --- | ---: | --- | --- | --- | --- | --- |"
    awk -F'\t' '
    NR == 1 { next }
    {
        printf "| %s | %s | %s | %s | %s | %s | %s |\n", $1, $2, $3, $4, $5, $6, $7
    }' "${summary_tsv}"
} > "${summary_md}"

{
    echo "% Auto-generated from ${manifest}"
    echo "\\begin{tabular}{lllllll}"
    echo "\\toprule"
    echo "Sweep & Points & Objects & Constraints & Avg latency (ms) & Throughput (events/s) & Memory mode \\\\"
    echo "\\midrule"
    awk -F'\t' '
    NR == 1 { next }
    {
        printf "%s & %s & %s & %s & %s & %s & %s \\\\\n", $1, $2, $3, $4, $5, $6, $7
    }' "${summary_tsv}"
    echo "\\bottomrule"
    echo "\\end{tabular}"
} > "${summary_tex}"

printf 'wrote %s\n' "${summary_tsv}"
printf 'wrote %s\n' "${summary_md}"
printf 'wrote %s\n' "${summary_tex}"
