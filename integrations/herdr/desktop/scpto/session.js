// Launch and reuse the integration-owned local scpto terminal.

const NAME = "scpto-files";
const LABEL = "scpto.integration";

async function core(hc, args) {
  const reply = await hc(["--json", ...args]);
  let value;
  try {
    value = JSON.parse(reply.code === 0 ? reply.stdout : reply.stderr);
  } catch {
    throw new Error("Herdr core returned an invalid response. Check its version and logs.");
  }
  if (reply.code !== 0) {
    const detail = value.error;
    const error = new Error(detail?.message || "Herdr core command failed.");
    error.kind = detail?.kind;
    throw error;
  }
  return value;
}

function owned(terminal) {
  if (terminal.name !== NAME || terminal.labels?.[LABEL] !== "desktop") {
    throw new Error(`The terminal name "${NAME}" is already in use by another program. Rename or close it first.`);
  }
  if (!Array.isArray(terminal.argv) || terminal.argv[0] !== "scpto") {
    throw new Error("The previous scpto terminal now runs another program. Close that terminal before opening scpto again.");
  }
  if (typeof terminal.address !== "string" || !terminal.address) {
    throw new Error("Herdr core did not return a terminal address.");
  }
  return terminal;
}

async function current(hc) {
  let terminal;
  try {
    terminal = await core(hc, ["get", NAME]);
  } catch (error) {
    if (error.kind === "not_found") return null;
    throw error;
  }
  if (terminal.state === "finished") return null;
  if (terminal.state !== "live") throw new Error("Herdr core returned an unknown terminal state.");
  return owned(terminal);
}

// The host functions are the documented Herdr mod API; keeping them at this
// boundary lets tests exercise lifecycle decisions without a GPUI renderer.
export function createLauncher(host) {
  let pending = null;

  async function open(waitFor) {
    const space = host.spaces().find((item) => item.active);
    if (!space || space.machine !== null) {
      throw new Error("Select a local Herdr space before opening scpto.");
    }
    const focus = host.focused();
    const folder = (focus?.space === space.id && focus.cwd) || space.folder || host.home();
    if (typeof folder !== "string" || !folder.startsWith("/")) {
      throw new Error("The local working directory must be an absolute path.");
    }

    let terminal = await current(host.hc);
    let reused = terminal !== null;
    if (!terminal) {
      try {
        const started = await core(host.hc, [
          "run", "--bg", "--name", NAME, "--label", `${LABEL}=desktop`,
          "--no-parent", "--no-notify", "--cwd", folder,
          "--", "scpto", "--left", folder,
        ]);
        if (typeof started.address !== "string" || !started.address) {
          throw new Error("Herdr core did not return a terminal address.");
        }
        terminal = await core(host.hc, ["get", started.address]);
        if (terminal.state !== "live") {
          throw new Error("scpto exited before its terminal opened. Check that scpto and OpenSSH are installed.");
        }
        owned(terminal);
      } catch (error) {
        // Another visible copy can win the core's atomic unique-name check.
        if (error.kind !== "name_taken") throw error;
        terminal = await current(host.hc);
        if (!terminal) throw error;
        reused = true;
      }
    }

    // Core process creation precedes the desktop's live terminal mirror.
    // Wait for that specific terminal before issuing a command against it.
    await waitFor(
      () => host.terminals().some((item) => item.address === terminal.address && item.state === "live"),
      "Herdr has not discovered the scpto terminal yet. Try Open scpto again.",
    );
    if (host.panes().some((pane) => pane.terminal === terminal.address)) {
      host.run("pane.focus", { terminal: terminal.address });
    } else {
      host.run("space.select", { id: space.id });
      host.run("terminal.keep", { terminal: terminal.address, place: "tab" });
    }
    await waitFor(
      () => host.panes().some((pane) => pane.terminal === terminal.address),
      "Herdr has not attached the scpto tab yet. Try Open scpto again.",
    );
    return { address: terminal.address, reused };
  }

  return function launch(waitFor) {
    if (!pending) pending = open(waitFor).finally(() => { pending = null; });
    return pending;
  };
}
