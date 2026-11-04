import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

// Load the actual dependency-free controller as ESM without changing the
// desktop mod's .js filenames or adding a Node package to the application.
const source = await readFile(new URL("../integrations/herdr/desktop/scpto/session.js", import.meta.url), "utf8");
const { createLauncher } = await import(`data:text/javascript;base64,${Buffer.from(source).toString("base64")}`);

const reply = (value) => ({ code: 0, stdout: JSON.stringify(value), stderr: "" });
const failure = (kind, message, code = 1) => ({ code, stdout: "", stderr: JSON.stringify({ error: { kind, message } }) });
const absent = () => failure("not_found", "No such terminal", 3);
const terminal = (changes = {}) => ({
  address: "t17", name: "scpto-files", state: "live", argv: ["scpto", "--left", "/workspace"],
  labels: { "scpto.integration": "desktop" }, ...changes,
});

function fixture(replies, options = {}) {
  const calls = [];
  const actions = [];
  const remaining = [...replies];
  const visibleTerminals = [...(options.terminals ?? [terminal()])];
  const visiblePanes = [...(options.panes ?? [])];
  const host = {
    hc: async (args) => {
      calls.push(args);
      assert.ok(remaining.length, `Unexpected core command: ${JSON.stringify(args)}`);
      return remaining.shift();
    },
    run: (command, args) => {
      actions.push({ command, args });
      if (command === "terminal.keep" && !options.deferAttach) visiblePanes.push({ terminal: args.terminal });
    },
    spaces: () => options.spaces ?? [{ id: "s1", active: true, machine: null, folder: "/workspace" }],
    focused: () => options.focus ?? { space: "s1", cwd: "/workspace" },
    home: () => "/home/example",
    panes: () => visiblePanes,
    terminals: () => visibleTerminals,
  };
  const launch = createLauncher(host);
  const immediate = async (ready, message) => {
    if (!ready()) throw new Error(message);
  };
  return {
    launch: (waitFor = immediate) => launch(waitFor), calls, actions, remaining,
    visibleTerminals, visiblePanes,
  };
}

function started(changes = {}) {
  return [absent(), reply({ address: "t17", state: "live" }), reply(terminal(changes))];
}

test("new session passes cwd literally and attaches a full terminal tab", async () => {
  const cwd = "/workspace/it's a project; $(touch injected)";
  const f = fixture(started(), { focus: { space: "s1", cwd } });
  assert.deepEqual(await f.launch(), { address: "t17", reused: false });
  const launch = f.calls.find((args) => args.includes("run"));
  assert.equal(launch[launch.indexOf("--cwd") + 1], cwd);
  assert.deepEqual(launch.slice(launch.indexOf("--") + 1), ["scpto", "--left", cwd]);
  assert.ok(launch.includes("--no-parent"));
  assert.ok(launch.includes("--no-notify"));
  assert.deepEqual(f.actions, [
    { command: "space.select", args: { id: "s1" } },
    { command: "terminal.keep", args: { terminal: "t17", place: "tab" } },
  ]);
  assert.equal(f.remaining.length, 0);
});

test("existing owned session focuses its pane without changing its directory", async () => {
  const f = fixture([reply(terminal())], {
    focus: { space: "s1", cwd: "/another-project" }, panes: [{ terminal: "t17" }],
  });
  assert.deepEqual(await f.launch(), { address: "t17", reused: true });
  assert.deepEqual(f.calls, [["--json", "get", "scpto-files"]]);
  assert.deepEqual(f.actions, [{ command: "pane.focus", args: { terminal: "t17" } }]);
});

test("finished process starts again instead of focusing a stale tab", async () => {
  const f = fixture([reply(terminal({ state: "finished" })), ...started().slice(1)]);
  assert.equal((await f.launch()).reused, false);
  assert.equal(f.calls.filter((args) => args.includes("run")).length, 1);
});

test("working directory belongs to the selected local space", async (t) => {
  for (const [folder, expected] of [["/selected-project", "/selected-project"], ["", "/home/example"]]) {
    await t.test(folder || "home when the space has no folder", async () => {
      const f = fixture(started(), {
        spaces: [{ id: "s1", active: true, machine: null, folder }],
        focus: { space: "other-space", cwd: "/wrong-project" },
      });
      await f.launch();
      const args = f.calls.find((entry) => entry.includes("run"));
      assert.equal(args[args.indexOf("--cwd") + 1], expected);
    });
  }
});

test("remote spaces, absent spaces, and relative directories never start a process", async (t) => {
  for (const options of [
    { spaces: [{ id: "remote/s1", active: true, machine: "remote", folder: "/remote" }] },
    { spaces: [] },
    { focus: { space: "s1", cwd: "relative/path" } },
  ]) {
    await t.test(JSON.stringify(options), async () => {
      const f = fixture([], options);
      await assert.rejects(f.launch(), /local Herdr space|absolute path/);
      assert.equal(f.calls.length, 0);
      assert.equal(f.actions.length, 0);
    });
  }
});

test("an unrelated named terminal or a recovery shell is never adopted", async (t) => {
  for (const value of [terminal({ labels: {} }), terminal({ argv: ["/bin/zsh", "-l"] })]) {
    await t.test(value.argv[0], async () => {
      const f = fixture([reply(value)]);
      await assert.rejects(f.launch(), /another program/);
      assert.equal(f.calls.length, 1);
      assert.equal(f.actions.length, 0);
    });
  }
});

test("launch failure is visible and a later activation can retry", async () => {
  const f = fixture([absent(), failure("spawn_failed", "scpto executable not found"), ...started()]);
  await assert.rejects(f.launch(), /scpto executable not found/);
  assert.equal(f.actions.length, 0);
  assert.equal((await f.launch()).address, "t17");
});

test("concurrent activations share one launch", async () => {
  const f = fixture(started());
  const first = f.launch();
  const second = f.launch();
  const results = await Promise.all([first, second]);
  assert.deepEqual(results.map((result) => result.address), ["t17", "t17"]);
  assert.equal(f.calls.filter((args) => args.includes("run")).length, 1);
  assert.equal(f.actions.filter((action) => action.command === "terminal.keep").length, 1);
});

test("another mod copy winning the unique name is reused after ownership verification", async () => {
  const f = fixture([absent(), failure("name_taken", "Name in use"), reply(terminal())], {
    panes: [{ terminal: "t17" }],
  });
  assert.deepEqual(await f.launch(), { address: "t17", reused: true });
  assert.deepEqual(f.actions, [{ command: "pane.focus", args: { terminal: "t17" } }]);
});

test("an immediately exited process and invalid core response cannot produce a usable tab", async (t) => {
  for (const replies of [
    started({ state: "finished" }),
    [{ code: 0, stdout: "not JSON", stderr: "" }],
  ]) {
    await t.test(replies.length === 1 ? "invalid JSON" : "early exit", async () => {
      const f = fixture(replies);
      await assert.rejects(f.launch(), /exited|invalid response/);
      assert.equal(f.actions.length, 0);
    });
  }
});


test("attachment waits for the desktop terminal mirror and pane before reporting success", async () => {
  const f = fixture(started(), { terminals: [], deferAttach: true });
  let stage = 0;
  await f.launch(async (ready) => {
    assert.equal(ready(), false);
    if (stage === 0) {
      assert.equal(f.actions.length, 0);
      f.visibleTerminals.push(terminal());
    } else {
      assert.equal(f.actions.filter((action) => action.command === "terminal.keep").length, 1);
      f.visiblePanes.push({ terminal: "t17" });
    }
    stage += 1;
    assert.equal(ready(), true);
  });
  assert.equal(stage, 2);
});

test("readiness timeout is visible and retries attach the existing process", async (t) => {
  for (const stage of ["terminal", "pane"]) {
    await t.test(stage, async () => {
      const f = fixture([...started(), reply(terminal())], {
        terminals: stage === "terminal" ? [] : [terminal()], deferAttach: true,
      });
      await assert.rejects(f.launch(), /not discovered|not attached/);
      f.visibleTerminals.push(terminal());
      f.visiblePanes.push({ terminal: "t17" });
      assert.deepEqual(await f.launch(), { address: "t17", reused: true });
      assert.equal(f.calls.filter((args) => args.includes("run")).length, 1);
      assert.deepEqual(f.actions.at(-1), { command: "pane.focus", args: { terminal: "t17" } });
    });
  }
});
