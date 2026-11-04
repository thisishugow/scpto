# Herdr integrations

These adapters run the installed scpto executable in a full terminal tab. The
standalone application, dual-pane interface, SSH authentication, and transfer
engine remain the same. The desktop mod and terminal plugin are separate
packages for Herdr's two extension systems.

## Requirements and preparation

- Install scpto and OpenSSH on the local host and make both available on `PATH`.
- Desktop: Herdr app 0.1.12 with herdr-core 0.1.17.
- Terminal: Herdr 0.9.5 and Python 3 on `PATH` for the launcher.
- Preparing packages from this repository requires Python 3.11+. No npm or
  third-party Python packages are needed at runtime.

From a repository checkout:

```sh
python3 tools/herdr.py target/herdr
```

The output directory must not already exist. Preparation reads the version from
`Cargo.toml`, renders the terminal manifest, and copies only the adapter files:

```text
target/herdr/
  desktop/scpto/main.js
  desktop/scpto/session.js
  desktop/scpto/mod.json
  desktop/scpto/LICENSE
  tui/herdr-plugin.toml
  tui/launch.py
  tui/LICENSE
```

Preparation does not build or install scpto, download binaries, or change Herdr
configuration. These packages support local installation; no marketplace listing
or GitHub plugin installation is required.

## Desktop mod

Check the generated mod and copy it into the app's mods directory. No layout
clone or persistent toolbar is needed. The copy command refuses to replace an
existing `scpto` mod; use the update instructions for an existing installation.

```sh
herdr-app check target/herdr/desktop/scpto
app_home="${HERDR_APP_HOME:-$HOME/.config/herdr-app}"
mkdir -p "$app_home/mods"
test ! -e "$app_home/mods/scpto" && cp -R target/herdr/desktop/scpto "$app_home/mods/scpto"
```

Choose an unused shortcut and merge it into the `global` object in
`$app_home/keys.json`. Check all existing sections for conflicts and preserve
other bindings. For example, on macOS:

```json
"cmd-shift-f": "mod.open:scpto"
```

On Linux, use an unused binding such as `ctrl-shift-f` with the same action.
Select a local space and press the shortcut to open or focus scpto directly.
You can also activate it from the CLI:

```sh
herdr-app do mod.open '{"name":"scpto"}'
```

A brief progress popup closes automatically when the terminal is ready; there
is no second launch button. Errors stay in that popup with a **Retry** button.
Escape or clicking outside dismisses it. Activating the shortcut while the
popup is open dismisses it too. Herdr 0.1.12 does not automatically list this
mod in its command palette.

The first launch uses the selected space's focused terminal directory,
otherwise the space's folder or your home directory. A remote space is
rejected; select remote SFTP hosts inside scpto instead. This is a terminal
integration: the existing scpto interface runs in a full terminal tab.

Repeated activation focuses the live integration-owned session without changing
its directories. One desktop session is shared across local spaces. After scpto
exits, the next activation starts a new session. The reserved core terminal name
is `scpto-files`; an unrelated terminal using that name produces an error rather
than being adopted.

## Terminal plugin

Link the generated package using its absolute path:

```sh
herdr plugin link "$PWD/target/herdr/tui"
```

From the local workspace where you want to browse files:

```sh
herdr plugin action invoke scpto.files.open
```

The action uses the focused pane's local directory, falling back to the workspace
directory, and opens the declared `files` pane as a full tab. Invoke it again to
focus the existing plugin session on that Herdr server. Once scpto exits, a later
invocation opens a new session. The launcher serializes concurrent invocations
and tracks the terminal identity separately for each server.

An optional command keybinding in Herdr's `config.toml` can invoke the same action:

```toml
[[keys.command]]
key = "prefix+f"
type = "plugin_action"
command = "scpto.files.open"
description = "open scpto"
```

Choose an unused binding in your configuration. The ordinary pane entrypoint is
also available to Herdr, but use the action for working-directory selection and
session reuse. The server must run locally: its filesystem, SSH configuration,
and installed tools are the ones scpto uses.

## Updating and removing

Prepare an updated package into a new directory, for example
`python3 tools/herdr.py target/herdr-next`. Close any scpto launch popup before
updating the desktop mod; its terminal session can keep running. Quit scpto
normally before replacing or unlinking the terminal plugin.

For the desktop mod, replace its runtime files and license from the new package:

```sh
app_home="${HERDR_APP_HOME:-$HOME/.config/herdr-app}"
cp target/herdr-next/desktop/scpto/main.js target/herdr-next/desktop/scpto/session.js target/herdr-next/desktop/scpto/mod.json target/herdr-next/desktop/scpto/LICENSE "$app_home/mods/scpto/"
```

Herdr hot-reloads the mod. If upgrading from the toolbar version, replace its
`scpto.open` keybinding with `mod.open:scpto`. Remove only the block whose
`content.mod` is `scpto` from your custom layout, preserving its other blocks.
Alternatively, switch back to the shipped layout with
`herdr-app do layout.switch '{"name":"main"}'`; your terminal sessions remain.
Changing the manifest alone does not remove a block already in a saved layout.

Remove the mod by deleting its `mods/scpto` directory and removing any
`mod.open:scpto` keybinding you added. Do not remove other mods or reset the app
layout.

For the terminal plugin, unlink the previous package and link the new one:

```sh
herdr plugin unlink scpto.files
herdr plugin link "$PWD/target/herdr-next/tui"
```

To remove the plugin, run only the unlink command and remove any
`scpto.files.open` keybinding you added. Unlinking does not uninstall scpto.

## Session behavior and verification

Use a full tab at least 60 columns wide for both panes. Below that width scpto
shows one pane at a time. Quit from scpto when a transfer or edit needs an orderly
shutdown. Closing a pane can terminate the process; closing the desktop window
or detaching the terminal client is different from stopping its core/server.
A restored tab after a server restart does not mean an interrupted transfer has
automatically restarted.

The adapters' session reuse is scoped to their own host. Desktop, terminal, and
standalone processes can still run concurrently. scpto's shared state is
last-writer-wins, and simultaneous transfers to the same destination can contend
for the same partial file. Do not treat the adapters as cross-process locking.

Run the dependency-free adapter tests from the checkout:

```sh
python3 -m unittest discover -s tests -p 'test_herdr.py'
node --test tests/herdr-desktop.test.mjs
```

For live checks, use disposable HOME/XDG directories, a separate `HERDR_APP_HOME`
and `HC_SESSION` for the desktop, and a separate terminal configuration/session.
Never stop the user's shared server to test recovery. Verify launch directory,
repeat activation, quit/relaunch, keyboard focus, resize, local copy, and editor
return. Use only the repository's isolated SSH fixtures for SFTP, authentication,
and sudo checks. `herdr-app check` validates initial rendering, not the full
interactive lifecycle. Finder drops and platform-specific modifier handling
still require the [manual terminal checklist](../../docs/finder-drop-checklist.md).
