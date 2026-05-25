#!/usr/bin/env python3

from __future__ import annotations

import csv
import statistics
import sys
from pathlib import Path


LINEAR_FAMILY_SPECS = {
    "object_sweep": {
        "x_field": "objects",
        "x_label": "Objects",
        "file_suffix": "object",
        "summary_label": "Object sweep",
        "table_label": "Object",
        "axis_label": "Objects",
        "claim_prefix": "Object",
    },
    "constraint_sweep": {
        "x_field": "constraints",
        "x_label": "Constraints",
        "file_suffix": "constraint",
        "summary_label": "Constraint sweep",
        "table_label": "Constraint",
        "axis_label": "Rules",
        "claim_prefix": "Constraint",
    },
    "history_sweep": {
        "x_field": "history_depth",
        "x_label": "History depth",
        "file_suffix": "history",
        "summary_label": "History sweep",
        "table_label": "History",
        "axis_label": "Depth",
        "claim_prefix": "History",
    },
    "periodic_sweep": {
        "x_field": "periodic_constraints",
        "x_label": "Periodic constraints",
        "file_suffix": "periodic",
        "summary_label": "Periodic sweep",
        "table_label": "Periodic",
        "axis_label": "Periodic rules",
        "claim_prefix": "Periodic",
    },
    "mixed_feature_sweep": {
        "x_field": "history_depth",
        "x_label": "Mixed level",
        "file_suffix": "mixed",
        "summary_label": "Mixed history+periodic sweep",
        "table_label": "Mixed",
        "axis_label": "Mixed level",
        "claim_prefix": "Mixed",
    },
    "burst_sweep": {
        "x_field": "burst_size",
        "x_label": "Burst size",
        "file_suffix": "burst",
        "summary_label": "Bursty trace sweep",
        "table_label": "Burst",
        "axis_label": "Burst size",
        "claim_prefix": "Burst",
    },
    "hotset_sweep": {
        "x_field": "hotset_size",
        "x_label": "Hot-set size",
        "file_suffix": "hotset",
        "summary_label": "Rotating hot-set sweep",
        "table_label": "Hot-set",
        "axis_label": "Hot-set size",
        "claim_prefix": "Hotset",
    },
    "soak_sweep": {
        "x_field": "events_requested",
        "x_label": "Trace length",
        "file_suffix": "soak",
        "summary_label": "Long-run soak sweep",
        "table_label": "Soak",
        "axis_label": "Events",
        "claim_prefix": "Soak",
    },
}

MATRIX_SPEC = {
    "family": "matrix_sweep",
    "file_suffix": "matrix",
    "summary_label": "Object x rule matrix",
    "table_label": "Matrix",
    "axis_label": "Objects x rules",
    "claim_prefix": "Matrix",
}


def parse_float(value: str) -> float | None:
    if value in ("", "NA", None):
        return None
    return float(value)


def parse_int(value: str) -> int | None:
    if value in ("", "NA", None):
        return None
    return int(value)


def fmt_range(values: list[float]) -> str:
    return f"{min(values):.3f}-{max(values):.3f}"


def fmt_display_value(value: float) -> str:
    if value >= 10000:
        return f"{value:,.0f}"
    if value >= 1000:
        return f"{value:,.1f}"
    if value >= 100:
        return f"{value:.1f}"
    if value >= 10:
        return f"{value:.2f}"
    return f"{value:.3f}"


def fmt_display_range(values: list[float]) -> str:
    return f"{fmt_display_value(min(values))}-{fmt_display_value(max(values))}"


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: prepare_plot_data.py <manifest.tsv>", file=sys.stderr)
        return 1

    manifest = Path(sys.argv[1]).resolve()
    if not manifest.is_file():
        print(f"manifest not found: {manifest}", file=sys.stderr)
        return 1

    out_dir = manifest.parent
    base_name = manifest.stem

    grouped: dict[str, dict[int, list[dict[str, float]]]] = {key: {} for key in LINEAR_FAMILY_SPECS}
    matrix_grouped: dict[tuple[int, int], list[dict[str, float]]] = {}
    family_claims: dict[str, dict[str, object]] = {}
    run_count = 0
    processed_total = 0
    duration_total = 0.0

    with manifest.open(newline="") as f:
        reader = csv.DictReader(f, delimiter="\t")
        for row in reader:
            family = row.get("family", "")
            avg_ms = parse_float(row.get("avg_ms", ""))
            duration_s = parse_float(row.get("duration_s", ""))
            processed = parse_int(row.get("events_processed", ""))
            throughput = parse_float(row.get("events_per_s", ""))

            if avg_ms is None and duration_s is not None and processed:
                avg_ms = (duration_s * 1000.0) / processed

            if avg_ms is None or throughput is None:
                continue

            run_count += 1
            if processed is not None:
                processed_total += processed
            if duration_s is not None:
                duration_total += duration_s

            point = {
                "avg_ms": avg_ms,
                "throughput": throughput,
            }

            if family in LINEAR_FAMILY_SPECS:
                spec = LINEAR_FAMILY_SPECS[family]
                x_field = spec["x_field"]
                x_value = parse_int(row.get(x_field, ""))
                if x_value is None:
                    continue
                grouped[family].setdefault(x_value, []).append(point)
                continue

            if family == MATRIX_SPEC["family"]:
                objects = parse_int(row.get("objects", ""))
                constraints = parse_int(row.get("constraints", ""))
                if objects is None or constraints is None:
                    continue
                matrix_grouped.setdefault((objects, constraints), []).append(point)

    summary_rows: list[dict[str, str]] = []

    for family, spec in LINEAR_FAMILY_SPECS.items():
        x_groups = grouped[family]
        if not x_groups:
            continue

        plot_path = out_dir / f"{base_name}_{spec['file_suffix']}_plot.tsv"
        with plot_path.open("w", newline="") as f:
            writer = csv.writer(f, delimiter="\t")
            writer.writerow(
                [
                    "x",
                    "avg_ms_median",
                    "avg_ms_min",
                    "avg_ms_max",
                    "avg_ms_err_minus",
                    "avg_ms_err_plus",
                    "throughput_median",
                    "throughput_min",
                    "throughput_max",
                    "throughput_err_minus",
                    "throughput_err_plus",
                ]
            )
            for x in sorted(x_groups):
                avg_values = [point["avg_ms"] for point in x_groups[x]]
                throughput_values = [point["throughput"] for point in x_groups[x]]
                avg_median = statistics.median(avg_values)
                thr_median = statistics.median(throughput_values)
                avg_min = min(avg_values)
                avg_max = max(avg_values)
                thr_min = min(throughput_values)
                thr_max = max(throughput_values)
                writer.writerow(
                    [
                        x,
                        f"{avg_median:.3f}",
                        f"{avg_min:.3f}",
                        f"{avg_max:.3f}",
                        f"{avg_median - avg_min:.3f}",
                        f"{avg_max - avg_median:.3f}",
                        f"{thr_median:.3f}",
                        f"{thr_min:.3f}",
                        f"{thr_max:.3f}",
                        f"{thr_median - thr_min:.3f}",
                        f"{thr_max - thr_median:.3f}",
                    ]
                )

        x_values = sorted(x_groups)
        avg_medians = [statistics.median(point["avg_ms"] for point in x_groups[x]) for x in x_values]
        thr_medians = [statistics.median(point["throughput"] for point in x_groups[x]) for x in x_values]
        repetitions = max(len(points) for points in x_groups.values())
        family_claims[family] = {
            "type": "linear",
            "prefix": spec["claim_prefix"],
            "x_values": x_values,
            "avg_medians": avg_medians,
            "thr_medians": thr_medians,
            "repetitions": repetitions,
        }
        summary_rows.append(
            {
                "family": family,
                "summary_label": spec["summary_label"],
                "table_label": spec["table_label"],
                "points": str(len(x_values)),
                "x_label": spec["x_label"],
                "axis_span": f"{spec['axis_label']} {x_values[0]}-{x_values[-1]}",
                "x_range": f"{x_values[0]}-{x_values[-1]}",
                "avg_ms_range": fmt_range(avg_medians),
                "avg_ms_range_display": fmt_display_range(avg_medians),
                "throughput_range": fmt_range(thr_medians),
                "throughput_range_display": fmt_display_range(thr_medians),
                "repetitions": str(repetitions),
            }
        )

    if matrix_grouped:
        plot_path = out_dir / f"{base_name}_{MATRIX_SPEC['file_suffix']}_plot.tsv"
        object_values: set[int] = set()
        constraint_values: set[int] = set()
        avg_medians: list[float] = []
        thr_medians: list[float] = []
        repetitions = 0
        with plot_path.open("w", newline="") as f:
            writer = csv.writer(f, delimiter="\t")
            writer.writerow(
                [
                    "objects",
                    "constraints",
                    "avg_ms_median",
                    "avg_ms_min",
                    "avg_ms_max",
                    "throughput_median",
                    "throughput_min",
                    "throughput_max",
                    "repetitions",
                ]
            )
            ordered_keys = sorted(matrix_grouped, key=lambda item: (item[1], item[0]))
            for objects, constraints in ordered_keys:
                samples = matrix_grouped[(objects, constraints)]
                avg_values = [point["avg_ms"] for point in samples]
                throughput_values = [point["throughput"] for point in samples]
                avg_median = statistics.median(avg_values)
                thr_median = statistics.median(throughput_values)
                writer.writerow(
                    [
                        objects,
                        constraints,
                        f"{avg_median:.3f}",
                        f"{min(avg_values):.3f}",
                        f"{max(avg_values):.3f}",
                        f"{thr_median:.3f}",
                        f"{min(throughput_values):.3f}",
                        f"{max(throughput_values):.3f}",
                        str(len(samples)),
                    ]
                )
                object_values.add(objects)
                constraint_values.add(constraints)
                avg_medians.append(avg_median)
                thr_medians.append(thr_median)
                repetitions = max(repetitions, len(samples))

        object_min = min(object_values)
        object_max = max(object_values)
        constraint_min = min(constraint_values)
        constraint_max = max(constraint_values)
        summary_rows.append(
            {
                "family": MATRIX_SPEC["family"],
                "summary_label": MATRIX_SPEC["summary_label"],
                "table_label": MATRIX_SPEC["table_label"],
                "points": str(len(matrix_grouped)),
                "x_label": MATRIX_SPEC["axis_label"],
                "axis_span": f"Objects {object_min}-{object_max} x Rules {constraint_min}-{constraint_max}",
                "x_range": f"{object_min}-{object_max} x {constraint_min}-{constraint_max}",
                "avg_ms_range": fmt_range(avg_medians),
                "avg_ms_range_display": fmt_display_range(avg_medians),
                "throughput_range": fmt_range(thr_medians),
                "throughput_range_display": fmt_display_range(thr_medians),
                "repetitions": str(repetitions),
            }
        )
        family_claims[MATRIX_SPEC["family"]] = {
            "type": "matrix",
            "prefix": MATRIX_SPEC["claim_prefix"],
            "object_min": object_min,
            "object_max": object_max,
            "constraint_min": constraint_min,
            "constraint_max": constraint_max,
            "avg_medians": avg_medians,
            "thr_medians": thr_medians,
            "points": len(matrix_grouped),
            "repetitions": repetitions,
        }

    control_points_total = sum(int(row["points"]) for row in summary_rows)

    summary_tsv = out_dir / f"{base_name}_family_summary.tsv"
    with summary_tsv.open("w", newline="") as f:
        writer = csv.writer(f, delimiter="\t")
        writer.writerow(
            [
                "sweep",
                "points",
                "control_axis",
                "control_range",
                "median_latency_ms_range",
                "median_throughput_range",
                "repetitions",
            ]
        )
        for row in summary_rows:
            writer.writerow(
                [
                    row["summary_label"],
                    row["points"],
                    row["x_label"],
                    row["x_range"],
                    row["avg_ms_range"],
                    row["throughput_range"],
                    row["repetitions"],
                ]
            )

    summary_md = out_dir / f"{base_name}_family_summary.md"
    with summary_md.open("w") as f:
        f.write("# RQ2 Showcase Family Summary\n\n")
        f.write(f"Source manifest: `{manifest}`\n\n")
        f.write(
            "| Sweep | Axis span | Points | Median latency (ms) | Median throughput (events/s) | Repetitions |\n"
        )
        f.write("| --- | --- | ---: | --- | --- | ---: |\n")
        for row in summary_rows:
            f.write(
                f"| {row['summary_label']} | {row['axis_span']} | {row['points']} | {row['avg_ms_range_display']} | {row['throughput_range_display']} | {row['repetitions']} |\n"
            )

    summary_tex = out_dir / f"{base_name}_family_summary.tex"
    with summary_tex.open("w") as f:
        f.write("% Auto-generated from showcase manifest\n")
        f.write("\\fontsize{6}{7.0}\\selectfont\n")
        f.write("\\setlength{\\tabcolsep}{1.8pt}\n")
        f.write("\\renewcommand{\\arraystretch}{1.03}\n")
        f.write(
            "\\begin{tabular}{>{\\raggedright\\arraybackslash}p{0.125\\linewidth}>{\\raggedright\\arraybackslash}p{0.205\\linewidth}>{\\centering\\arraybackslash}p{0.05\\linewidth}>{\\centering\\arraybackslash}p{0.175\\linewidth}>{\\centering\\arraybackslash}p{0.22\\linewidth}>{\\centering\\arraybackslash}p{0.05\\linewidth}}\n"
        )
        f.write("\\toprule\n")
        f.write(
            "Family & Axis span & Pts & Latency span (ms) & Throughput span (events/s) & Reps \\\\\n"
        )
        f.write("\\midrule\n")
        for row in summary_rows:
            f.write(
                f"{row['table_label']} & {row['axis_span']} & {row['points']} & {row['avg_ms_range_display']} & {row['throughput_range_display']} & {row['repetitions']} \\\\\n"
            )
        f.write("\\bottomrule\n")
        f.write("\\end{tabular}\n")

    highlights_tex = out_dir / f"{base_name}_highlights.tex"
    with highlights_tex.open("w") as f:
        f.write("% Auto-generated workload highlights\n")
        f.write(f"\\def\\rqTwoFamilyCount{{{len(summary_rows)}}}\n")
        f.write(f"\\def\\rqTwoControlPointCount{{{control_points_total}}}\n")
        f.write(f"\\def\\rqTwoRunCount{{{run_count}}}\n")
        f.write(f"\\def\\rqTwoProcessedEventCount{{{processed_total}}}\n")
        f.write(f"\\def\\rqTwoDurationMinutes{{{duration_total / 60.0:.1f}}}\n")

    claims_tex = out_dir / f"{base_name}_claims.tex"
    with claims_tex.open("w") as f:
        f.write("% Auto-generated family-level claim macros\n")
        for family in LINEAR_FAMILY_SPECS:
            claim = family_claims.get(family)
            if claim is None:
                continue
            prefix = claim["prefix"]
            x_values = claim["x_values"]
            avg_medians = claim["avg_medians"]
            thr_medians = claim["thr_medians"]
            repetitions = claim["repetitions"]
            f.write(f"\\def\\rqTwo{prefix}PointCount{{{len(x_values)}}}\n")
            f.write(f"\\def\\rqTwo{prefix}Repetitions{{{repetitions}}}\n")
            f.write(f"\\def\\rqTwo{prefix}XStart{{{x_values[0]}}}\n")
            f.write(f"\\def\\rqTwo{prefix}XEnd{{{x_values[-1]}}}\n")
            f.write(f"\\def\\rqTwo{prefix}LatencyStart{{{fmt_display_value(avg_medians[0])}}}\n")
            f.write(f"\\def\\rqTwo{prefix}LatencyEnd{{{fmt_display_value(avg_medians[-1])}}}\n")
            f.write(f"\\def\\rqTwo{prefix}LatencyMin{{{fmt_display_value(min(avg_medians))}}}\n")
            f.write(f"\\def\\rqTwo{prefix}LatencyMax{{{fmt_display_value(max(avg_medians))}}}\n")
            f.write(
                f"\\def\\rqTwo{prefix}LatencySpan{{{fmt_display_value(max(avg_medians) - min(avg_medians))}}}\n"
            )
            f.write(f"\\def\\rqTwo{prefix}ThroughputStart{{{fmt_display_value(thr_medians[0])}}}\n")
            f.write(f"\\def\\rqTwo{prefix}ThroughputEnd{{{fmt_display_value(thr_medians[-1])}}}\n")
            f.write(f"\\def\\rqTwo{prefix}ThroughputMin{{{fmt_display_value(min(thr_medians))}}}\n")
            f.write(f"\\def\\rqTwo{prefix}ThroughputMax{{{fmt_display_value(max(thr_medians))}}}\n")
            f.write(
                f"\\def\\rqTwo{prefix}ThroughputSpan{{{fmt_display_value(max(thr_medians) - min(thr_medians))}}}\n"
            )

        matrix_claim = family_claims.get(MATRIX_SPEC["family"])
        if matrix_claim is not None:
            prefix = matrix_claim["prefix"]
            avg_medians = matrix_claim["avg_medians"]
            thr_medians = matrix_claim["thr_medians"]
            f.write(f"\\def\\rqTwo{prefix}PointCount{{{matrix_claim['points']}}}\n")
            f.write(f"\\def\\rqTwo{prefix}Repetitions{{{matrix_claim['repetitions']}}}\n")
            f.write(f"\\def\\rqTwo{prefix}ObjectMin{{{matrix_claim['object_min']}}}\n")
            f.write(f"\\def\\rqTwo{prefix}ObjectMax{{{matrix_claim['object_max']}}}\n")
            f.write(f"\\def\\rqTwo{prefix}ConstraintMin{{{matrix_claim['constraint_min']}}}\n")
            f.write(f"\\def\\rqTwo{prefix}ConstraintMax{{{matrix_claim['constraint_max']}}}\n")
            f.write(f"\\def\\rqTwo{prefix}LatencyMin{{{fmt_display_value(min(avg_medians))}}}\n")
            f.write(f"\\def\\rqTwo{prefix}LatencyMax{{{fmt_display_value(max(avg_medians))}}}\n")
            f.write(
                f"\\def\\rqTwo{prefix}LatencySpan{{{fmt_display_value(max(avg_medians) - min(avg_medians))}}}\n"
            )
            f.write(f"\\def\\rqTwo{prefix}ThroughputMin{{{fmt_display_value(min(thr_medians))}}}\n")
            f.write(f"\\def\\rqTwo{prefix}ThroughputMax{{{fmt_display_value(max(thr_medians))}}}\n")
            f.write(
                f"\\def\\rqTwo{prefix}ThroughputSpan{{{fmt_display_value(max(thr_medians) - min(thr_medians))}}}\n"
            )

    print(f"wrote {summary_tsv}")
    print(f"wrote {summary_md}")
    print(f"wrote {summary_tex}")
    print(f"wrote {highlights_tex}")
    print(f"wrote {claims_tex}")
    for family, spec in LINEAR_FAMILY_SPECS.items():
        plot_path = out_dir / f"{base_name}_{spec['file_suffix']}_plot.tsv"
        if plot_path.exists():
            print(f"wrote {plot_path}")
    matrix_plot_path = out_dir / f"{base_name}_{MATRIX_SPEC['file_suffix']}_plot.tsv"
    if matrix_plot_path.exists():
        print(f"wrote {matrix_plot_path}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
