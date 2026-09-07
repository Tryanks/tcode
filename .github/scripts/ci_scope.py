#!/usr/bin/env python3
"""Add tcode's small repository policy layer to a Cargo-Rail CI plan."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path, PurePosixPath
import subprocess
import sys


GLOBAL_BUILD_PATHS = {"Cargo.toml", "Cargo.lock"}
GLOBAL_BUILD_PREFIXES = (
    ".cargo/", ".github/actions/", ".github/scripts/", ".github/workflows/"
)
DOC_SUFFIXES = {".md", ".markdown", ".rst", ".png", ".jpg", ".jpeg", ".gif", ".svg"}
EMBEDDED_PROMPTS = {
    "assets/orchestrate/astra.md",
    "assets/orchestrate/collaboration.md",
    "assets/orchestrate/fable-5-1.md",
    "assets/orchestrate/workflow.md",
}
IGNORED_RESOURCES = {
    "assets/fonts/OFL.txt",
    "assets/fonts/lilex/OFL.txt",
    "assets/icons/app/tcode.icns",
    "assets/icons/app/tcode.png",
    "assets/macos/tcode.entitlements",
    "crates/web/assets/NotoSans-LICENSE",
}
PLATFORMS = {
    "ios": ("tcode-ios", "aarch64-apple-ios-sim"),
    "android": ("tcode-android", "aarch64-linux-android"),
    "web": ("tcode-web", "wasm32-unknown-unknown"),
}


class ScopeError(RuntimeError):
    pass


def run(command: list[str], root: Path) -> bytes:
    try:
        return subprocess.run(
            command, cwd=root, check=True, stdout=subprocess.PIPE
        ).stdout
    except (OSError, subprocess.CalledProcessError) as error:
        raise ScopeError(f"command failed: {' '.join(command)}") from error


def write_outputs(path: Path, values: dict[str, object]) -> None:
    with path.open("a", encoding="utf-8") as output:
        for key, value in values.items():
            if isinstance(value, bool):
                value = str(value).lower()
            output.write(f"{key}={value}\n")


def event_range(root: Path, event_path: Path) -> tuple[str, str, bool]:
    try:
        event = json.loads(event_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ScopeError(f"cannot read GitHub event {event_path}") from error

    event_name = os.environ.get("GITHUB_EVENT_NAME")
    if event_name == "pull_request":
        try:
            base = event["pull_request"]["base"]["sha"]
            head = event["pull_request"]["head"]["sha"]
        except (KeyError, TypeError) as error:
            raise ScopeError("pull request event has no base/head SHA") from error
        try:
            merge_base = run(["git", "merge-base", base, head], root).decode().strip()
        except ScopeError:
            return "", head, True
        return merge_base, head, False

    if event_name == "push":
        before = event.get("before")
        head = event.get("after") or os.environ.get("GITHUB_SHA")
        if not isinstance(before, str) or not isinstance(head, str):
            return "", str(head or "HEAD"), True
        if not before or set(before) == {"0"}:
            return "", head, True
        try:
            run(["git", "cat-file", "-e", f"{before}^{{commit}}"], root)
        except ScopeError:
            return "", head, True
        return before, head, False

    raise ScopeError(f"unsupported GitHub event {event_name!r}")


def changed_paths(root: Path, base: str, head: str) -> list[tuple[str, str]]:
    data = run(
        ["git", "diff", "--name-status", "--no-renames", "-z", base, head, "--"], root
    )
    fields = data.split(b"\0")
    if fields and not fields[-1]:
        fields.pop()
    if len(fields) % 2:
        raise ScopeError("malformed NUL-delimited Git diff")
    return [
        (os.fsdecode(fields[index])[:1], os.fsdecode(fields[index + 1]).replace(os.sep, "/"))
        for index in range(0, len(fields), 2)
    ]


def is_documentation(path: str) -> bool:
    pure = PurePosixPath(path)
    if path in {"LICENSE", "CODE_OF_CONDUCT.md"} or path in IGNORED_RESOURCES:
        return True
    if len(pure.parts) == 1 and pure.suffix.lower() in {".md", ".markdown", ".rst"}:
        return True
    if path.startswith("docs/") and pure.suffix.lower() in DOC_SUFFIXES:
        return True
    if path.startswith(".github/") and pure.suffix.lower() == ".md":
        return True
    return pure.name.lower() == "readme.md" and path.startswith("crates/")


def is_global_build_input(path: str) -> bool:
    name = PurePosixPath(path).name
    return (
        path in GLOBAL_BUILD_PATHS
        or path.startswith(GLOBAL_BUILD_PREFIXES)
        or path.endswith("/Cargo.toml")
        or name == "build.rs"
        or name.startswith("rust-toolchain")
        or name in {"clippy.toml", "rustfmt.toml", "Cross.toml", "Makefile", "Justfile"}
    )


def classify(changes: list[tuple[str, str]]) -> dict[str, object]:
    relevant: list[str] = []
    full = False
    dependencies = False
    for status, path in changes:
        if path in EMBEDDED_PROMPTS:
            if status == "M":
                continue
            full = True
            continue
        if is_documentation(path):
            continue
        relevant.append(path)
        if is_global_build_input(path):
            full = True
            dependencies = True
        elif path.endswith(".rs") and path.startswith("crates/"):
            dependencies = True
        else:
            # Non-document inputs may be generated, embedded, or executable.
            # Rail still supplies the host plan, but unknown ownership widens it.
            full = True
            dependencies = dependencies or PurePosixPath(path).suffix.lower() in {
                ".rs", ".toml", ".py", ".sh", ".yml", ".yaml"
            }
    return {
        "skip": not relevant and not full,
        "full": full,
        "dependencies": dependencies,
        "relevant": relevant,
    }


def metadata_model(root: Path) -> tuple[dict[str, Path], set[str]]:
    raw = run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps", "--locked", "--offline"],
        root,
    )
    try:
        metadata = json.loads(raw)
        packages = metadata["packages"]
    except (json.JSONDecodeError, KeyError, TypeError) as error:
        raise ScopeError("cargo metadata returned an invalid package model") from error

    directories: dict[str, Path] = {}
    for package in packages:
        try:
            directories[package["name"]] = Path(package["manifest_path"]).resolve().parent
        except (KeyError, TypeError, AttributeError) as error:
            raise ScopeError("cargo metadata returned an incomplete package") from error
    for package in packages:
        for dependency in package.get("dependencies", []):
            dependency_path = dependency.get("path")
            dependency_name = dependency.get("name")
            if isinstance(dependency_path, str) and isinstance(dependency_name, str):
                directories.setdefault(dependency_name, Path(dependency_path).resolve())
    return directories, set(directories)


def owner(root: Path, path: str, directories: dict[str, Path]) -> str | None:
    candidate = (root / path).resolve()
    matches = []
    for package, directory in directories.items():
        try:
            candidate.relative_to(directory)
        except ValueError:
            continue
        matches.append((len(directory.parts), package))
    return max(matches)[1] if matches else None


def is_dev_only(root: Path, path: str, package_directory: Path) -> bool:
    relative = (root / path).resolve().relative_to(package_directory)
    return bool(relative.parts) and relative.parts[0] in {"tests", "examples", "benches"}


def active_packages(root: Path, package_names: set[str], package: str, target: str) -> set[str]:
    raw = run(
        [
            "cargo", "tree", "--locked", "-p", package, "--target", target,
            "--edges", "normal,build", "--prefix", "none", "--format", "{p}",
        ],
        root,
    )
    active = {
        fields[0]
        for line in raw.decode("utf-8", errors="surrogateescape").splitlines()
        if (fields := line.split()) and fields[0] in package_names
    }
    if package not in active:
        raise ScopeError(f"cargo tree omitted root package {package}")
    return active


def finalize(
    root: Path,
    changes: list[tuple[str, str]],
    classification: dict[str, object],
    required_work_json: str,
) -> dict[str, object]:
    if classification["skip"]:
        return {
            "desktop": False, "build": False, "clippy": False, "test": False,
            "host_full": False, "dependencies": False,
            "ios": False, "android": False, "web": False,
        }
    try:
        required_projection = json.loads(required_work_json)
    except (json.JSONDecodeError, TypeError) as error:
        raise ScopeError("Cargo-Rail required-work output is invalid") from error
    if not isinstance(required_projection, list) or not all(
        isinstance(item, str) for item in required_projection
    ):
        raise ScopeError("Cargo-Rail required-work output contains a non-string")
    required_work = set(required_projection)

    build = "cargo.build" in required_work
    clippy = "cargo.clippy" in required_work
    test = "cargo.test" in required_work
    result = {
        "desktop": build or clippy or test,
        "host_full": False,
        "build": build,
        "clippy": clippy,
        "test": test,
        "dependencies": classification["dependencies"],
    }
    if not result["desktop"]:
        return {**result, "desktop": True, "host_full": True,
                "build": True, "clippy": True, "test": True,
                "ios": True, "android": True, "web": True}
    if classification["full"]:
        return {**result, "desktop": True, "host_full": True,
                "build": True, "clippy": True, "test": True,
                "ios": True, "android": True, "web": True}

    directories, package_names = metadata_model(root)
    seeds = set()
    for _, path in changes:
        if path not in classification["relevant"]:
            continue
        package = owner(root, path, directories)
        if package is None:
            return {**result, "desktop": True, "host_full": True,
                    "build": True, "clippy": True, "test": True,
                    "dependencies": True, "ios": True, "android": True, "web": True}
        if is_dev_only(root, path, directories[package]):
            continue
        seeds.add(package)

    for platform, (package, target) in PLATFORMS.items():
        result[platform] = bool(seeds & active_packages(root, package_names, package, target))
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    classify_parser = subparsers.add_parser("classify")
    classify_parser.add_argument("--root", type=Path, default=Path.cwd())
    classify_parser.add_argument("--event", type=Path)
    classify_parser.add_argument("--from-ref")
    classify_parser.add_argument("--to-ref", default="HEAD")
    classify_parser.add_argument("--github-output", type=Path, required=True)

    finalize_parser = subparsers.add_parser("finalize")
    finalize_parser.add_argument("--root", type=Path, default=Path.cwd())
    finalize_parser.add_argument("--from-ref", required=True)
    finalize_parser.add_argument("--to-ref", required=True)
    finalize_parser.add_argument("--skip", choices=("true", "false"), required=True)
    finalize_parser.add_argument("--full", choices=("true", "false"), required=True)
    finalize_parser.add_argument("--dependencies", choices=("true", "false"), required=True)
    finalize_parser.add_argument("--required-work", default="[]")
    finalize_parser.add_argument("--github-output", type=Path, required=True)
    args = parser.parse_args()
    root = args.root.resolve()

    try:
        if args.command == "classify":
            if args.from_ref:
                base, head, fallback = args.from_ref, args.to_ref, False
            else:
                if args.event is None:
                    raise ScopeError("classify requires --event or --from-ref")
                base, head, fallback = event_range(root, args.event)
            if fallback:
                values = {"base": "", "head": head, "skip": False, "full": True,
                          "dependencies": True, "changed": 0}
            else:
                changes = changed_paths(root, base, head)
                values = {"base": base, "head": head, **classify(changes), "changed": len(changes)}
                values.pop("relevant")
            write_outputs(args.github_output, values)
            print(json.dumps(values, indent=2, sort_keys=True))
        else:
            changes = [] if args.full == "true" and not args.from_ref else changed_paths(
                root, args.from_ref, args.to_ref
            )
            classification = classify(changes)
            classification["skip"] = args.skip == "true"
            classification["full"] = args.full == "true"
            classification["dependencies"] = args.dependencies == "true"
            values = finalize(root, changes, classification, args.required_work)
            write_outputs(args.github_output, values)
            print(json.dumps(values, indent=2, sort_keys=True))
    except ScopeError as error:
        print(f"CI scope planning failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
