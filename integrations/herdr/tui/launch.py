#!/usr/bin/env python3
"""Open or focus this plugin's scpto pane in the invoking Herdr server."""

import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys

PLUGIN_ID = "scpto.files"
ENTRYPOINT = "files"


class LaunchError(Exception):
    """A launch or host-protocol failure that must be visible in the action log."""

    def __init__(self, message, code=None):
        super().__init__(message)
        self.code = code


def required_env(name):
    """Read a value that Herdr supplies to every plugin action."""
    value = os.environ.get(name)
    if not value:
        raise LaunchError(f"Missing {name}; invoke the scpto.files.open plugin action.")
    return value


def call_herdr(binary, *args, expected):
    """Call the host through argv and require its documented response variant."""
    command = subprocess.run(
        [binary, *args], stdin=subprocess.DEVNULL, capture_output=True, text=True,
    )
    output = command.stderr if command.returncode else command.stdout
    try:
        response = json.loads(output)
    except json.JSONDecodeError as error:
        raise LaunchError(f"Herdr returned invalid JSON (exit {command.returncode}).") from error
    if not isinstance(response, dict):
        raise LaunchError("Herdr returned an invalid response object.")
    if command.returncode:
        failure = response.get("error")
        if not isinstance(failure, dict):
            raise LaunchError(f"Herdr failed without a structured error (exit {command.returncode}).")
        raise LaunchError(
            f"Herdr command failed: {failure.get('message', 'unknown error')}",
            failure.get("code"),
        )
    result = response.get("result")
    if not isinstance(result, dict) or result.get("type") != expected:
        raise LaunchError(f"Herdr did not return {expected}.")
    return result


def owned_terminal(result):
    """Validate the host's plugin ownership before retaining a terminal identity."""
    owner = result.get("plugin_pane")
    if not isinstance(owner, dict) or owner.get("plugin_id") != PLUGIN_ID or owner.get("entrypoint") != ENTRYPOINT:
        raise LaunchError("Herdr returned a pane owned by another plugin or entrypoint.")
    pane = owner.get("pane")
    if not isinstance(pane, dict) or not isinstance(pane.get("terminal_id"), str) or not pane["terminal_id"]:
        raise LaunchError("Herdr returned a pane without a terminal identity.")
    return pane["terminal_id"]


def save_terminal(state, terminal_id):
    """Replace the cache while its file lock is held."""
    state.seek(0)
    json.dump({"terminal_id": terminal_id}, state)
    state.truncate()
    state.flush()


def open_or_focus(context, binary, state):
    """Reuse an owned live terminal, or open a tab in the invocation's workspace."""
    cached = state.read()
    terminal_id = None
    if cached:
        record = json.loads(cached)
        if not isinstance(record, dict) or not isinstance(record.get("terminal_id"), (str, type(None))):
            raise LaunchError("Invalid scpto session cache; remove the plugin state file and retry.")
        terminal_id = record.get("terminal_id")
    if terminal_id:
        result = call_herdr(binary, "pane", "list", expected="pane_list")
        panes = result.get("panes")
        if not isinstance(panes, list) or any(not isinstance(pane, dict) for pane in panes):
            raise LaunchError("Herdr returned an invalid pane list.")
        for pane in panes:
            if pane.get("terminal_id") != terminal_id:
                continue
            pane_id = pane.get("pane_id")
            if not isinstance(pane_id, str) or not pane_id:
                raise LaunchError("Herdr returned a pane without a pane identity.")
            try:
                result = call_herdr(binary, "plugin", "pane", "focus", pane_id, expected="plugin_pane_focused")
            except LaunchError as error:
                if error.code == "plugin_pane_not_found":
                    save_terminal(state, None)
                raise
            if owned_terminal(result) != terminal_id:
                raise LaunchError("Herdr focused a different terminal; refusing to replace the session cache.")
            print("Focused the running scpto session; its directories are unchanged.")
            return

    cwd = context.get("focused_pane_cwd") or context.get("workspace_cwd")
    workspace = context.get("workspace_id")
    if not isinstance(cwd, str) or not Path(cwd).is_absolute() or not Path(cwd).is_dir():
        raise LaunchError("The invoking pane or workspace must have an existing absolute local directory.")
    if not isinstance(workspace, str) or not workspace:
        raise LaunchError("Open a local Herdr workspace before launching scpto.")
    if shutil.which("scpto") is None:
        raise LaunchError("scpto is not on the Herdr server PATH; install scpto and restart the server when convenient.")
    result = call_herdr(
        binary, "plugin", "pane", "open", "--plugin", PLUGIN_ID,
        "--entrypoint", ENTRYPOINT, "--placement", "tab", "--workspace", workspace,
        "--cwd", cwd, "--focus", expected="plugin_pane_opened",
    )
    save_terminal(state, owned_terminal(result))
    print("Opened scpto in a terminal tab.")


def main():
    """Serialize action invocations using a cache scoped to this socket lifetime."""
    try:
        context = json.loads(required_env("HERDR_PLUGIN_CONTEXT_JSON"))
        if not isinstance(context, dict):
            raise LaunchError("HERDR_PLUGIN_CONTEXT_JSON must contain an object.")
        binary = required_env("HERDR_BIN_PATH")
        socket_path = Path(required_env("HERDR_SOCKET_PATH"))
        if not socket_path.is_absolute():
            raise LaunchError("HERDR_SOCKET_PATH must be absolute.")
        socket = socket_path.stat()
        if not stat.S_ISSOCK(socket.st_mode):
            raise LaunchError("HERDR_SOCKET_PATH must identify the running Herdr socket.")
        identity = f"{socket_path.resolve()}:{socket.st_dev}:{socket.st_ino}:{socket.st_ctime_ns}"
        digest = hashlib.sha256(identity.encode()).hexdigest()
        directory = Path(required_env("HERDR_PLUGIN_STATE_DIR"))
        if not directory.is_absolute() or not directory.is_dir():
            raise LaunchError("HERDR_PLUGIN_STATE_DIR must be an existing absolute directory.")
        cache = directory / f"scpto-{digest}.json"
        descriptor = os.open(cache, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        with os.fdopen(descriptor, "r+", encoding="utf-8") as state:
            fcntl.flock(state, fcntl.LOCK_EX)
            open_or_focus(context, binary, state)
    except (LaunchError, OSError, ValueError) as error:
        print(f"Cannot open scpto: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
