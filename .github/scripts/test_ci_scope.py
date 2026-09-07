#!/usr/bin/env python3
"""Behavioral tests for tcode's policy around the real Cargo-Rail planner."""

from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("ci_scope.py")
EXECUTOR = Path(__file__).with_name("run_rail_cargo.py")


def rail_binary() -> str | None:
    configured = os.environ.get("CARGO_RAIL_BIN")
    if configured:
        return configured
    found = shutil.which("cargo-rail")
    if found:
        return found
    probe = Path("/tmp/tcode-rail-probe/cargo-rail")
    return str(probe) if probe.is_file() else None


class Workspace:
    def __init__(self, rail: str) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.rail = rail
        self.git("init", "-q")
        self.git("config", "user.name", "CI scope test")
        self.git("config", "user.email", "ci@example.test")
        self.write(
            "Cargo.toml",
            """[workspace]
members = [
  "crates/core", "crates/runtime", "crates/ui", "crates/app", "crates/unrelated",
  "crates/mobile", "crates/ios", "crates/android", "crates/web",
]
exclude = ["crates/platform/gpui-ios", "crates/platform/gpui-android"]
resolver = "3"
""",
        )
        self.package("core")
        self.package("runtime", '[dependencies]\ntcode-core = { path = "../core" }\n')
        self.package(
            "ui",
            '[features]\ndefault = ["desktop"]\ndesktop = ["dep:tcode-runtime"]\n'
            '[dependencies]\ntcode-core = { path = "../core" }\n'
            'tcode-runtime = { path = "../runtime", optional = true }\n',
        )
        self.package("app", '[dependencies]\ntcode-core = { path = "../core" }\n')
        self.package("unrelated")
        self.package(
            "mobile",
            '[dependencies]\ntcode-core = { path = "../core" }\n'
            'tcode-ui = { path = "../ui", default-features = false }\n',
        )
        self.package(
            "ios",
            '[dependencies]\ntcode-mobile = { path = "../mobile" }\n'
            'tcode-ui = { path = "../ui", default-features = false }\n'
            'gpui-ios = { path = "../platform/gpui-ios" }\n',
        )
        self.package(
            "android",
            '[dependencies]\ntcode-mobile = { path = "../mobile" }\n'
            'tcode-ui = { path = "../ui", default-features = false }\n'
            'gpui-android = { path = "../platform/gpui-android" }\n',
        )
        self.package(
            "web",
            '[target.\'cfg(target_family = "wasm")\'.dependencies]\n'
            'tcode-mobile = { path = "../mobile" }\n'
            'tcode-ui = { path = "../ui", default-features = false }\n',
        )
        self.package("gpui-ios", directory="crates/platform/gpui-ios")
        self.package("gpui-android", directory="crates/platform/gpui-android")
        self.write("README.md", "baseline\n")
        self.write("assets/orchestrate/workflow.md", "prompt\n")
        subprocess.run(["cargo", "generate-lockfile", "--offline"], cwd=self.root, check=True)
        self.baseline = self.commit("baseline")

    def close(self) -> None:
        self.temporary.cleanup()

    def git(self, *args: str) -> str:
        return subprocess.run(
            ["git", *args], cwd=self.root, check=True, text=True, stdout=subprocess.PIPE
        ).stdout.strip()

    def write(self, relative: str, value: str) -> None:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(value, encoding="utf-8")

    def package(self, short: str, extra: str = "", *, directory: str | None = None) -> None:
        package_name = "tcode" if short == "app" else (
            short if short.startswith("gpui-") else f"tcode-{short}"
        )
        package_dir = directory or f"crates/{short}"
        self.write(
            f"{package_dir}/Cargo.toml",
            f'[package]\nname = "{package_name}"\nversion = "0.1.0"\nedition = "2024"\n{extra}',
        )
        self.write(f"{package_dir}/src/lib.rs", f"pub fn {short.replace('-', '_')}() {{}}\n")

    def commit(self, message: str) -> str:
        self.git("add", "-A")
        self.git("commit", "-qm", message)
        return self.git("rev-parse", "HEAD")

    def scenario(self, name: str, mutate) -> str:
        self.git("checkout", "-q", "-B", name, self.baseline)
        mutate()
        return self.commit(name)

    def classify(self, head: str) -> tuple[subprocess.CompletedProcess[str], dict[str, str]]:
        output = self.root / ".git/classify.out"
        output.unlink(missing_ok=True)
        result = subprocess.run(
            ["python3", str(SCRIPT), "classify", "--root", str(self.root),
             "--from-ref", self.baseline, "--to-ref", head, "--github-output", str(output)],
            text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        values = {}
        if output.exists():
            values = dict(line.split("=", 1) for line in output.read_text().splitlines())
        return result, values

    def classify_event(self, event_name: str, event: dict[str, object]):
        event_path = self.root / ".git/event.json"
        event_path.write_text(json.dumps(event), encoding="utf-8")
        output = self.root / ".git/event.out"
        output.unlink(missing_ok=True)
        result = subprocess.run(
            ["python3", str(SCRIPT), "classify", "--root", str(self.root),
             "--event", str(event_path), "--github-output", str(output)],
            env={**os.environ, "GITHUB_EVENT_NAME": event_name},
            text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        values = {}
        if output.exists():
            values = dict(line.split("=", 1) for line in output.read_text().splitlines())
        return result, values

    def rail_plan(self, head: str) -> dict[str, object]:
        result = subprocess.run(
            [self.rail, "--workspace-root", str(self.root), "--json", "plan",
             "--from", self.baseline, "--to", head],
            check=True, text=True, stdout=subprocess.PIPE,
        )
        return json.loads(result.stdout)

    def finalize(self, head: str, classification: dict[str, str], plan: dict[str, object] | None):
        output = self.root / ".git/final.out"
        output.unlink(missing_ok=True)
        required = json.dumps(plan["required"], separators=(",", ":")) if plan else "[]"
        result = subprocess.run(
            ["python3", str(SCRIPT), "finalize", "--root", str(self.root),
             "--from-ref", self.baseline, "--to-ref", head,
             "--skip", classification["skip"], "--full", classification["full"],
             "--dependencies", classification["dependencies"],
             "--required-work", required, "--github-output", str(output)],
            text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        values = {}
        if output.exists():
            values = dict(line.split("=", 1) for line in output.read_text().splitlines())
        return result, values


class ScopeTests(unittest.TestCase):
    def setUp(self) -> None:
        rail = rail_binary()
        if rail is None:
            if os.environ.get("CI"):
                self.fail("CI requires the pinned Cargo-Rail binary")
            self.skipTest("set CARGO_RAIL_BIN to run the Cargo-Rail integration tests")
        version = subprocess.run([rail, "--version"], check=True, text=True, stdout=subprocess.PIPE).stdout
        self.assertIn("0.25.0", version)
        self.workspace = Workspace(rail)

    def tearDown(self) -> None:
        self.workspace.close()

    def classified(self, head: str) -> dict[str, str]:
        result, values = self.workspace.classify(head)
        self.assertEqual(result.returncode, 0, result.stderr)
        return values

    def test_documentation_and_modified_prompt_skip_before_rail(self) -> None:
        head = self.workspace.scenario(
            "docs", lambda: (
                self.workspace.write("README.md", "docs\n"),
                self.workspace.write("assets/orchestrate/workflow.md", "wording\n"),
            )
        )
        classification = self.classified(head)
        self.assertEqual(classification["skip"], "true")
        result, final = self.workspace.finalize(head, classification, None)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(final["desktop"], "false")
        self.assertEqual(final["ios"], "false")

    def test_pull_request_uses_merge_base_and_zero_push_falls_back_full(self) -> None:
        head = self.workspace.scenario(
            "event-docs", lambda: self.workspace.write("README.md", "docs\n")
        )
        result, pull = self.workspace.classify_event(
            "pull_request",
            {"pull_request": {"base": {"sha": self.workspace.baseline}, "head": {"sha": head}}},
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(pull["base"], self.workspace.baseline)
        self.assertEqual(pull["skip"], "true")

        result, push = self.workspace.classify_event(
            "push", {"before": "0" * 40, "after": head}
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(push["full"], "true")

    def test_rail_scopes_leaf_and_reverse_dependent_source_changes(self) -> None:
        leaf = self.workspace.scenario(
            "leaf", lambda: self.workspace.write("crates/app/src/lib.rs", "pub fn changed() {}\n")
        )
        leaf_plan = self.workspace.rail_plan(leaf)
        self.assertEqual(
            leaf_plan["work"]["cargo.test"]["scope"]["selection"]["cargo_args"],
            ["-p", "tcode"],
        )
        core = self.workspace.scenario(
            "core", lambda: self.workspace.write("crates/core/src/lib.rs", "pub fn changed() {}\n")
        )
        core_plan = self.workspace.rail_plan(core)
        args = core_plan["work"]["cargo.test"]["scope"]["selection"]["cargo_args"]
        self.assertIn("tcode", args)
        self.assertIn("tcode-core", args)
        self.assertNotIn("tcode-unrelated", args)

    def test_platform_scope_uses_modified_owner_not_host_reverse_closure(self) -> None:
        runtime = self.workspace.scenario(
            "runtime", lambda: self.workspace.write("crates/runtime/src/lib.rs", "pub fn changed() {}\n")
        )
        classification = self.classified(runtime)
        result, final = self.workspace.finalize(runtime, classification, self.workspace.rail_plan(runtime))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((final["ios"], final["android"], final["web"]), ("false", "false", "false"))

        ui = self.workspace.scenario(
            "ui", lambda: self.workspace.write("crates/ui/src/lib.rs", "pub fn changed() {}\n")
        )
        classification = self.classified(ui)
        result, final = self.workspace.finalize(ui, classification, self.workspace.rail_plan(ui))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((final["ios"], final["android"], final["web"]), ("true", "true", "true"))

    def test_excluded_local_path_owner_selects_matching_platform(self) -> None:
        head = self.workspace.scenario(
            "gpui-ios",
            lambda: self.workspace.write("crates/platform/gpui-ios/src/lib.rs", "pub fn changed() {}\n"),
        )
        classification = self.classified(head)
        result, final = self.workspace.finalize(head, classification, self.workspace.rail_plan(head))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((final["ios"], final["android"], final["web"]), ("true", "false", "false"))

    def test_unknown_config_and_prompt_delete_or_rename_force_full(self) -> None:
        scenarios = {
            "unknown": lambda: self.workspace.write("tools/generate.py", "pass\n"),
            "action": lambda: self.workspace.write(
                ".github/actions/check/action.yml", "runs: {using: composite, steps: []}\n"
            ),
            "delete": lambda: (self.workspace.root / "assets/orchestrate/workflow.md").unlink(),
            "rename": lambda: (self.workspace.root / "assets/orchestrate/workflow.md").rename(
                self.workspace.root / "assets/orchestrate/renamed.md"
            ),
        }
        for name, mutation in scenarios.items():
            with self.subTest(name=name):
                head = self.workspace.scenario(name, mutation)
                classification = self.classified(head)
                self.assertEqual(classification["full"], "true")
                self.assertEqual(classification["skip"], "false")

    def test_invalid_rail_projection_fails_planning(self) -> None:
        head = self.workspace.scenario(
            "invalid", lambda: self.workspace.write("crates/app/src/lib.rs", "pub fn changed() {}\n")
        )
        classification = self.classified(head)
        output = self.workspace.root / ".git/invalid.out"
        result = subprocess.run(
            ["python3", str(SCRIPT), "finalize", "--root", str(self.workspace.root),
             "--from-ref", self.workspace.baseline, "--to-ref", head,
             "--skip", classification["skip"], "--full", classification["full"],
             "--dependencies", classification["dependencies"],
             "--required-work", "not-json", "--github-output", str(output)],
            text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("planning failed", result.stderr)

    def test_empty_valid_rail_projection_falls_back_to_full(self) -> None:
        head = self.workspace.scenario(
            "empty", lambda: self.workspace.write("crates/app/src/lib.rs", "pub fn changed() {}\n")
        )
        classification = self.classified(head)
        result, final = self.workspace.finalize(head, classification, {"required": []})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(final["host_full"], "true")
        self.assertEqual(final["desktop"], "true")

    def test_src_tests_module_is_not_treated_as_dev_only(self) -> None:
        head = self.workspace.scenario(
            "src-tests", lambda: self.workspace.write("crates/ui/src/tests/helpers.rs", "pub fn changed() {}\n")
        )
        classification = self.classified(head)
        result, final = self.workspace.finalize(head, classification, self.workspace.rail_plan(head))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((final["ios"], final["android"], final["web"]), ("true", "true", "true"))


class RailExecutorTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.plan = self.root / "plan.json"
        self.plan.write_text("{}", encoding="utf-8")
        self.log = self.root / "cargo.json"
        self.reader = self.root / "reader.py"
        self.reader.write_text(
            """#!/usr/bin/env python3
import os
import sys
mode = os.environ['READER_MODE']
if mode == 'fail':
    raise SystemExit(2)
if sys.argv[1] == 'cargo-scope':
    print('packages' if mode == 'empty' else mode)
elif sys.argv[1] == 'cargo-args' and mode == 'packages':
    sys.stdout.buffer.write(b'-p\\0tcode-core\\0')
elif sys.argv[1] == 'cargo-args' and mode == 'empty':
    sys.stdout.buffer.write(b'\\0')
""",
            encoding="utf-8",
        )
        cargo = self.root / "cargo"
        cargo.write_text(
            """#!/usr/bin/env python3
import json
import os
import sys
open(os.environ['CARGO_LOG'], 'w').write(json.dumps(sys.argv[1:]))
""",
            encoding="utf-8",
        )
        cargo.chmod(0o755)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def execute(self, mode: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["python3", str(EXECUTOR), "--plan", str(self.plan), "--reader", str(self.reader),
             "--work", "cargo.test", "test", "--locked"],
            env={**os.environ, "PATH": f"{self.root}{os.pathsep}{os.environ['PATH']}",
                 "READER_MODE": mode, "CARGO_LOG": str(self.log)},
            text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )

    def test_workspace_scope_is_explicit(self) -> None:
        result = self.execute("workspace")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(self.log.read_text()), ["test", "--workspace", "--locked"])

    def test_package_scope_uses_typed_nul_arguments(self) -> None:
        result = self.execute("packages")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(self.log.read_text()), ["test", "-p", "tcode-core", "--locked"])

    def test_reader_failure_never_starts_cargo(self) -> None:
        result = self.execute("fail")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.log.exists())

    def test_empty_nul_argument_never_starts_cargo(self) -> None:
        result = self.execute("empty")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.log.exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
