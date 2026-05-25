#!/usr/bin/env python3

from __future__ import annotations

import csv
import argparse
import hashlib
import json
import re
import statistics
import sys
from collections import Counter, defaultdict
from pathlib import Path


LINEAR_FAMILIES = {
    "object_sweep": ("object", "objects", 12, 5),
    "constraint_sweep": ("constraint", "constraints", 12, 5),
    "history_sweep": ("history", "history_depth", 10, 5),
    "periodic_sweep": ("periodic", "periodic_constraints", 10, 5),
    "mixed_feature_sweep": ("mixed", "history_depth", 9, 3),
    "burst_sweep": ("burst", "burst_size", 6, 3),
    "hotset_sweep": ("hotset", "hotset_size", 6, 3),
    "soak_sweep": ("soak", "events_requested", 8, 2),
}

SUMMARY_LABELS = {
    "object_sweep": "object sweep",
    "constraint_sweep": "constraint sweep",
    "history_sweep": "history sweep",
    "periodic_sweep": "periodic sweep",
    "mixed_feature_sweep": "mixed history+periodic sweep",
    "burst_sweep": "bursty trace sweep",
    "hotset_sweep": "rotating hot-set sweep",
    "soak_sweep": "long-run soak sweep",
}

MATRIX_FAMILY = "matrix_sweep"
MATRIX_SUFFIX = "matrix"
MATRIX_EXPECTED_POINTS = 49
MATRIX_EXPECTED_REPETITIONS = 3
EXPECTED_RUNS = sum(points * reps for *_rest, points, reps in LINEAR_FAMILIES.values()) + (
    MATRIX_EXPECTED_POINTS * MATRIX_EXPECTED_REPETITIONS
)

MANIFEST_COLUMNS = {
    "label",
    "objects",
    "constraints",
    "events_requested",
    "events_processed",
    "avg_ms",
    "duration_s",
    "events_per_s",
    "memory_mode",
    "memory_value",
    "memory_note",
    "history_depth",
    "periodic_constraints",
    "periodic_hz",
    "burst_size",
    "hotset_size",
    "phase_length",
    "time_step_ms",
    "burst_gap_ms",
    "family",
    "repetition",
}

LINEAR_PLOT_COLUMNS = [
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

MATRIX_PLOT_COLUMNS = [
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

TEX_DEF_PATTERN = re.compile(r"\\def\\([A-Za-z]+)\{([^}]*)\}")

SUMMARY_COLUMNS = [
    "sweep",
    "points",
    "control_axis",
    "control_range",
    "median_latency_ms_range",
    "median_throughput_range",
    "repetitions",
]


def fail(message: str) -> None:
    print(f"verify_rq2_outputs: {message}", file=sys.stderr)
    raise SystemExit(1)


def read_tsv(path: Path) -> list[dict[str, str]]:
    if not path.is_file():
        fail(f"missing file: {path}")
    with path.open(newline="") as f:
        reader = csv.DictReader(f, delimiter="\t")
        if reader.fieldnames is None:
            fail(f"missing TSV header: {path}")
        return list(reader)


def require_columns(path: Path, actual: list[str] | None, expected: list[str] | set[str]) -> None:
    if actual is None:
        fail(f"missing header: {path}")
    missing = [col for col in expected if col not in actual]
    if missing:
        fail(f"{path} missing columns: {', '.join(missing)}")


def parse_positive_float(path: Path, row_label: str, field: str, value: str) -> float:
    try:
        parsed = float(value)
    except ValueError as exc:
        fail(f"{path} row {row_label}: {field} is not a float: {value!r}")
        raise AssertionError from exc
    if parsed < 0:
        fail(f"{path} row {row_label}: {field} must be non-negative, got {value!r}")
    return parsed


def parse_positive_int(path: Path, row_label: str, field: str, value: str) -> int:
    try:
        parsed = int(value)
    except ValueError as exc:
        fail(f"{path} row {row_label}: {field} is not an integer: {value!r}")
        raise AssertionError from exc
    if parsed < 0:
        fail(f"{path} row {row_label}: {field} must be non-negative, got {value!r}")
    return parsed


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


def validate_manifest(
    manifest: Path,
) -> tuple[str, dict[str, set[int]], dict[str, Counter[int]], int, int, float]:
    with manifest.open(newline="") as f:
        reader = csv.DictReader(f, delimiter="\t")
        require_columns(manifest, reader.fieldnames, MANIFEST_COLUMNS)
        rows = list(reader)

    if not rows:
        fail(f"manifest has no data rows: {manifest}")

    family_points: dict[str, set[int]] = defaultdict(set)
    family_repetition_counts: dict[str, Counter[int]] = defaultdict(Counter)
    matrix_points: set[tuple[int, int]] = set()
    matrix_repetition_counts: Counter[tuple[int, int]] = Counter()
    processed_total = 0
    duration_total = 0.0

    for idx, row in enumerate(rows, start=2):
        label = row.get("label") or f"line {idx}"
        family = row.get("family", "")
        if family not in LINEAR_FAMILIES and family != MATRIX_FAMILY:
            fail(f"{manifest} row {label}: unknown family {family!r}")

        requested = parse_positive_int(manifest, label, "events_requested", row["events_requested"])
        processed = parse_positive_int(manifest, label, "events_processed", row["events_processed"])
        if processed != requested:
            fail(
                f"{manifest} row {label}: events_processed={processed} "
                f"does not match events_requested={requested}"
            )
        processed_total += processed

        parse_positive_float(manifest, label, "avg_ms", row["avg_ms"])
        duration_total += parse_positive_float(manifest, label, "duration_s", row["duration_s"])
        parse_positive_float(manifest, label, "events_per_s", row["events_per_s"])
        parse_positive_int(manifest, label, "repetition", row["repetition"])

        if family in LINEAR_FAMILIES:
            _suffix, x_field, _expected_points, _expected_repetitions = LINEAR_FAMILIES[family]
            point = parse_positive_int(manifest, label, x_field, row[x_field])
            family_points[family].add(point)
            family_repetition_counts[family][point] += 1
        else:
            objects = parse_positive_int(manifest, label, "objects", row["objects"])
            constraints = parse_positive_int(manifest, label, "constraints", row["constraints"])
            point = (objects, constraints)
            matrix_points.add(point)
            matrix_repetition_counts[point] += 1

    if len(rows) != EXPECTED_RUNS:
        fail(f"{manifest} has {len(rows)} measured runs; expected {EXPECTED_RUNS}")

    for family, (_suffix, _x_field, expected_points, expected_repetitions) in LINEAR_FAMILIES.items():
        observed = len(family_points[family])
        if observed != expected_points:
            fail(f"{family} has {observed} control points; expected {expected_points}")
        for point in sorted(family_points[family]):
            observed_repetitions = family_repetition_counts[family][point]
            if observed_repetitions != expected_repetitions:
                fail(
                    f"{family} point {point} has {observed_repetitions} runs; "
                    f"expected {expected_repetitions}"
                )

    if len(matrix_points) != MATRIX_EXPECTED_POINTS:
        fail(
            f"{MATRIX_FAMILY} has {len(matrix_points)} object/rule points; "
            f"expected {MATRIX_EXPECTED_POINTS}"
        )
    for point in sorted(matrix_points):
        observed_repetitions = matrix_repetition_counts[point]
        if observed_repetitions != MATRIX_EXPECTED_REPETITIONS:
            fail(
                f"{MATRIX_FAMILY} point {point} has {observed_repetitions} runs; "
                f"expected {MATRIX_EXPECTED_REPETITIONS}"
            )

    return manifest.stem, family_points, family_repetition_counts, len(rows), processed_total, duration_total


def validate_plot(path: Path, expected_columns: list[str], expected_rows: int) -> None:
    with path.open(newline="") as f:
        reader = csv.DictReader(f, delimiter="\t")
        require_columns(path, reader.fieldnames, expected_columns)
        rows = list(reader)
    if len(rows) != expected_rows:
        fail(f"{path} has {len(rows)} data rows; expected {expected_rows}")
    for idx, row in enumerate(rows, start=2):
        label = f"line {idx}"
        for field in expected_columns:
            if field in ("x", "objects", "constraints", "repetitions"):
                parse_positive_int(path, label, field, row[field])
            else:
                parse_positive_float(path, label, field, row[field])


def validate_summary_files(
    out_dir: Path,
    base_name: str,
    family_points: dict[str, set[int]],
    family_repetition_counts: dict[str, Counter[int]],
) -> None:
    summary_tsv = out_dir / f"{base_name}_family_summary.tsv"
    with summary_tsv.open(newline="") as f:
        reader = csv.DictReader(f, delimiter="\t")
        require_columns(summary_tsv, reader.fieldnames, SUMMARY_COLUMNS)
        rows = list(reader)
    expected_rows = len(LINEAR_FAMILIES) + 1
    if len(rows) != expected_rows:
        fail(f"{summary_tsv} has {len(rows)} rows; expected {expected_rows}")

    expected_summary = {}
    for family, (_suffix, _x_field, expected_points, expected_repetitions) in LINEAR_FAMILIES.items():
        expected_summary[SUMMARY_LABELS[family]] = (expected_points, expected_repetitions)
    expected_summary["object x rule matrix"] = (MATRIX_EXPECTED_POINTS, MATRIX_EXPECTED_REPETITIONS)

    for row in rows:
        label = row["sweep"].lower()
        if label not in expected_summary:
            fail(f"{summary_tsv} has unexpected summary row: {row['sweep']!r}")
        expected_points, expected_repetitions = expected_summary[label]
        observed_points = parse_positive_int(summary_tsv, row["sweep"], "points", row["points"])
        observed_repetitions = parse_positive_int(
            summary_tsv,
            row["sweep"],
            "repetitions",
            row["repetitions"],
        )
        if observed_points != expected_points:
            fail(f"{summary_tsv} row {row['sweep']}: points={observed_points}; expected {expected_points}")
        if observed_repetitions != expected_repetitions:
            fail(
                f"{summary_tsv} row {row['sweep']}: repetitions={observed_repetitions}; "
                f"expected {expected_repetitions}"
            )

    for suffix in ("family_summary.md", "family_summary.tex", "highlights.tex", "claims.tex"):
        path = out_dir / f"{base_name}_{suffix}"
        if not path.is_file():
            fail(f"missing summary file: {path}")
        if path.stat().st_size == 0:
            fail(f"empty summary file: {path}")


def read_tex_defines(path: Path) -> dict[str, str]:
    if not path.is_file():
        fail(f"missing TeX macro file: {path}")
    macros: dict[str, str] = {}
    for match in TEX_DEF_PATTERN.finditer(path.read_text(encoding="utf-8")):
        name, value = match.groups()
        if name in macros:
            fail(f"{path} defines \\{name} more than once")
        macros[name] = value
    if not macros:
        fail(f"{path} does not contain any \\def macros")
    return macros


def require_tex_macro(path: Path, macros: dict[str, str], name: str, expected: object) -> None:
    expected_value = str(expected)
    actual = macros.get(name)
    if actual is None:
        fail(f"{path} missing \\{name}")
    if actual != expected_value:
        fail(f"{path} \\{name}={actual!r}; expected {expected_value!r}")


def require_text_contains(path: Path, text: str, needle: str) -> None:
    if needle not in text:
        fail(f"{path} does not contain expected paper-facing value: {needle!r}")


def require_text_contains_any(path: Path, text: str, needles: list[str], label: str) -> None:
    if not any(needle in text for needle in needles):
        fail(
            f"{path} does not contain any expected paper-facing value for {label}: "
            f"{', '.join(repr(needle) for needle in needles)}"
        )


def count_phrase(count: int, noun: str, plural: str | None = None) -> list[str]:
    words = {
        1: "one",
        2: "two",
        3: "three",
        4: "four",
        5: "five",
        6: "six",
        7: "seven",
        8: "eight",
        9: "nine",
    }
    plural_noun = plural or f"{noun}s"
    phrases = [f"{count}-{noun}"]
    phrases.append(f"{count} {plural_noun}")
    if count in words:
        phrases.append(f"{words[count]}-{noun}")
        phrases.append(f"{words[count]} {plural_noun}")
    return phrases


def collect_claim_inputs(manifest: Path) -> tuple[dict[str, dict[str, object]], dict[str, object]]:
    grouped: dict[str, dict[int, list[dict[str, float]]]] = defaultdict(lambda: defaultdict(list))
    matrix_grouped: dict[tuple[int, int], list[dict[str, float]]] = defaultdict(list)

    with manifest.open(newline="") as f:
        reader = csv.DictReader(f, delimiter="\t")
        for idx, row in enumerate(reader, start=2):
            label = row.get("label") or f"line {idx}"
            family = row.get("family", "")
            avg_ms = parse_positive_float(manifest, label, "avg_ms", row["avg_ms"])
            throughput = parse_positive_float(manifest, label, "events_per_s", row["events_per_s"])
            point = {"avg_ms": avg_ms, "throughput": throughput}

            if family in LINEAR_FAMILIES:
                _suffix, x_field, _expected_points, _expected_repetitions = LINEAR_FAMILIES[family]
                x_value = parse_positive_int(manifest, label, x_field, row[x_field])
                grouped[family][x_value].append(point)
            elif family == MATRIX_FAMILY:
                objects = parse_positive_int(manifest, label, "objects", row["objects"])
                constraints = parse_positive_int(manifest, label, "constraints", row["constraints"])
                matrix_grouped[(objects, constraints)].append(point)

    linear_claims: dict[str, dict[str, object]] = {}
    for family, (_suffix, _x_field, _expected_points, _expected_repetitions) in LINEAR_FAMILIES.items():
        x_groups = grouped[family]
        x_values = sorted(x_groups)
        if not x_values:
            fail(f"{manifest}: no rows available for {family}")
        linear_claims[family] = {
            "x_values": x_values,
            "avg_medians": [
                statistics.median(point["avg_ms"] for point in x_groups[x])
                for x in x_values
            ],
            "throughput_medians": [
                statistics.median(point["throughput"] for point in x_groups[x])
                for x in x_values
            ],
            "repetitions": max(len(points) for points in x_groups.values()),
        }

    ordered_matrix_keys = sorted(matrix_grouped, key=lambda item: (item[1], item[0]))
    if not ordered_matrix_keys:
        fail(f"{manifest}: no rows available for {MATRIX_FAMILY}")
    matrix_claim = {
        "objects": [objects for objects, _constraints in ordered_matrix_keys],
        "constraints": [constraints for _objects, constraints in ordered_matrix_keys],
        "avg_medians": [
            statistics.median(point["avg_ms"] for point in matrix_grouped[key])
            for key in ordered_matrix_keys
        ],
        "throughput_medians": [
            statistics.median(point["throughput"] for point in matrix_grouped[key])
            for key in ordered_matrix_keys
        ],
        "repetitions": max(len(matrix_grouped[key]) for key in ordered_matrix_keys),
        "points": len(ordered_matrix_keys),
    }

    return linear_claims, matrix_claim


def validate_linear_claim_macros(
    path: Path,
    macros: dict[str, str],
    prefix: str,
    x_values: list[int],
    avg_medians: list[float],
    throughput_medians: list[float],
    repetitions: int,
) -> None:
    if not x_values:
        fail(f"cannot validate \\rqTwo{prefix}* macros without plot rows")

    base = f"rqTwo{prefix}"

    require_tex_macro(path, macros, f"{base}PointCount", len(x_values))
    require_tex_macro(path, macros, f"{base}Repetitions", repetitions)
    require_tex_macro(path, macros, f"{base}XStart", x_values[0])
    require_tex_macro(path, macros, f"{base}XEnd", x_values[-1])
    require_tex_macro(path, macros, f"{base}LatencyStart", fmt_display_value(avg_medians[0]))
    require_tex_macro(path, macros, f"{base}LatencyEnd", fmt_display_value(avg_medians[-1]))
    require_tex_macro(path, macros, f"{base}LatencyMin", fmt_display_value(min(avg_medians)))
    require_tex_macro(path, macros, f"{base}LatencyMax", fmt_display_value(max(avg_medians)))
    require_tex_macro(path, macros, f"{base}LatencySpan", fmt_display_value(max(avg_medians) - min(avg_medians)))
    require_tex_macro(path, macros, f"{base}ThroughputStart", fmt_display_value(throughput_medians[0]))
    require_tex_macro(path, macros, f"{base}ThroughputEnd", fmt_display_value(throughput_medians[-1]))
    require_tex_macro(path, macros, f"{base}ThroughputMin", fmt_display_value(min(throughput_medians)))
    require_tex_macro(path, macros, f"{base}ThroughputMax", fmt_display_value(max(throughput_medians)))
    require_tex_macro(
        path,
        macros,
        f"{base}ThroughputSpan",
        fmt_display_value(max(throughput_medians) - min(throughput_medians)),
    )


def validate_matrix_claim_macros(
    path: Path,
    macros: dict[str, str],
    matrix_claim: dict[str, object],
) -> None:
    objects = matrix_claim["objects"]
    constraints = matrix_claim["constraints"]
    avg_medians = matrix_claim["avg_medians"]
    throughput_medians = matrix_claim["throughput_medians"]
    repetitions = matrix_claim["repetitions"]

    base = "rqTwoMatrix"
    require_tex_macro(path, macros, f"{base}PointCount", matrix_claim["points"])
    require_tex_macro(path, macros, f"{base}Repetitions", repetitions)
    require_tex_macro(path, macros, f"{base}ObjectMin", min(objects))
    require_tex_macro(path, macros, f"{base}ObjectMax", max(objects))
    require_tex_macro(path, macros, f"{base}ConstraintMin", min(constraints))
    require_tex_macro(path, macros, f"{base}ConstraintMax", max(constraints))
    require_tex_macro(path, macros, f"{base}LatencyMin", fmt_display_value(min(avg_medians)))
    require_tex_macro(path, macros, f"{base}LatencyMax", fmt_display_value(max(avg_medians)))
    require_tex_macro(path, macros, f"{base}LatencySpan", fmt_display_value(max(avg_medians) - min(avg_medians)))
    require_tex_macro(path, macros, f"{base}ThroughputMin", fmt_display_value(min(throughput_medians)))
    require_tex_macro(path, macros, f"{base}ThroughputMax", fmt_display_value(max(throughput_medians)))
    require_tex_macro(
        path,
        macros,
        f"{base}ThroughputSpan",
        fmt_display_value(max(throughput_medians) - min(throughput_medians)),
    )


def validate_tex_macro_files(
    manifest: Path,
    out_dir: Path,
    base_name: str,
    row_count: int,
    processed_total: int,
    duration_total: float,
    family_repetition_counts: dict[str, Counter[int]],
) -> None:
    highlights_path = out_dir / f"{base_name}_highlights.tex"
    claims_path = out_dir / f"{base_name}_claims.tex"
    highlights = read_tex_defines(highlights_path)
    claims = read_tex_defines(claims_path)
    linear_claims, matrix_claim = collect_claim_inputs(manifest)

    control_points_total = 0
    for family, (suffix, _x_field, _expected_points, _expected_repetitions) in LINEAR_FAMILIES.items():
        rows = read_tsv(out_dir / f"{base_name}_{suffix}_plot.tsv")
        control_points_total += len(rows)

    matrix_rows = read_tsv(out_dir / f"{base_name}_{MATRIX_SUFFIX}_plot.tsv")
    control_points_total += len(matrix_rows)

    require_tex_macro(highlights_path, highlights, "rqTwoFamilyCount", len(LINEAR_FAMILIES) + 1)
    require_tex_macro(highlights_path, highlights, "rqTwoControlPointCount", control_points_total)
    require_tex_macro(highlights_path, highlights, "rqTwoRunCount", row_count)
    require_tex_macro(highlights_path, highlights, "rqTwoProcessedEventCount", processed_total)
    require_tex_macro(highlights_path, highlights, "rqTwoDurationMinutes", f"{duration_total / 60.0:.1f}")

    for family, (suffix, _x_field, _expected_points, _expected_repetitions) in LINEAR_FAMILIES.items():
        prefix = suffix.title()
        claim = linear_claims[family]
        repetitions = max(family_repetition_counts[family].values())
        validate_linear_claim_macros(
            claims_path,
            claims,
            prefix,
            claim["x_values"],
            claim["avg_medians"],
            claim["throughput_medians"],
            repetitions,
        )

    validate_matrix_claim_macros(claims_path, claims, matrix_claim)


def validate_manuscript_rq2_headlines(
    manifest: Path,
    row_count: int,
    family_count: int,
    linear_claims: dict[str, dict[str, object]],
    matrix_claim: dict[str, object],
) -> None:
    repo_root = manifest.parents[4]
    main_tex = repo_root / "paper" / "main.tex"
    abstract_tex = repo_root / "paper" / "sections" / "abstract.tex"
    evaluation_tex = repo_root / "paper" / "sections" / "evaluation.tex"

    if not main_tex.is_file():
        fail(f"missing manuscript file: {main_tex}")

    main_text = main_tex.read_text(encoding="utf-8")
    require_text_contains(main_tex, main_text, r"\input{sections/abstract}")
    require_text_contains(main_tex, main_text, r"\input{sections/evaluation}")
    require_text_contains(main_tex, main_text, "showcase_manifest_highlights.tex")
    require_text_contains(main_tex, main_text, "showcase_manifest_claims.tex")

    text = main_text
    if abstract_tex.is_file():
        text += "\n" + abstract_tex.read_text(encoding="utf-8")
    if evaluation_tex.is_file():
        text += "\n" + evaluation_tex.read_text(encoding="utf-8")

    object_claim = linear_claims["object_sweep"]
    constraint_claim = linear_claims["constraint_sweep"]
    object_x = object_claim["x_values"]
    object_latency = object_claim["avg_medians"]
    constraint_x = constraint_claim["x_values"]
    constraint_latency = constraint_claim["avg_medians"]

    require_text_contains_any(
        main_tex,
        text,
        count_phrase(row_count, "run")
        + [r"\rqTwoRunCount-run", r"\rqTwoRunCount{}-run", r"\rqTwoRunCount runs", r"\rqTwoRunCount{} runs"],
        "RQ2 run count",
    )
    require_text_contains_any(
        main_tex,
        text,
        count_phrase(family_count, "family", "families")
        + [
            r"\rqTwoFamilyCount-family",
            r"\rqTwoFamilyCount{}-family",
            r"\rqTwoFamilyCount families",
            r"\rqTwoFamilyCount{} families",
            r"\rqTwoFamilyCount workload families",
            r"\rqTwoFamilyCount{} workload families",
        ],
        "RQ2 family count",
    )
    require_text_contains_any(
        main_tex,
        text,
        count_phrase(matrix_claim["points"], "cell") + [r"\rqTwoMatrixPointCount-cell", r"\rqTwoMatrixPointCount{}-cell"],
        "RQ2 matrix cell count",
    )
    require_text_contains_any(
        main_tex,
        text,
        [
            r"monotone rise in latency from \rqTwoObjectLatencyStart\,ms at \rqTwoObjectXStart monitored object to \rqTwoObjectLatencyEnd\,ms at \rqTwoObjectXEnd objects",
            r"monotone rise in latency from",
        ],
        "RQ2 object latency headline",
    )
    require_text_contains_any(
        main_tex,
        text,
        [
            r"number of monitored objects increases",
            r"monitored objects increases",
        ],
        "RQ2 object axis headline",
    )
    require_text_contains_any(
        main_tex,
        text,
        [
            r"median latency rises from \rqTwoConstraintLatencyStart\,ms at \rqTwoConstraintXStart rule to \rqTwoConstraintLatencyEnd\,ms at \rqTwoConstraintXEnd rules",
            r"latency rises from",
        ],
        "RQ2 constraint latency headline",
    )
    require_text_contains_any(
        main_tex,
        text,
        [
            r"number of event-driven constraints increases",
            r"event-driven constraints increases",
        ],
        "RQ2 constraint axis headline",
    )


def build_verification_summary(
    manifest: Path,
    base_name: str,
    row_count: int,
    family_points: dict[str, set[int]],
    family_repetition_counts: dict[str, Counter[int]],
) -> dict[str, object]:
    out_dir = manifest.parent
    linear_families = {}
    for family, (suffix, x_field, expected_points, expected_repetitions) in LINEAR_FAMILIES.items():
        points = sorted(family_points[family])
        repetition_counts = [family_repetition_counts[family][point] for point in points]
        linear_families[family] = {
            "plot_file": f"{base_name}_{suffix}_plot.tsv",
            "control_axis": x_field,
            "control_points": len(points),
            "expected_control_points": expected_points,
            "control_values": points,
            "repetitions_per_point": sorted(set(repetition_counts)),
            "expected_repetitions_per_point": expected_repetitions,
            "runs": sum(repetition_counts),
        }

    files = [f"{base_name}.tsv"]
    for family, (suffix, *_rest) in LINEAR_FAMILIES.items():
        files.append(f"{base_name}_{suffix}_plot.tsv")
    files.extend(
        [
            f"{base_name}_{MATRIX_SUFFIX}_plot.tsv",
            f"{base_name}_family_summary.tsv",
            f"{base_name}_family_summary.md",
            f"{base_name}_family_summary.tex",
            f"{base_name}_highlights.tex",
            f"{base_name}_claims.tex",
        ]
    )

    return {
        "manifest_file": manifest.name,
        "manifest_sha256": hashlib.sha256(manifest.read_bytes()).hexdigest(),
        "verified_runs": row_count,
        "expected_runs": EXPECTED_RUNS,
        "family_count": len(LINEAR_FAMILIES) + 1,
        "linear_families": linear_families,
        "matrix_family": {
            "name": MATRIX_FAMILY,
            "plot_file": f"{base_name}_{MATRIX_SUFFIX}_plot.tsv",
            "control_points": MATRIX_EXPECTED_POINTS,
            "repetitions_per_point": MATRIX_EXPECTED_REPETITIONS,
            "runs": MATRIX_EXPECTED_POINTS * MATRIX_EXPECTED_REPETITIONS,
        },
        "claim_surface_checks": {
            "generated_tex_macros": [
                f"{base_name}_highlights.tex",
                f"{base_name}_claims.tex",
            ],
            "manuscript_headlines": [
                "paper/main.tex",
                "paper/sections/abstract.tex",
            ],
        },
        "paper_facing_files": [
            file_name
            for file_name in files
            if (out_dir / file_name).is_file()
        ],
    }


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Validate paper-facing RQ2 manifest and derived plot inputs."
    )
    parser.add_argument("manifest", help="Path to the RQ2 manifest TSV.")
    parser.add_argument(
        "--write-summary",
        type=Path,
        help="Optional JSON file recording the validated manifest fingerprint and shape.",
    )
    args = parser.parse_args()

    manifest = Path(args.manifest).resolve()
    if not manifest.is_file():
        fail(f"manifest not found: {manifest}")

    (
        base_name,
        family_points,
        family_repetition_counts,
        row_count,
        processed_total,
        duration_total,
    ) = validate_manifest(manifest)
    out_dir = manifest.parent

    for family, (suffix, _x_field, expected_points, _expected_repetitions) in LINEAR_FAMILIES.items():
        validate_plot(
            out_dir / f"{base_name}_{suffix}_plot.tsv",
            LINEAR_PLOT_COLUMNS,
            expected_points,
        )
        if len(family_points[family]) != expected_points:
            fail(f"{family} point count changed during plot validation")

    validate_plot(
        out_dir / f"{base_name}_{MATRIX_SUFFIX}_plot.tsv",
        MATRIX_PLOT_COLUMNS,
        MATRIX_EXPECTED_POINTS,
    )
    validate_summary_files(out_dir, base_name, family_points, family_repetition_counts)
    linear_claims, matrix_claim = collect_claim_inputs(manifest)
    validate_tex_macro_files(
        manifest,
        out_dir,
        base_name,
        row_count,
        processed_total,
        duration_total,
        family_repetition_counts,
    )
    validate_manuscript_rq2_headlines(
        manifest,
        row_count,
        len(LINEAR_FAMILIES) + 1,
        linear_claims,
        matrix_claim,
    )

    if args.write_summary is not None:
        summary = build_verification_summary(
            manifest,
            base_name,
            row_count,
            family_points,
            family_repetition_counts,
        )
        args.write_summary.parent.mkdir(parents=True, exist_ok=True)
        args.write_summary.write_text(
            json.dumps(summary, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        print(f"wrote verification summary {args.write_summary}")

    print(
        f"verified {manifest} with {row_count} runs, "
        f"{len(LINEAR_FAMILIES) + 1} families, paper-facing plot/summary files, "
        "TeX claim macros, and manuscript headlines"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
