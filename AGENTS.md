# AGENTS.md

`scpto` is a Rust terminal file manager and FileZilla replacement for macOS and Linux. Either pane can browse a local or remote endpoint. Remote transfers use SFTP over the system OpenSSH client. Windows is out of scope; use WSL.

## Modules

| Path | Responsibility |
| --- | --- |
| `src/main.rs`, `src/lib.rs`, `src/startup.rs` | Thin binary, library entry point, startup and dependency wiring |
| `src/cli.rs` | CLI syntax and endpoint parsing |
| `src/settings.rs`, `src/groups.rs`, `src/pattern.rs` | Settings, sshto groups, and glob matching |
| `src/hosts.rs` | SSH alias discovery, Include traversal, lazy host resolution |
| `src/state.rs` | Bookmarks, last directories/sorts, and partial-file metadata |
| `src/error.rs` | Shared error classification and user-facing messages |
| `src/connections.rs` | Connected-endpoint abstraction, tool versions, shell quoting |
| `src/session.rs`, `src/sudo.rs` | ControlMaster lifecycle, SFTP channels, reconnect, root sftp-server startup |
| `src/vfs/` | `Fs` contract, local filesystem, and SFTP backend |
| `src/transfer/model.rs` | Queue types, commands, and events |
| `src/transfer/engine.rs`, `stream.rs`, `tar.rs` | Scheduling/conflicts/resume, per-file copy, tar acceleration (all under `src/transfer/`) |
| `src/sync.rs`, `src/edit.rs`, `src/preview.rs` | Compare/sync, editor lifecycle, preview loading and formatting |
| `src/term.rs` | Terminal guard, restoration, signals, foreground handoff |
| `src/ui/action.rs`, `pane.rs`, `hit.rs`, `input.rs` | Actions/effects/events, pane state, hit map, input mapping (all under `src/ui/`) |
| `src/ui/app/` | Pure app state and feature reducers |
| `src/ui/effects/` | Filesystem, session, transfer, editor, preview, sync, external-command, state, and timer I/O |
| `src/ui/widgets/` | Individual widgets and shared formatting |
| `src/ui/render.rs`, `src/ui/runtime.rs` | Frame composition and event loop |

Read each affected module's `//!` contract and Public API section before editing. App state emits effects; executors perform I/O; rendering builds the hit map consumed by input.

## Shared contracts

Treat these as shared contracts and change them deliberately, in a focused commit that updates every caller and its tests: `Cargo.toml`, `src/main.rs`, `src/lib.rs` module declarations, any `mod.rs`, the error enums in `src/error.rs`, the `Fs`/entry/endpoint types in `src/vfs/mod.rs`, `src/transfer/model.rs`, `src/ui/action.rs`, `src/connections.rs`, the key list in `src/settings.rs`, the `HitMap`, the `Pane` API, the persisted-state/partials API, and the widget view structs.

Keep feature logic in its existing app/effect/widget files; `startup.rs`, `ui/runtime.rs`, and `ui/render.rs` only wire features together. Keep changes scoped to the task and preserve unrelated work.

## Checks

```sh
cargo build
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo test
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
rg -n '[\x{4e00}-\x{9fff}]' src tests Cargo.toml
rg -n 'TODO|FIXME|todo!|unimplemented!|allow\(dead_code\)|allow\(unused' src tests
python3 tests/verify.py
```

The two scans pass only with no matches (exit 1); all other checks must exit 0. For fixture coverage, run `tests/it/fixtures/up.sh`, `SCPTO_IT=1 cargo test`, `SCPTO_IT=1 cargo test -- --ignored`, and `SCPTO_IT=1 python3 tests/verify.py`. See [CONTRIBUTING.md](CONTRIBUTING.md) for setup and manual checks. Report failures honestly; never alter tests just to make them pass.

## Language and implementation

- Write every file and commit message in English. Only `README.zh-TW.md` uses Traditional Chinese, mirroring `README.md`. Use escapes for non-ASCII test data.
- Document every public item; preserve `#![warn(missing_docs)]` and warning-free builds/docs.
- Use crossterm only through `ratatui::crossterm`.
- Use letter keys, not function keys. Keep help, input, button bars, and documentation aligned. Right-click is ignored; Option/Alt+click and `x` open context menus.
- Fix root causes. No dead code, duplicated implementations, warning suppression, unfinished markers, swallowed errors, or speculative compatibility paths. Prefer existing dependencies.
- Keep `Cargo.toml` as the version source. Read the actual code when documenting behavior. Query the CLI clock before adding a date.
- Use imperative English commit subjects. Agent co-authorship is `Debussy <debussy@colosscious.com>`.

## Security and lifecycle

- Never store or log SSH passwords, passphrases, or sudo secrets. OpenSSH owns SSH authentication and host-key handling; interactive prompts run with the TUI suspended.
- Sudo uses the fixed root `sh` wrapper and marker protocol. Try without a password first; write one password line only after a password-required result, then wait for the exact success marker before sending SFTP bytes. Never retry passwords on the same child.
- Keep sudo input in fixed-capacity `Zeroizing<String>` and the write buffer in zeroizing storage. Mask without revealing length, reject oversized or line-breaking input, redact diagnostics, and re-prompt after reconnect. Never retain a password to open another channel.
- Use `connections::quote` / `connections::join_command` for every remote shell command. They escape single quotes and reject newline-containing words. Do not create a second quoting helper. rsync `-e` uses its own parser and the dedicated encoding in `sync.rs`.
- Keep ControlMaster directories private and ownership-checked. Close only this process's masters. Background children use separate process groups and managed lifetimes; foreground editor/shell/VPN/auth uses `TerminalGuard::run_foreground`.
- Restore terminal state on normal exit, errors, signals, panics, and foreground handoff. Never let an input reader steal foreground keystrokes.
- Create partial and editor files with restrictive permissions before writing data. Preserve failed edits for recovery while the app runs. Check resume source metadata and remote-edit conflicts before writing.
- Parse group/settings files as data. Execute a configured VPN command only after Run is chosen. Never modify personal SSH/config files as a test.
- Tests use isolated directories, fixture credentials, stubs, and fictional hosts. Never commit real inventories or secrets. Tar extraction rejects absolute/traversing paths; symlinks are not recursively followed during transfer expansion.
