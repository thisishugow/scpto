"""Exercise package preparation and terminal actions without a real Herdr server."""

import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import textwrap
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[1]
LAUNCHER = ROOT / "integrations/herdr/tui/launch.py"

FAKE_HERDR = r'''
import json
import os
from pathlib import Path
import sys

path = Path(os.environ["FAKE_HERDR_STATE"])
state = json.loads(path.read_text())
args = sys.argv[1:]
state.setdefault("calls", []).append(args)

def finish(value, code=0):
    path.write_text(json.dumps(state))
    print(json.dumps(value), file=sys.stderr if code else sys.stdout)
    raise SystemExit(code)

def owned(kind):
    return {"result": {"type": kind, "plugin_pane": {
        "plugin_id": state.get("owner", "scpto.files"), "entrypoint": "files", "pane": state["pane"],
    }}}

if args == ["pane", "list"]:
    finish({"result": {"type": "pane_list", "panes": [state["pane"]] if state.get("pane") else []}})
if args[:3] == ["plugin", "pane", "open"]:
    if state.get("open_error"):
        finish({"error": {"code": "spawn_failed", "message": "fixture launch failed"}}, 1)
    count = state.get("opens", 0) + 1
    state["opens"] = count
    state["pane"] = {"pane_id": "w1:p" + str(count), "terminal_id": "term" + str(count)}
    finish(owned("plugin_pane_opened"))
if args[:3] == ["plugin", "pane", "focus"]:
    if state.get("focus_error"):
        finish({"error": {"code": "plugin_pane_not_found", "message": "fixture pane disappeared"}}, 1)
    finish(owned("plugin_pane_focused"))
finish({"error": {"code": "unexpected", "message": "Unexpected fixture command"}}, 1)
'''


class PackageTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="scpto-package-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / "repo"
        (self.repo / "tools").mkdir(parents=True)
        shutil.copyfile(ROOT / "tools/herdr.py", self.repo / "tools/herdr.py")
        shutil.copyfile(ROOT / "LICENSE", self.repo / "LICENSE")
        shutil.copytree(ROOT / "integrations/herdr", self.repo / "integrations/herdr")
        (self.repo / "Cargo.toml").write_text('[package]\nversion = "2.3.4-test"\n')
        self.output = self.root / "output"

    def prepare(self):
        return subprocess.run(
            [sys.executable, str(self.repo / "tools/herdr.py"), str(self.output)],
            capture_output=True, text=True, check=False,
        )

    def test_package_uses_cargo_version_and_only_runtime_files(self):
        # A host checker may create editor metadata; it is not part of a package.
        (self.repo / "integrations/herdr/desktop/scpto/jsconfig.json").write_text("{}")
        result = self.prepare()
        self.assertEqual(result.returncode, 0, result.stderr)
        files = {path.relative_to(self.output).as_posix() for path in self.output.rglob("*") if path.is_file()}
        self.assertEqual(files, {
            "tui/herdr-plugin.toml", "tui/launch.py", "desktop/scpto/main.js",
            "desktop/scpto/session.js", "desktop/scpto/mod.json",
            "tui/LICENSE", "desktop/scpto/LICENSE",
        })
        for package in ("tui", "desktop/scpto"):
            self.assertEqual((self.output / package / "LICENSE").read_bytes(), (ROOT / "LICENSE").read_bytes())
        manifest = tomllib.loads((self.output / "tui/herdr-plugin.toml").read_text())
        self.assertEqual(manifest["version"], "2.3.4-test")
        self.assertEqual(manifest["id"], "scpto.files")
        self.assertEqual(manifest["panes"][0]["command"], ["scpto"])
        for relative in ("main.js", "session.js", "mod.json"):
            self.assertEqual(
                (self.output / "desktop/scpto" / relative).read_bytes(),
                (self.repo / "integrations/herdr/desktop/scpto" / relative).read_bytes(),
            )

    def test_existing_destination_is_not_changed(self):
        self.output.mkdir()
        marker = self.output / "keep.txt"
        marker.write_text("existing user content")
        result = self.prepare()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Herdr preparation failed", result.stderr)
        self.assertEqual(list(self.output.iterdir()), [marker])
        self.assertEqual(marker.read_text(), "existing user content")

    def test_invalid_inputs_do_not_create_a_package(self):
        template = self.repo / "integrations/herdr/tui/herdr-plugin.toml.in"
        original = template.read_text()
        for kind in ("symlink", "unresolved placeholder", "invalid version"):
            with self.subTest(kind=kind):
                if template.is_symlink() or template.exists():
                    template.unlink()
                template.write_text(original)
                (self.repo / "Cargo.toml").write_text('[package]\nversion = "2.3.4"\n')
                if kind == "symlink":
                    template.unlink()
                    template.symlink_to(ROOT / "integrations/herdr/tui/herdr-plugin.toml.in")
                elif kind == "unresolved placeholder":
                    template.write_text(original + '\nunknown = "@@UNRESOLVED@@"\n')
                else:
                    (self.repo / "Cargo.toml").write_text('[package]\nversion = "not a version"\n')
                result = self.prepare()
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(self.output.exists())


class LauncherTests(unittest.TestCase):
    def setUp(self):
        # Short socket paths also work under macOS's Unix socket path limit.
        self.temp = tempfile.TemporaryDirectory(prefix="scpto-herdr-", dir="/tmp")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.state_dir = self.root / "plugin-state"
        self.state_dir.mkdir()
        self.workspace = self.root / "it's a project; $(touch injected)"
        self.workspace.mkdir()
        self.socket_path = self.root / "herdr.sock"
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.bind(str(self.socket_path))
        self.addCleanup(self.socket.close)
        self.fake_state = self.root / "host.json"
        self.write_host({})
        self.fake = self.bin / "herdr"
        self.fake.write_text(f"#!{sys.executable}\n" + textwrap.dedent(FAKE_HERDR))
        self.fake.chmod(0o755)
        scpto = self.bin / "scpto"
        scpto.write_text("#!/bin/sh\nexit 0\n")
        scpto.chmod(0o755)
        self.context = {"focused_pane_cwd": str(self.workspace), "workspace_cwd": str(self.root), "workspace_id": "w1"}
        self.env = {
            **os.environ, "PATH": str(self.bin), "HOME": str(self.root),
            "XDG_CONFIG_HOME": str(self.root / "config"), "XDG_STATE_HOME": str(self.root / "state"),
            "XDG_DATA_HOME": str(self.root / "data"), "XDG_CACHE_HOME": str(self.root / "cache"),
            "HERDR_BIN_PATH": str(self.fake), "HERDR_SOCKET_PATH": str(self.socket_path),
            "HERDR_PLUGIN_STATE_DIR": str(self.state_dir), "FAKE_HERDR_STATE": str(self.fake_state),
        }

    def write_host(self, state):
        self.fake_state.write_text(json.dumps(state))

    def host(self):
        return json.loads(self.fake_state.read_text())

    def command(self, **changes):
        env = {**self.env, "HERDR_PLUGIN_CONTEXT_JSON": json.dumps(self.context), **changes}
        return [sys.executable, str(LAUNCHER)], env

    def launch(self, **changes):
        args, env = self.command(**changes)
        return subprocess.run(args, env=env, capture_output=True, text=True, check=False)

    def assert_success(self, result):
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_launch_reuses_moved_terminal_and_restarts_after_exit(self):
        self.assert_success(self.launch())
        host = self.host()
        opened = host["calls"][0]
        self.assertEqual(opened[opened.index("--cwd") + 1], str(self.workspace))
        self.assertEqual(opened[opened.index("--workspace") + 1], "w1")
        self.assertEqual(opened[opened.index("--placement") + 1], "tab")
        self.assertFalse((self.root / "injected").exists())
        # Pane ids can change when a terminal moves to another workspace.
        host["pane"]["pane_id"] = "w2:p7"
        self.write_host(host)
        self.context["focused_pane_cwd"] = str(self.root)
        self.assert_success(self.launch())
        host = self.host()
        self.assertEqual(host["opens"], 1)
        self.assertEqual(host["calls"][-1], ["plugin", "pane", "focus", "w2:p7"])
        host["pane"] = None
        self.write_host(host)
        self.assert_success(self.launch())
        self.assertEqual(self.host()["opens"], 2)

    def test_workspace_directory_is_used_without_focused_cwd(self):
        self.context["focused_pane_cwd"] = None
        self.assert_success(self.launch())
        args = self.host()["calls"][0]
        self.assertEqual(args[args.index("--cwd") + 1], str(self.root))

    def test_bad_context_and_missing_binary_never_call_the_host(self):
        for value in ("", "not JSON", "[]", json.dumps({**self.context, "focused_pane_cwd": "relative"}),
                      json.dumps({**self.context, "focused_pane_cwd": str(self.root / "missing")}),
                      json.dumps({**self.context, "workspace_id": ""})):
            with self.subTest(context=value):
                result = self.launch(HERDR_PLUGIN_CONTEXT_JSON=value)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("Cannot open scpto", result.stderr)
                self.assertEqual(self.host().get("calls", []), [])
        (self.bin / "scpto").unlink()
        result = self.launch()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not on the Herdr server PATH", result.stderr)
        self.assertEqual(self.host().get("calls", []), [])

    def test_host_failure_and_wrong_ownership_are_visible(self):
        for host, message in [({"open_error": True}, "fixture launch failed"), ({"owner": "another.plugin"}, "another plugin")]:
            with self.subTest(host=host):
                self.write_host(host)
                result = self.launch()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(message, result.stderr)
                self.assertEqual(len(self.host()["calls"]), 1)

    def test_failed_focus_does_not_blindly_launch_a_duplicate(self):
        self.assert_success(self.launch())
        host = self.host()
        host["focus_error"] = True
        self.write_host(host)
        result = self.launch()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fixture pane disappeared", result.stderr)
        self.assertEqual(self.host()["opens"], 1)

    def test_concurrent_invocations_open_once(self):
        args, env = self.command()
        processes = [subprocess.Popen(args, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) for _ in range(2)]
        try:
            for process in processes:
                _, error = process.communicate(timeout=10)
                self.assertEqual(process.returncode, 0, error)
        finally:
            for process in processes:
                if process.poll() is None:
                    process.kill()
                    process.communicate()
        self.assertEqual(self.host()["opens"], 1)
        self.assertEqual(sum(call[:3] == ["plugin", "pane", "focus"] for call in self.host()["calls"]), 1)

    def test_restarted_socket_does_not_reuse_previous_server_terminal(self):
        self.assert_success(self.launch())
        self.socket.close()
        self.socket_path.unlink()
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.bind(str(self.socket_path))
        self.addCleanup(self.socket.close)
        self.write_host({})
        self.assert_success(self.launch())
        self.assertEqual(self.host()["calls"][0][:3], ["plugin", "pane", "open"])


if __name__ == "__main__":
    unittest.main()
