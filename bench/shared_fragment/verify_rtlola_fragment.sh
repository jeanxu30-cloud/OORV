#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../../.." && pwd)"
tool_dir="${repo_root}/tool"
rtlola_bin="${RTLOLA_CLI:-${tool_dir}/bench/third_party/rtlola/RTLola-Interpreter-main/crates/target/release/rtlola-cli}"

python3 - "${repo_root}" "${tool_dir}" "${rtlola_bin}" <<'PY'
import re
import subprocess
import sys
from pathlib import Path

repo_root = Path(sys.argv[1])
tool_dir = Path(sys.argv[2])
rtlola_bin = Path(sys.argv[3])
oorv_bin = tool_dir / "target" / "release" / "oorv"

message = "Two distinct cars are too close."


def fail(msg: str) -> None:
    raise SystemExit(msg)


def run(label: str, cmd: list[str], cwd: Path, timeout: int = 30) -> subprocess.CompletedProcess[str]:
    try:
        proc = subprocess.run(
            cmd,
            cwd=cwd,
            text=True,
            capture_output=True,
            timeout=timeout,
        )
    except PermissionError as exc:
        fail(
            f"{label} failed with a permission error: {exc}\n"
            f"Hint: chmod +x {rtlola_bin}"
        )
    except subprocess.TimeoutExpired as exc:
        stdout = exc.stdout or ""
        stderr = exc.stderr or ""
        fail(
            f"{label} timed out after {timeout}s.\n"
            f"stdout:\n{stdout}\n"
            f"stderr:\n{stderr}\n"
            "On macOS, also check whether the binary still has "
            "`com.apple.quarantine` and remove it with xattr -d if needed."
        )

    if proc.returncode != 0:
        fail(
            f"{label} failed with exit code {proc.returncode}\n"
            f"command: {' '.join(cmd)}\n"
            f"stdout:\n{proc.stdout}\n"
            f"stderr:\n{proc.stderr}"
        )
    return proc


def parse_oorv_triggers(text: str) -> list[str]:
    pattern = re.compile(r"@([0-9]+\.[0-9]+) \| constraint \| #\d+ :: value = \"([^\"]+)\"")
    return [time for time, msg in pattern.findall(text) if msg == message]


def parse_rtlola_triggers(text: str) -> list[str]:
    pattern = re.compile(r"\[([0-9]+\.[0-9]+)\]\[Trigger\]\[#\d+\]\[Value\] = \"([^\"]+)\"")
    return [time for time, msg in pattern.findall(text) if msg == message]


if not oorv_bin.is_file():
    fail(f"missing OORV binary: {oorv_bin}\nRun `cargo build --release --offline --locked` in tool/.")
if not rtlola_bin.is_file():
    fail(
        f"missing RTLola binary: {rtlola_bin}\n"
        "Run the source-build command documented in baselines/rtlola/README.md."
    )

run(
    "RTLola help",
    [str(rtlola_bin), "--help"],
    repo_root,
)
run(
    "RTLola analyze",
    [
        str(rtlola_bin),
        "analyze",
        "tool/bench/shared_fragment/baselines/rtlola/pairwise_distance_two_cars.lola",
    ],
    repo_root,
)

oorv_proc = run(
    "OORV shared-fragment monitor",
    [
        str(oorv_bin),
        "bench/shared_fragment/pairwise_distance.oorv",
        "--offline",
        "relative",
        "--csv-in",
        "bench/shared_fragment/pairwise_distance.csv",
        "--verbosity",
        "warnings",
    ],
    tool_dir,
)

rtlola_proc = run(
    "RTLola shared-fragment monitor",
    [
        str(rtlola_bin),
        "monitor",
        "tool/bench/shared_fragment/baselines/rtlola/pairwise_distance_two_cars.lola",
        "--offline",
        "relative",
        "--csv-in",
        "tool/bench/shared_fragment/baselines/rtlola/pairwise_distance_two_cars.csv",
        "--verbosity",
        "warnings",
    ],
    repo_root,
)

oorv_triggers = parse_oorv_triggers(oorv_proc.stdout)
rtlola_triggers = parse_rtlola_triggers(rtlola_proc.stdout)

if oorv_triggers != rtlola_triggers:
    fail(
        "shared-fragment trigger times differ\n"
        f"OORV:   {oorv_triggers}\n"
        f"RTLola: {rtlola_triggers}\n"
        f"OORV stdout:\n{oorv_proc.stdout}\n"
        f"RTLola stdout:\n{rtlola_proc.stdout}"
    )

print("shared_fragment_trigger_times=" + ",".join(oorv_triggers))
print("rtlola_cli=" + str(rtlola_bin))
print("status=ok")
PY
