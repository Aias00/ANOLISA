"""Exercise orchestration with stub commands; no component suites run."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SOURCE = Path(__file__).resolve().with_name("run-all-tests.sh")


class RunnerPathsTest(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory(prefix="anolisa-runner-")
        self.addCleanup(self.directory.cleanup)
        self.root = (Path(self.directory.name) / "repository with spaces").resolve()
        for component in ["tests", "deprecated/copilot-shell", "src/agent-sec-core", "src/agentsight", "src/tokenless", "src/agent-memory"]:
            (self.root / component).mkdir(parents=True)
        self.script = self.root / "tests/run-all-tests.sh"
        shutil.copy2(SOURCE, self.script)
        self.log = Path(self.directory.name) / "calls"
        self.bin = Path(self.directory.name) / "bin"
        self.bin.mkdir()
        for name in ["npm", "make", "cargo", "python3", "uv", "linux-sandbox"]:
            stub = self.bin / name
            stub.write_text('#!/bin/sh\nprintf "%s|%s|%s\\n" "${0##*/}" "$PWD" "$*" >> "$RUNNER_TEST_LOG"\n')
            stub.chmod(0o755)
        self.environment = dict(os.environ, PATH=f"{self.bin}:/usr/bin:/bin", RUNNER_TEST_LOG=str(self.log))

    def run_runner(self, *args: str, cwd: Path | None = None) -> subprocess.CompletedProcess[str]:
        return subprocess.run(["/bin/bash", str(self.script), *args], cwd=cwd or self.directory.name, env=self.environment, capture_output=True, text=True, timeout=10)

    def calls(self) -> list[list[str]]:
        return [line.split("|", 2) for line in self.log.read_text().splitlines()]

    def assert_sec_e2e(self) -> None:
        calls = [args for command, _, args in self.calls() if command == "python3"]
        self.assertEqual(calls, [str(self.root / "src/agent-sec-core/tests/e2e/linux-sandbox/e2e_test.py")])

    def test_sec_filter_from_foreign_directory_uses_absolute_script(self) -> None:
        result = self.run_runner("--filter", "sec")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_sec_e2e()

    def test_full_run_reaches_sec_e2e_after_shell_changes_directory(self) -> None:
        result = self.run_runner(cwd=self.root)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_sec_e2e()
        for command, directory, _ in self.calls():
            if command == "npm":
                self.assertEqual(Path(directory), self.root / "deprecated/copilot-shell")
            elif command == "cargo":
                self.assertEqual(Path(directory), self.root / "src/agentsight")

    def test_caller_directory_is_unchanged_when_a_suite_is_sourced(self) -> None:
        prefix = self.script.read_text().split('if [ -z "$FILTER" ]; then', 1)[0]
        result = subprocess.run(["/bin/bash", "-c", prefix + '\nROOT_DIR="$PWD"\nrun_shell\nprintf "CALLER=%s\\n" "$PWD"'], cwd=self.root, env=self.environment, capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"CALLER={self.root}\n", result.stdout)

    def test_unknown_filter_fails_without_running_a_suite(self) -> None:
        result = self.run_runner("--filter", "unknown")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Unknown filter", result.stdout)
        self.assertFalse(self.log.exists())

    def test_component_failure_stops_the_runner(self) -> None:
        (self.bin / "npm").write_text("#!/bin/sh\nexit 7\n")
        result = self.run_runner()
        self.assertEqual(result.returncode, 7)
        self.assertNotIn("All tests completed successfully", result.stdout)

    def test_runner_keeps_executable_permission(self) -> None:
        self.assertTrue(os.access(self.script, os.X_OK))


if __name__ == "__main__":
    unittest.main()
