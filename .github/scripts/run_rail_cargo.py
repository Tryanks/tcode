#!/usr/bin/env python3
"""Run one Cargo command with scope lowered by Cargo-Rail's strict reader."""

from __future__ import annotations

import argparse
from pathlib import Path
import subprocess
import sys


def reader(reader: Path, *arguments: str) -> bytes:
    return subprocess.run(
        [sys.executable, str(reader), *arguments], check=True, stdout=subprocess.PIPE
    ).stdout


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--plan", type=Path, required=True)
    parser.add_argument("--reader", type=Path, required=True)
    parser.add_argument("--work", required=True)
    parser.add_argument("--host-full", action="store_true")
    parser.add_argument("--xvfb", action="store_true")
    parser.add_argument("cargo_command")
    parser.add_argument("cargo_arguments", nargs=argparse.REMAINDER)
    args = parser.parse_args()

    if args.host_full:
        cargo_scope = ["--workspace"]
    else:
        scope = reader(args.reader, "cargo-scope", str(args.plan), args.work).decode().strip()
        if scope == "workspace":
            cargo_scope = ["--workspace"]
        elif scope == "packages":
            raw = reader(args.reader, "cargo-args", str(args.plan), args.work)
            if not raw.endswith(b"\0"):
                raise RuntimeError("Cargo-Rail package arguments are not NUL terminated")
            cargo_scope = [part.decode() for part in raw[:-1].split(b"\0")]
            if not cargo_scope or any(not argument for argument in cargo_scope):
                raise RuntimeError("Cargo-Rail returned an empty package selection")
        else:
            raise RuntimeError(f"Cargo-Rail returned unusable scope {scope!r} for {args.work}")

    command = ["cargo", args.cargo_command, *cargo_scope, *args.cargo_arguments]
    if args.xvfb:
        command = ["xvfb-run", "-a", *command]
    return subprocess.run(command, check=False).returncode


if __name__ == "__main__":
    raise SystemExit(main())
