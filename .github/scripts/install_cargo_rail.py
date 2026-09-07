#!/usr/bin/env python3
"""Install the pinned native Cargo-Rail binary from its verified release archive."""

from __future__ import annotations

import hashlib
import os
from pathlib import Path
import platform
import shutil
import sys
import tarfile
import tempfile
import urllib.request
import zipfile


VERSION = "0.25.0"
RELEASE = f"https://github.com/loadingalias/cargo-rail/releases/download/v{VERSION}"
ARCHIVES = {
    ("Darwin", "arm64"): (
        "cargo-rail-aarch64-apple-darwin.tar.gz",
        "7ba3508ec8c03565bf55a98b3b7b33eae006425c2a7083e4e9c7fd04f69c5dfe",
    ),
    ("Linux", "x86_64"): (
        "cargo-rail-x86_64-unknown-linux-gnu.tar.gz",
        "8199c6736031c0f2d8807f7af50b3a8108b5bd7e5ce08b129b80865477ddbf69",
    ),
    ("Windows", "AMD64"): (
        "cargo-rail-x86_64-pc-windows-msvc.zip",
        "9e798ae2a625cc97cf64aecdb15ccc7a30e8d943315ffe15fc453d0a89077e8c",
    ),
}


def main() -> int:
    target = ARCHIVES.get((platform.system(), platform.machine()))
    if target is None:
        print(f"unsupported Cargo-Rail host: {platform.system()} {platform.machine()}", file=sys.stderr)
        return 1
    archive_name, expected = target
    install_root = Path(os.environ["RUNNER_TEMP"]) / f"cargo-rail-{VERSION}"
    install_root.mkdir(mode=0o700, parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=install_root) as temporary:
        archive = Path(temporary) / archive_name
        with urllib.request.urlopen(f"{RELEASE}/{archive_name}") as response, archive.open("wb") as output:
            shutil.copyfileobj(response, output)
        actual = hashlib.sha256(archive.read_bytes()).hexdigest()
        if actual != expected:
            print(f"Cargo-Rail checksum mismatch: expected {expected}, got {actual}", file=sys.stderr)
            return 1
        if archive_name.endswith(".zip"):
            with zipfile.ZipFile(archive) as bundle:
                with bundle.open("cargo-rail.exe") as source, (install_root / "cargo-rail.exe").open("wb") as output:
                    shutil.copyfileobj(source, output)
        else:
            with tarfile.open(archive, "r:gz") as bundle:
                member = bundle.getmember("cargo-rail")
                source = bundle.extractfile(member)
                if source is None:
                    raise RuntimeError("Cargo-Rail archive contains no executable bytes")
                with source, (install_root / "cargo-rail").open("wb") as output:
                    shutil.copyfileobj(source, output)

    executable = install_root / ("cargo-rail.exe" if platform.system() == "Windows" else "cargo-rail")
    executable.chmod(0o755)
    with Path(os.environ["GITHUB_PATH"]).open("a", encoding="utf-8") as github_path:
        github_path.write(f"{install_root}\n")
    print(f"Installed {executable}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
