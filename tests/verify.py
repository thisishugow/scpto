#!/usr/bin/env python3
"""Black-box tmux verification. Docker scenarios opt in with SCPTO_IT=1."""
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time

REPO = Path(__file__).resolve().parents[1]
IT = os.environ.get("SCPTO_IT") == "1"


def run(*args, **kwargs):
    return subprocess.run(args, check=True, text=True, capture_output=True, **kwargs).stdout


def wait_for(check, description, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(0.08)
    raise AssertionError("timed out: " + description)


class Driver:
    def __init__(self, root, binary):
        self.root = root
        self.socket = "scpto-verify-" + str(os.getpid())
        self.target = "verify:0.0"
        self.left = root / "left"
        self.right = root / "right"
        self.home = root / "home"
        self.bin = root / "bin"
        self.remote_roots = []
        self.exited = False
        self.unreachable = None
        for directory in (self.left, self.right, self.home / ".ssh", self.bin):
            directory.mkdir(parents=True)
        (self.left / "folder").mkdir()
        (self.left / "folder" / "inside.txt").write_text("inside directory\n")
        (self.left / "alpha.txt").write_text("SCPTOTEXT unique preview\n")
        (self.left / "notes.md").write_text("# SCPTOMARKDOWN\n\n**bold content**\n")
        (self.left / "multi-a.txt").write_text("selected a\n")
        (self.left / "multi-b.txt").write_text("selected b\n")
        (self.left / "drag.txt").write_text("mouse payload\n")
        (self.left / "paste.txt").write_text("paste payload\n")
        (self.left / ".hidden-check").write_text("hidden\n")
        self.ssh = shutil.which("ssh")
        assert self.ssh, "ssh must be available on PATH"
        config = REPO / "target/it/ssh_config"
        if IT:
            assert config.is_file(), "run tests/it/fixtures/up.sh first"
        isolated_config = self.home / ".ssh/config"
        isolated_config.write_text("Include " + str(config) + "\n" if IT else "")
        self.script("ssh", "exec " + shlex.join([self.ssh, "-F", str(isolated_config)]) + ' "$@"\n')
        if IT:
            self.unreachable = socket.socket()
            self.unreachable.bind(("127.0.0.1", 0))
            with isolated_config.open("a") as stream:
                stream.write("Host it-unreachable\n  HostName 127.0.0.1\n  Port " +
                             str(self.unreachable.getsockname()[1]) + "\n  ConnectTimeout 1\n")
            self.script("vpn", 'printf "VPN READY\\n"\nread -r reply\nprintf "%s" "$reply" > ' +
                        shlex.quote(str(root / "vpn.done")) + "\n")
            settings = self.home / ".config/scpto/config"
            settings.parent.mkdir(parents=True)
            settings.write_text("vpn|it-unreachable|" + shlex.quote(str(self.bin / "vpn")) + "\n")
        self.script("editor", 'printf "EDITOR READY\\n"\nread -r reply\nprintf "edited-%s\\n" "$reply" >> "$1"\n')
        env = {"HOME": str(self.home), "XDG_CONFIG_HOME": str(self.home / ".config"),
               "XDG_STATE_HOME": str(self.home / ".state"), "XDG_DATA_HOME": str(self.home / ".data"),
               "PATH": str(self.bin) + os.pathsep + os.environ["PATH"], "SHELL": "/bin/sh",
               "EDITOR": str(self.bin / "editor"), "TERM": "xterm-256color"}
        launch = root / "launch.sh"
        launch.write_text("#!/bin/sh\nunset VISUAL SCPTO_CONFIG SSHTO_CONFIG\n" +
                          "\n".join("export " + key + "=" + shlex.quote(value) for key, value in env.items()) +
                          "\ncd " + shlex.quote(str(self.left)) + "\n" +
                          "sh -c " + shlex.quote("echo $$ > " + shlex.quote(str(root / "app.pid")) +
                          "; exec " + shlex.join([str(binary), "--left", str(self.left), "--right", str(self.right)])) +
                          " 2> " + shlex.quote(str(root / "stderr")) +
                          '\nresult=$?\nprintf "SCPTO_EXIT=%s\\n" "$result"\nexec /bin/sh\n')
        self.tmux("new-session", "-d", "-x", "160", "-y", "45", "-s", "verify", "sh " + shlex.quote(str(launch)))
        self.tmux("set-option", "-g", "status", "off")
        self.tmux("set-option", "-g", "window-size", "manual")
        try:
            self.wait("alpha.txt")
        except Exception:
            print("FAILING STARTUP SCREEN:\n" + self.screen(), file=sys.stderr)
            print("STDERR:\n" + (self.root / "stderr").read_text(), file=sys.stderr)
            self.close()
            raise

    def script(self, name, contents):
        path = self.bin / name
        path.write_text("#!/bin/sh\nset -eu\n" + contents)
        path.chmod(0o700)

    def tmux(self, *args):
        return run("tmux", "-L", self.socket, *args)

    def screen(self):
        return self.tmux("capture-pane", "-p", "-t", self.target)

    def wait(self, text, present=True):
        wait_for(lambda: (text in self.screen()) == present, repr(text))

    def key(self, *keys):
        self.tmux("send-keys", "-t", self.target, *keys)
        time.sleep(0.12)

    def text(self, text):
        self.tmux("send-keys", "-t", self.target, "-l", "--", text)
        time.sleep(0.12)

    def mouse(self, x, y, button=0, release=True):
        self.text(f"\x1b[<{button};{x};{y}M")
        if release:
            self.text(f"\x1b[<{button};{x};{y}m")

    def row(self, name, right=False):
        for y, line in enumerate(self.screen().splitlines()):
            start, end = (80, 160) if right else (0, 80)
            column = line.find(name, start, end)
            if column >= 0 and 2 <= y < 40:
                return column + 1, y + 1
        raise AssertionError("no file row: " + name)

    def select(self, name, right=False):
        self.mouse(*self.row(name, right))

    def ssh_run(self, host, command):
        return run(self.ssh, "-F", str(REPO / "target/it/ssh_config"), host, command)

    def remote_file(self, host, path, expected):
        def matches():
            quoted = shlex.quote(path)
            result = subprocess.run([self.ssh, "-F", str(REPO / "target/it/ssh_config"),
                                     host, "test -f " + quoted + " && cat " + quoted],
                                    text=True, capture_output=True, timeout=10)
            if result.returncode == 1:
                return False  # The transfer has not committed its final name yet.
            assert result.returncode == 0, result.stderr
            return result.stdout == expected
        wait_for(matches, "remote content " + path)

    def picker(self, host):
        self.key("o")
        self.wait("Open in the")
        self.text(host)
        self.key("Enter")
        self.wait("seed")
        self.wait("connecting", present=False)

    def local(self):
        self.wait("..")
        self.select("folder")
        self.key("Enter")
        self.wait("inside.txt")
        self.key("Home", "Enter")
        self.wait("alpha.txt")
        self.key("s")
        self.wait("Sort by")
        self.key("e")
        self.wait("Name (ext)")
        header = self.screen().splitlines()[2][:80]
        self.mouse(5, 3)
        assert self.screen().splitlines()[2][:80] != header, "header click must reverse order"
        self.key("s", "n")
        self.key("/")
        self.text("alpha")
        self.wait("notes.md", present=False)
        self.key("Escape")
        self.wait("notes.md")
        self.key(".")
        self.wait(".hidden-check")
        self.key(".")
        self.wait(".hidden-check", present=False)
        self.key("n")
        self.wait("Create directory")
        self.text("created")
        self.key("Enter")
        wait_for(lambda: (self.left / "created").is_dir(), "mkdir")
        self.wait("created/")
        self.select("created")
        self.key("r")
        self.wait("Rename")
        self.key("C-u")
        self.text("renamed")
        self.key("Enter")
        wait_for(lambda: (self.left / "renamed").is_dir(), "rename")
        assert not (self.left / "created").exists()
        self.wait("renamed/")
        self.select("renamed")
        self.key("d")
        self.wait("trash")
        self.key("y")
        wait_for(lambda: not (self.left / "renamed").exists(), "delete to trash")
        self.wait("Delete permanently", present=False)
        # Filesystem completion precedes the listing refresh. Wait for the
        # removed row to disappear before deriving the next click coordinates.
        self.wait("renamed/", present=False)
        self.select("alpha.txt")
        self.key("i")
        self.wait("SCPTOTEXT")
        self.key("i")
        self.wait("SCPTOTEXT", present=False)
        self.key("v")
        self.wait("SCPTOTEXT")
        self.key("Escape")
        self.select("notes.md")
        self.key("v")
        self.wait("SCPTOMARKDOWN")
        self.wait("(rendered)")
        assert "**bold content**" not in self.screen(), "markdown emphasis must be rendered"
        self.key("m")
        self.wait("(raw)")
        self.wait("**bold content**")
        self.key("Escape")
        self.key("?")
        self.wait("Help")
        assert not re.search(r"\bF(?:[1-9]|10)\b", self.screen()), "help contains obsolete function keys"
        self.key("Escape")
        # Copy through keys, then drag a different file through SGR mouse events.
        self.select("alpha.txt")
        self.key("c")
        wait_for(lambda: (self.right / "alpha.txt").exists(), "keyboard copy")
        assert (self.right / "alpha.txt").read_bytes() == (self.left / "alpha.txt").read_bytes()
        self.mouse(*self.row("multi-a.txt"), button=16)
        self.mouse(*self.row("multi-b.txt"), button=16)
        self.wait("2 selected")
        self.key("c")
        for name in ("multi-a.txt", "multi-b.txt"):
            wait_for(lambda name=name: (self.right / name).exists(), "selected copy " + name)
            assert (self.right / name).read_bytes() == (self.left / name).read_bytes()
        # Accepted transfers clear the source selection without another click.
        self.wait("selected", present=False)
        x, y = self.row("drag.txt")
        self.mouse(x, y, release=False)
        self.mouse(110, 20, button=32, release=False)
        self.text("\x1b[<0;110;20m")
        wait_for(lambda: (self.right / "drag.txt").exists(), "SGR drag copy")
        assert (self.right / "drag.txt").read_text() == "mouse payload\n"
        self.mouse(110, 20)
        self.text("\x1b[200~" + shlex.quote(str(self.left / "paste.txt")) + "\x1b[201~")
        self.wait("Upload")
        self.key("Enter")
        wait_for(lambda: (self.right / "paste.txt").exists(), "bracketed paste copy")
        assert (self.right / "paste.txt").read_text() == "paste payload\n"
        for width, height in [(90, 30), (50, 20), (30, 8), (160, 45)]:
            self.tmux("resize-window", "-t", "verify:0", "-x", str(width), "-y", str(height))
            time.sleep(0.25)
            if width == 30:
                self.wait("Terminal too small")
            else:
                self.wait("Terminal too small", present=False)
                self.wait("Local")
            if width == 50:
                self.key("Tab")
                self.wait("Local")
        print("PASS local navigation, sort, CRUD, preview, help, transfer, mouse, paste, resize")

    def remote(self):
        host = "it-modern-plain"
        remote = "/tmp/scpto-it-tui-" + str(os.getpid())
        self.ssh_run(host, "mkdir " + shlex.quote(remote))
        self.remote_roots.append((host, remote))
        # The picker starts at home; a private symlink provides navigation to
        # the test directory without changing shared seed data.
        link = "/home/plain/scpto-it-tui-" + str(os.getpid())
        self.ssh_run(host, "ln -s " + shlex.quote(remote) + " " + shlex.quote(link))
        self.remote_roots.append((host, link))
        self.mouse(110, 20)
        self.picker(host)
        self.select(Path(link).name, right=True)
        self.key("Enter")
        self.wait(link)
        self.select("alpha.txt")
        self.key("c")
        self.remote_file(host, remote + "/alpha.txt", "SCPTOTEXT unique preview\n")
        self.key("c")
        self.wait("Overwrite")
        self.key("s")
        self.wait("Overwrite", present=False)
        self.select("alpha.txt", right=True)
        self.key("e")
        self.wait("EDITOR READY")
        assert self.tmux("display-message", "-p", "-t", self.target, "#{alternate_on}").strip() == "0"
        self.text("saved")
        self.key("Enter")
        self.remote_file(host, remote + "/alpha.txt", "SCPTOTEXT unique preview\nedited-saved\n")
        self.wait(link)
        self.key("c")
        self.wait("Overwrite")
        self.key("o")
        wait_for(lambda: (self.left / "alpha.txt").read_text().endswith("edited-saved\n"), "download edited file")
        # The launcher records its PID immediately before exec. Only this
        # scpto process's master is terminated; the fixture stays available.
        pid = int((self.root / "app.pid").read_text())
        control = f"/tmp/scpto-{os.getuid()}/{pid}/%C"
        run(self.ssh, "-F", str(REPO / "target/it/ssh_config"), "-o", "ControlPath=" + control, "-O", "exit", host)
        time.sleep(0.5)
        self.wait("reconnecting", present=False)
        self.key("v")
        self.wait("edited-saved")
        self.key("Escape")
        self.key("t")
        wait_for(lambda: self.tmux("display-message", "-p", "-t", self.target, "#{alternate_on}").strip() == "0", "shell suspension")
        self.text("printf 'SHELL_READY\\n'")
        self.key("Enter")
        self.wait("SHELL_READY")
        self.key("C-c")
        self.text("exit")
        self.key("Enter")
        self.wait(link)
        self.key("v")
        self.wait("edited-saved")
        self.key("Escape")
        self.picker("it-modern-sudopw")
        self.key("S")
        self.wait("password")
        self.text("s")
        self.wait("********")
        self.text("cpto-test-pw")
        screen = self.screen()
        assert "********" in screen and "*********" not in screen
        assert "scpto-test-pw" not in screen
        self.key("Enter")
        self.wait("[sudo]")
        self.wait("password", present=False)
        assert "scpto-test-pw" not in self.screen()
        for path in self.home.rglob("*"):
            if path.is_file():
                assert b"scpto-test-pw" not in path.read_bytes(), "password persisted: " + str(path)
        self.key("o")
        self.wait("Open in the")
        self.text("it-unreachable")
        self.key("Enter")
        self.wait("VPN may be required")
        self.key("r")
        self.wait("VPN READY")
        self.text("resumed")
        self.key("Enter")
        wait_for(lambda: (self.root / "vpn.done").exists(), "VPN stub execution")
        assert (self.root / "vpn.done").read_text() == "resumed"
        self.wait("reconnecting")
        self.key("o")
        self.wait("Open in the")
        self.text("Local")
        self.key("Enter")
        wait_for(lambda: "Local" in self.screen().splitlines()[0][80:], "return to local endpoint")
        print("PASS host picker, upload/download, conflict, editor/shell/VPN handoff, reconnect, masked sudo", flush=True)

    def quit(self):
        self.key("q")
        self.wait("SCPTO_EXIT=0")
        assert self.tmux("display-message", "-p", "-t", self.target, "#{alternate_on}").strip() == "0"
        assert "panic" not in (self.root / "stderr").read_text().lower()
        self.exited = True
        print("PASS quit restores terminal", flush=True)

    def close(self):
        errors = []
        if not self.exited and (self.root / "app.pid").exists():
            try:
                os.kill(int((self.root / "app.pid").read_text()), signal.SIGTERM)
                self.wait("SCPTO_EXIT=")
            except ProcessLookupError:
                pass  # The owned process already exited.
            except Exception as error:
                errors.append(str(error))
        for host, path in reversed(self.remote_roots):
            try:
                self.ssh_run(host, "rm -rf -- " + shlex.quote(path))
            except Exception as error:
                errors.append(str(error))
        if self.unreachable is not None:
            self.unreachable.close()
        self.tmux("kill-server")
        if errors:
            raise RuntimeError("cleanup failures: " + "; ".join(errors))


def main():
    subprocess.run(["cargo", "build", "--message-format=json"], cwd=REPO, check=True,
                   stdout=subprocess.PIPE, text=True)
    metadata = json.loads(run("cargo", "metadata", "--no-deps", "--format-version=1", cwd=REPO))
    binary = Path(metadata["target_directory"]) / "debug/scpto"
    with tempfile.TemporaryDirectory(prefix="scpto-verify-") as directory:
        driver = None
        try:
            driver = Driver(Path(directory), binary)
            driver.local()
            if IT:
                driver.remote()
            else:
                print("SKIP Docker TUI scenarios: set SCPTO_IT=1")
            driver.quit()
        except Exception:
            if driver:
                print("FAILING SCREEN:\n" + driver.screen(), file=sys.stderr)
                print("STDERR:\n" + (driver.root / "stderr").read_text(), file=sys.stderr)
            raise
        finally:
            if driver:
                driver.close()


if __name__ == "__main__":
    main()
