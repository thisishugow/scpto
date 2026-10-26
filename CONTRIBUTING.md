# Contributing

Keep scpto small, safe with existing SSH configuration, and usable with both the keyboard and mouse on macOS and Linux. Windows users run it under WSL.

## Development setup

Use the stable Rust toolchain with rustfmt and Clippy, an OpenSSH client, and Python 3.11 or newer for the PTY verification suite. Docker is needed for the optional integration fixtures. Work from the repository root:

```sh
cargo build
cargo run -- --left . --right ./src
```

Both panes in this example are local. Use disposable directories for file operations. Automated tests must use isolated home/config/state directories, SSH stubs, or the Docker fixtures; never use a real host or personal credentials.

## Checks

Run these before submitting a change:

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

Both `rg` commands must print nothing and exit 1. The other commands must exit 0. Do not suppress warnings or change assertions merely to get a passing run.

For integration coverage, start the [Docker fixtures](tests/it/fixtures/README.md), then enable the fixture tests and PTY round:

```sh
tests/it/fixtures/up.sh
SCPTO_IT=1 cargo test
SCPTO_IT=1 cargo test -- --ignored
SCPTO_IT=1 python3 tests/verify.py
tests/it/fixtures/down.sh
```

The fixtures generate their key, known-hosts file, and SSH config under `target/it`; they cover modern OpenSSH and OpenSSH 7.9, including sudo policy variants. Do not copy fixture configuration into your real `~/.ssh`. The fixture scripts recreate/stop their named containers, so coordinate with anyone sharing them.

For interface changes, check keyboard navigation, mouse selection and drag/drop, dialogs, narrow layouts, live resize, and terminal restoration after normal exit, errors, signals, and foreground commands. Use the [Finder-drop checklist](docs/finder-drop-checklist.md) for actual terminal drag/paste behavior; simulated input does not establish terminal support.

For documentation changes, verify every relative link, keep both READMEs aligned, and check the SVG:

```sh
rg -n '[\x{4e00}-\x{9fff}]' --glob '!README.zh-TW.md' .
python3 -c "import xml.dom.minidom,sys;xml.dom.minidom.parse('assets/scpto.svg')"
```

The language scan must exit 1 and the SVG parse must exit 0. Keep the SVG under 4 KB.

## Code and documentation

All code, comments, test data, documentation, and commit messages are English. The sole exception is [README.zh-TW.md](README.zh-TW.md), a faithful Traditional Chinese mirror of [README.md](README.md). Express non-ASCII test data with escapes; do not embed literal CJK names in source or fixtures.

Every public item needs a doc comment. Use crossterm only through `ratatui::crossterm`. Prefer existing dependencies and direct implementations; fix root causes without compatibility layers, dead code, duplicated helpers, or swallowed errors. Keep the letter-based interface free of function-key bindings. See [AGENTS.md](AGENTS.md) for the module map, frozen contracts, and security requirements.

Never commit personal SSH configuration, private keys, passwords, tokens, real host inventories, or screenshots containing them. Use fictional hosts and documentation IP ranges. Group rules are data; VPN commands run only after the user chooses Run.

## Changes and commits

Use a short, imperative English subject describing the behavior, following the repository's style: `Fix ...`, `Add ...`, or `Document ...`. Keep commits focused; explain the reason and material limitations in the body when needed. Describe validation in the change request and update both READMEs and the changelog when behavior changes.

For agent co-authorship, use `Co-Authored-By: Debussy <debussy@colosscious.com>`; do not use model names. In coordinated tasks, leave changes for the coordinator to commit and respect the assigned file scope.

Contributions are provided under the project's [MIT license](LICENSE).
