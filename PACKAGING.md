# Packaging and Homebrew

Repository: https://github.com/thisishugow/scpto

scpto ships MIT-licensed binaries for macOS and Linux. Windows users run the Linux build under WSL. Transfers require the system OpenSSH client; optional external tools are described in the README.

## Source archives

`tools/package.py` requires Python 3.11 or newer. It reads the version from `Cargo.toml` and collects only:

- `src/**/*.rs`, including nested Rust modules
- `Cargo.toml`, `Cargo.lock`
- `README.md`, `README.zh-TW.md`, `LICENSE`, `CHANGELOG.md`
- `assets/scpto.svg`

Every input must be a regular file; missing files, symlinks and special files fail packaging. An empty Rust source glob also fails. Archive entries are sorted under `scpto-<version>/`, with mode 0644, zero timestamps and ownership, and no original gzip filename or timestamp. Identical inputs and the same Python/zlib toolchain produce identical archives. No personal configuration or build outputs are collected. Dependencies are locked, but not vendored.

Build a source archive into a new directory:

```sh
python3 tools/package.py target/package-preview
```

The command writes `scpto-<version>.tar.gz`, `SHA256SUMS`, and `Formula/scpto.rb.in`. Without binary assets, the Formula retains checksum placeholders and is not installable. Extract the source archive and run `cargo build --release --locked` and `cargo test --no-run --locked` to verify it independently. The source archive is a build distribution; integration fixtures and development tooling are not included. Cargo discovers the `it` test target from `tests/it/main.rs` in a repository checkout; no explicit target declaration requires that file in the source archive.

## CI and binary releases

CI runs on pushes and pull requests. Separate jobs check formatting, Clippy with warnings denied, tests on Ubuntu and macOS, and Rust documentation with warnings denied. Ubuntu policy checks reject CJK source text, unfinished-code markers, `chafa` and `onig`, and multiple versions of `crossterm` or `ratatui`. A musl release build runs on every push and pull request.

The Ubuntu test jobs explicitly install `openssh-sftp-server` for unit tests. The integration job also installs rsync 3.x and tmux, starts both Docker SSH fixtures, and runs `cargo test --locked -- --include-ignored`, `cargo test --test it`, and `python3 tests/verify.py` with `SCPTO_IT=1` and `SCPTO_EXPECT_COPY_DATA=1`. Fixture teardown runs even after a failure.

Pushing a `v*` tag starts `.github/workflows/release.yml`:

| Target | Runner |
| --- | --- |
| `x86_64-unknown-linux-musl` | `ubuntu-latest` with `musl-tools` |
| `aarch64-unknown-linux-musl` | `ubuntu-24.04-arm` with `musl-tools` |
| `x86_64-apple-darwin` | `macos-latest` |
| `aarch64-apple-darwin` | `macos-latest` |

Each build verifies the tag matches `v` plus the Cargo version, installs the target, and builds with `--release --locked`. Each deterministic `scpto-<version>-<target>.tar.gz` contains `scpto` (mode 0755), `README.md`, `README.zh-TW.md`, `LICENSE`, and `CHANGELOG.md` (mode 0644).

Assembly requires exactly the four expected binary archive names, builds the source archive, and emits `SHA256SUMS` covering all five archives. It renders `scpto.rb` with checksums from the actual binary assets, checks Ruby syntax, and creates a draft GitHub release containing the five archives, checksums and Formula. Only assembly has `contents: write`; all other jobs have read access. Checkouts do not persist credentials. The workflow never pushes to the tap.

To reproduce assembly locally, put only the four binary archives in `target/release-binaries` and run:

```sh
python3 tools/package.py target/source-release --binary-dir target/release-binaries
ruby -c target/source-release/Formula/scpto.rb
```

The output directory must not already exist. Its checksum file covers the source archive in that directory and the four archives in the binary directory; collect all five in one directory before running `shasum -a 256 -c SHA256SUMS`.

## Cut a release

1. Update `Cargo.toml`, refresh `Cargo.lock` with `cargo check`, and update `CHANGELOG.md` and both READMEs as needed.
2. Run `cargo build`, `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked`, and `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked`. Run the Docker integration checks above and confirm CI passes.
3. Build and inspect a source archive, then verify an extracted copy builds with `cargo build --release --locked`.
4. Commit the reviewed release changes through the normal project process, then tag and push through the existing remote:

   ```sh
   git tag v0.1.0
   git push origin v0.1.0
   ```

5. Confirm the workflow succeeds. Download the draft release assets, verify `SHA256SUMS`, and smoke-test `--version` and `--help` on the supported platforms. Review the generated release notes and publish the draft.
6. Update the tap manually using the Formula attached to that exact release.

## Manual tap update

The tap is [thisishugow/homebrew-tap](https://github.com/thisishugow/homebrew-tap). Download the published `scpto.rb` asset and copy it to `Formula/scpto.rb` in your existing tap checkout. Review all four URLs, the version and binary checksums against the published `SHA256SUMS`; do not use checksums from an unrelated rebuild.

The Formula selects the macOS or Linux binary for Intel or ARM, installs documentation and the MIT license, and requires no Rust build toolchain. Its test verifies the version and the `SCPTO_CONFIG` help entry. Linux support preserves the sshto tap's Linuxbrew support using the musl assets.

Commit and publish the Formula through the tap's normal review process, then verify installation:

```sh
brew install thisishugow/tap/scpto
brew test thisishugow/tap/scpto
```

For an existing installation, run `brew update` and `brew upgrade thisishugow/tap/scpto`, then repeat `brew test`. No release automation changes the tap or the user's SSH configuration.
