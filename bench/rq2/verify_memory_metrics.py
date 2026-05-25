#!/usr/bin/env python3

from __future__ import annotations

import csv
import sys
from datetime import datetime
from pathlib import Path


REQUIRED_COLUMNS = [
    "label",
    "started",
    "ended",
    "duration_ms",
    "exit_code",
    "peak_private_bytes",
    "poll_ms",
    "samples",
    "stdout",
    "stderr",
    "command",
]

SUITE_MANIFEST_COLUMNS = [
    "suite_label",
    "label",
    "family",
    "objects",
    "constraints",
    "events_requested",
    "workload_dir",
    "metrics_path",
    "helper_exit_code",
]


def fail(message: str) -> None:
    print(f"verify_memory_metrics: {message}", file=sys.stderr)
    raise SystemExit(1)


def parse_iso_timestamp(path: Path, label: str, field: str, value: str) -> datetime:
    if not value:
        fail(f"{path} row {label}: {field} is empty")
    # PowerShell's round-trip format uses seven fractional second digits.
    normalized = value
    if normalized.endswith("Z"):
        normalized = normalized[:-1] + "+00:00"
    if "." in normalized:
        prefix, suffix = normalized.split(".", 1)
        tz = ""
        if "+" in suffix:
            fraction, tz = suffix.split("+", 1)
            tz = "+" + tz
        elif "-" in suffix:
            fraction, tz = suffix.split("-", 1)
            tz = "-" + tz
        else:
            fraction = suffix
        normalized = f"{prefix}.{fraction[:6]}{tz}"
    try:
        return datetime.fromisoformat(normalized)
    except ValueError as exc:
        fail(f"{path} row {label}: {field} is not an ISO timestamp: {value!r}")
        raise AssertionError from exc


def parse_int(path: Path, label: str, field: str, value: str, minimum: int) -> int:
    try:
        parsed = int(value)
    except ValueError as exc:
        fail(f"{path} row {label}: {field} is not an integer: {value!r}")
        raise AssertionError from exc
    if parsed < minimum:
        fail(f"{path} row {label}: {field} must be >= {minimum}, got {parsed}")
    return parsed


def parse_float(path: Path, label: str, field: str, value: str, minimum: float) -> float:
    try:
        parsed = float(value)
    except ValueError as exc:
        fail(f"{path} row {label}: {field} is not a number: {value!r}")
        raise AssertionError from exc
    if parsed < minimum:
        fail(f"{path} row {label}: {field} must be >= {minimum}, got {parsed}")
    return parsed


def read_tsv(path: Path, required_columns: list[str]) -> list[dict[str, str]]:
    if not path.is_file():
        fail(f"TSV file not found: {path}")

    with path.open(newline="", encoding="utf-8-sig") as f:
        reader = csv.DictReader(f, delimiter="\t")
        if reader.fieldnames is None:
            fail(f"missing TSV header: {path}")
        missing = [column for column in required_columns if column not in reader.fieldnames]
        if missing:
            fail(f"{path} missing columns: {', '.join(missing)}")
        rows = list(reader)

    if not rows:
        fail(f"metrics file has no data rows: {path}")
    return rows


def validate_file(path: Path, require_success: bool) -> set[str]:
    rows = read_tsv(path, REQUIRED_COLUMNS)
    labels: set[str] = set()

    for idx, row in enumerate(rows, start=2):
        label = row.get("label") or f"line {idx}"
        if label in labels:
            fail(f"{path} row {label}: duplicate label")
        labels.add(label)
        started = parse_iso_timestamp(path, label, "started", row["started"])
        ended = parse_iso_timestamp(path, label, "ended", row["ended"])
        if ended < started:
            fail(f"{path} row {label}: ended timestamp precedes started timestamp")

        duration_ms = parse_float(path, label, "duration_ms", row["duration_ms"], 0.0)
        exit_code = parse_int(path, label, "exit_code", row["exit_code"], 0)
        peak_private = parse_int(
            path, label, "peak_private_bytes", row["peak_private_bytes"], 1
        )
        poll_ms = parse_int(path, label, "poll_ms", row["poll_ms"], 1)
        samples = parse_int(path, label, "samples", row["samples"], 1)
        if require_success and exit_code != 0:
            fail(f"{path} row {label}: exit_code must be 0, got {exit_code}")

        if not row["stdout"]:
            fail(f"{path} row {label}: stdout path is empty")
        if not row["stderr"]:
            fail(f"{path} row {label}: stderr path is empty")
        if "oorv" not in row["command"]:
            fail(f"{path} row {label}: command does not mention oorv")

        # Keep variables visibly used in validation and in error messages above.
        _ = (duration_ms, peak_private, poll_ms, samples)

    return labels


def validate_suite_manifest(path: Path, expected_labels: set[str], require_success: bool) -> int:
    rows = read_tsv(path, SUITE_MANIFEST_COLUMNS)
    labels: set[str] = set()
    for row in rows:
        label = row["label"]
        if not label:
            fail(f"{path}: suite manifest row has an empty label")
        if label in labels:
            fail(f"{path}: duplicate suite manifest label: {label}")
        labels.add(label)

        parse_int(path, label, "objects", row["objects"], 1)
        parse_int(path, label, "constraints", row["constraints"], 0)
        parse_int(path, label, "events_requested", row["events_requested"], 1)
        helper_exit_code = parse_int(
            path, label, "helper_exit_code", row["helper_exit_code"], 0
        )
        if require_success and helper_exit_code != 0:
            fail(
                f"{path} row {label}: helper_exit_code must be 0, got {helper_exit_code}"
            )
        if not row["suite_label"]:
            fail(f"{path} row {label}: suite_label is empty")
        if not row["family"]:
            fail(f"{path} row {label}: family is empty")
        if not row["workload_dir"]:
            fail(f"{path} row {label}: workload_dir is empty")
        if not row["metrics_path"]:
            fail(f"{path} row {label}: metrics_path is empty")

    missing_from_suite = sorted(expected_labels - labels)
    extra_in_suite = sorted(labels - expected_labels)
    if missing_from_suite or extra_in_suite:
        fail(
            f"{path}: suite manifest labels do not match memory metrics labels; "
            f"missing_from_suite={missing_from_suite}, extra_in_suite={extra_in_suite}"
        )
    return len(rows)


def main() -> int:
    require_success = True
    suite_manifest: Path | None = None
    args = sys.argv[1:]
    if "--allow-nonzero-exit" in args:
        require_success = False
        args.remove("--allow-nonzero-exit")
    if "--suite-manifest" in args:
        ix = args.index("--suite-manifest")
        try:
            suite_manifest = Path(args[ix + 1]).resolve()
        except IndexError:
            print("missing value after --suite-manifest", file=sys.stderr)
            return 1
        del args[ix : ix + 2]
    if len(args) != 1:
        print(
            "usage: verify_memory_metrics.py [--allow-nonzero-exit] "
            "[--suite-manifest <suite_manifest.tsv>] <memory_metrics.tsv>",
            file=sys.stderr,
        )
        return 1

    path = Path(args[0]).resolve()
    labels = validate_file(path, require_success)
    suite_msg = ""
    if suite_manifest is not None:
        suite_row_count = validate_suite_manifest(suite_manifest, labels, require_success)
        suite_msg = f" and suite manifest {suite_manifest} with {suite_row_count} row(s)"
    row_count = len(labels)
    print(f"verified {path} with {row_count} Windows peak-private-memory row(s)")
    if suite_msg:
        print(f"verified {path}{suite_msg}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
