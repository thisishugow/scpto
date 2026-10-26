<p align="center">
  <img src="assets/scpto.svg" alt="scpto - dual-pane terminal file transfer" width="480">
</p>

[Traditional Chinese (zh-TW)](README.zh-TW.md)

A FileZilla replacement in the terminal for macOS and Linux. Browse local files on the left and a remote host on the right, then copy with the keyboard or drag between panes. Either pane can use any endpoint, including two local directories or two remote hosts.

## Features

- SFTP over your system `ssh`, using your existing SSH aliases and sshto groups.
- Keyboard navigation, multi-selection, mouse transfers, sorting, and a clickable button bar.
- A transfer queue with pause, retry, conflict handling, and resumable partial files.
- Remote editing, syntax-highlighted previews, Markdown, images, and one-way directory sync.
- Optional sudo browsing and transfers, automatic reconnect, and a configurable VPN hint.

## Requirements

An OpenSSH client on `PATH`, an SFTP server on the remote host, and a terminal with an interactive input/output pair. No remote agent is required. Remote shell startup files must not print to stdout for non-interactive commands: that output corrupts the SFTP stream.

## Installation

### Homebrew

```sh
brew install thisishugow/tap/scpto
```

Update with `brew upgrade thisishugow/tap/scpto`. The tap is [thisishugow/homebrew-tap](https://github.com/thisishugow/homebrew-tap).

### Release binaries

Download the matching archive from [Releases](https://github.com/thisishugow/scpto/releases), verify its checksum against the release checksums, extract it, and place `scpto` on your `PATH`, for example in `~/.local/bin`. Prebuilt binaries do not require Rust.

| Platform | Target |
| --- | --- |
| macOS, Intel | `x86_64-apple-darwin` |
| macOS, Apple silicon | `aarch64-apple-darwin` |
| Linux, Intel/AMD 64-bit | `x86_64-unknown-linux-musl` |
| Linux, ARM64 | `aarch64-unknown-linux-musl` |

Use `shasum -a 256 <archive>` on macOS or `sha256sum <archive>` on Linux to calculate the checksum. Windows is supported only through WSL, using a Linux binary and OpenSSH inside WSL.

### From source

From the source directory, with the stable Rust toolchain installed:

```sh
cargo install --locked --path .
```

Ensure `~/.cargo/bin` is on your `PATH`.

## Usage

```sh
scpto                                  # Local cwd and the remote host picker
scpto my-server                        # Right pane on this SSH host
scpto my-server:/srv/files              # Right pane at an absolute remote path
scpto --left ./incoming --right ./outgoing
scpto --left source:/srv/data --right destination:/srv/data
scpto --right '[2001:db8::1]:/srv/files'
scpto --help
scpto --version
```

The positional `host[:path]` always selects a remote endpoint and cannot be combined with `--right`. Each pane option accepts `--left value` or `--left=value` (likewise for `--right`), once per option.

For pane options, `host:/path` and `host:` are remote; a value without a colon is local. A slash before the first colon also makes a path local, so use `./a:b` for a local name containing a colon. Remote paths must be absolute; `host:` uses the remembered directory or remote home. Local paths must name existing directories and may be relative or start with `~/`. `--help`/`-h` and `--version`/`-V` must be used alone. Invalid CLI syntax exits with code 2.

## Key bindings

Actions use letters and ordinary navigation keys; no function keys are needed. Keys are case-sensitive. Operations use the marked entries, or the cursor entry when nothing is marked.

| Key | Action |
| --- | --- |
| Tab | Switch panes |
| Up / Down, `k` / `j` | Move the cursor |
| PgUp / PgDn, Home / End, `g` / `G` | Page, or jump to the first / last entry |
| Enter / Right / `l` | Enter a directory or preview a file |
| Backspace / Left / `h` | Go to the parent directory |
| Space / Insert | Toggle selection and move down |
| Ctrl+A / `*` | Select all / invert selection |
| `/` | Fuzzy filter; Enter keeps it, Esc clears it |
| `.` | Toggle hidden files |
| `s` | Sort menu: `n` name, `e` extension, `s` size, `m` modified, `r` reverse |
| Ctrl+R | Refresh the focused pane |
| `x` | Entry context menu |
| `c` / `m` | Copy / move to the other pane's directory |
| `n` / `r` | Create directory / rename |
| `d` / Delete | Delete, after confirmation |
| `e` / `p` | Edit / change permissions |
| `v` / `i` | Full-screen preview / toggle quick view |
| `R` | Reveal locally; download remote files and reveal |
| `o` / `S` | Host picker / reconnect the remote pane with sudo |
| `b` / `B` | Open bookmarks / bookmark this directory |
| `=` / `t` | Compare directories / open a shell here |
| `Q` / `?` | Queue panel / help |
| `q` / Ctrl+C | Quit; asks first while transfers are running |

The `..` row stays at the top outside `/`. Enter or double-click goes up; the row cannot be marked or transferred. Directories sort before files. Click a column header to sort by it, and click the active header again to reverse. Sorting preserves the cursor entry and is remembered per endpoint.

In dialogs, use Tab / Shift+Tab, Enter, Esc, and the displayed letter hotkeys. In the queue, Space or `p` pauses/resumes, `c` cancels, `r` retries, `d` discards a partial, `R` retries all failed items, and `C` clears finished jobs. Tar jobs cannot pause; rsync jobs can only be canceled while running.

Local deletion uses the system trash; if that fails, scpto offers permanent deletion. Remote deletion is permanent and the confirmation says so.

## Mouse and Finder

Click to focus a pane and move its cursor. Ctrl+click toggles individual selections; Shift+click selects a range. Double-click enters a directory or previews a file. Option/Alt+click opens the entry context menu, also available with `x`.

The endpoint button in each pane title opens its host picker. The bottom button bar runs the displayed actions on the focused pane; it adapts to the available width and changes with the active mode. Queue rows have their own controls.

Drag entries between panes to copy them into the destination directory, or onto a folder row to copy into that folder. Dragging onto a folder within the source pane moves entries. Esc cancels a drag. Quick view is not a drop target.

Finder drops arrive as bracketed paste containing shell-quoted or escaped absolute paths. scpto checks that every path exists and asks **Upload / Cancel** before copying. Focus the intended remote pane first: the destination is the focused remote pane, otherwise the other remote pane, otherwise the focused local pane. The external drop's pointer position does not choose the destination. A terminal must send bracketed paste for this to work; see the [manual drop checklist](docs/finder-drop-checklist.md).

Dragging out of a terminal into Finder is not supported. Press `R` on a remote file to download it to `~/Downloads` and reveal it in Finder. Existing names become `name (1).ext`, then the next free number. On Linux, `R` opens the containing directory with `xdg-open`; local files are revealed without copying.

## SSH and sudo

scpto discovers concrete aliases in `~/.ssh/config`, including supported `Include` globs with cycle detection and a nesting limit of 16. Wildcard and negated `Host` entries are not listed. Host details are resolved lazily with `ssh -G`; the picker can reuse sshto groups.

Connections use the system `ssh`, with one ControlMaster per host alias under `/tmp/scpto-<uid>/<pid>`. The directories are private (0700). Browse and transfer channels share that master. Background sessions disable configured remote commands, TTY requests, forwarding, and local commands so they cannot interfere with file transfers.

OpenSSH handles SSH passwords, passphrases, and host-key verification entirely. scpto never stores passwords. Interactive authentication suspends the TUI and returns the terminal to OpenSSH. Clean exit closes scpto's masters; after a crash, a master can remain for its `ControlPersist=10m` idle period.

Press `S` on a remote pane to reconnect with sudo. The remote machine needs an executable `sftp-server`, and sudoers must allow running `sh` as root. Rules allowing only `sftp-server` are insufficient. The password field is masked without exposing its length; secret buffers are zeroized, never stored or logged, and prompted again after reconnect when required. Sudo endpoints use per-file SFTP, without tar or rsync acceleration.

`requiretty` is unsupported. The message is:

```text
sudo on this host requires a terminal (requiretty), which an SFTP session cannot provide
```

## Transfers

Per-file copies write `<name>.scpto-part` beside the destination and rename it when complete. `partials.json` records the source size and modification time; resuming requires both to still match. Pause and cancel keep the partial. Retry the transfer to resume, or choose **Discard partial** in the queue to remove the file and its record. On SFTP servers without `posix-rename`, replacing an existing destination requires deleting it before renaming and is not atomic.

The conflict dialog offers **Overwrite**, **Skip**, **Rename**, **Overwrite if newer**, and **Resume** when a matching partial is available, with **Apply to all in this job**. Rename chooses a free `name (N).ext`. Name matching accounts for Unicode NFC equivalents and the destination filesystem's actual lookup behavior.

At the default threshold of 200 regular-file items after directory expansion, eligible local-to-remote or remote-to-local jobs use tar when the non-sudo remote has it. The queue labels this **tar (no resume)**. Tar requires UTF-8 command paths; downloads also require sources with a common parent. It has no `.scpto-part` protection, pause, or resume. Existing top-level destinations trigger an **Overwrite all / Cancel** dialog; interruption can leave partially extracted destination files.

Remote-to-remote copies stream through local memory. Symlinks are copied as links; special files such as sockets and devices are skipped. Cross-endpoint moves delete each source file only after its copy succeeds, and remove source directories only after all their items succeed.

## Remote editing

Press `e` to use `$VISUAL`, then `$EDITOR`, then `vi`. Editor commands accept quoted arguments. The TUI suspends while the editor runs. For GUI editors, use a wait flag:

```sh
VISUAL='code --wait' scpto my-server
```

Remote edits use a private, mode-0600 temporary file. Changed contents are uploaded after the editor exits successfully. scpto rechecks the remote size and modification time and asks before overwriting a file that changed meanwhile. An upload failure shows the retained temporary path for recovery or retry; recover it before quitting, because temporary edits are cleaned up on exit.

## Preview

`v` opens a full-screen preview; `i` shows a live quick view in the other pane. Text and code are syntax highlighted, Markdown is rendered (`m` toggles raw/rendered), and PNG, JPEG, GIF (first frame), and WebP images are supported. Scroll with arrows, paging keys, or the wheel; Esc or `q` closes the full viewer.

Text reads stop at 256 KiB and show a truncation marker. Binary files show metadata. Image previews are limited to 20 MiB. Image rendering depends on the terminal; inside tmux, scpto uses half-block characters. Remote previews use the browsing channel.

## Compare and sync

Press `=` to compare the two directory trees by normalized name, size, and modification time, without content hashing. Space selects entries; selecting a directory also selects its descendants. `>` syncs left to right and `<` right to left, after confirmation. Deleting selected destination-only entries is an explicit, unchecked option in the confirmation dialog.

rsync is used for sync only when one side is local, the other is non-sudo remote, and both have Samba rsync >= 3.1. macOS openrsync and rsync 2.x do not qualify. Other endpoint combinations use the SFTP/file transfer engine. This is selected, one-way sync, not background mirroring.

## Reconnect and VPN

Lost connections park affected transfers while scpto reconnects with exponential backoff, from 1 second up to 30 seconds by default. The pane shows the reconnect countdown. Authentication and host-key problems need OpenSSH interaction rather than automatic retries.

An unreachable host can offer a configured VPN command:

```text
vpn|work-*|ssh -t vpn-gateway connect-vpn
```

Rules use `vpn|<glob>|<cmd>`; the first case-insensitive alias or resolved-hostname match wins. The dialog displays the command and executes it with `/bin/sh -c` only when you choose **Run**, suspending the TUI. A successful command triggers an immediate reconnect. Commands may contain pipes after the first two separators.

## Configuration

Settings are read from nonempty `$SCPTO_CONFIG`, then `$XDG_CONFIG_HOME/scpto/config`, then `~/.config/scpto/config`. Relative XDG paths are ignored. Missing default files use defaults; invalid or unreadable settings, including a missing explicit file, produce a startup status warning and use defaults.

Use `set|key|value`, without surrounding spaces or quotes. Empty lines and full-line `#` comments are ignored; later assignments win. All accepted keys and defaults are:

| Key | Default | Meaning |
| --- | --- | --- |
| `concurrency` | `3` | Concurrent per-file items; integer >= 1 |
| `tar-threshold` | `200` | Regular-file count for tar eligibility; integer >= 1 |
| `show-hidden` | `false` | Initially show dotfiles; `true` or `false` |
| `mtime-tolerance` | `0` | Whole seconds treated as equal; integer >= 0; try `2` for FAT/exFAT |
| `reconnect-base-ms` | `1000` | Initial reconnect delay in milliseconds; integer >= 1 |
| `reconnect-cap-ms` | `30000` | Maximum delay; integer >= base |
| `keepalive-interval` | `15` | SSH keepalive seconds; integer >= 1 |

The last three keys are primarily timing controls for tests. SSH's keepalive failure count is fixed at 3.

```text
set|concurrency|3
set|tar-threshold|200
set|show-hidden|false
set|mtime-tolerance|0
```

### sshto groups

Group rules come from nonempty `$SSHTO_CONFIG`, then `$XDG_CONFIG_HOME/sshto/config`, then `~/.config/sshto/config`. They are parsed as data, never sourced or evaluated:

```text
group|Work
group|Personal
match|Work|alias|work-*
match|Personal|hostname|*.example.net
exclude|github.*
```

Declare groups before their rules. Match fields are `alias`, `hostname`, or `either`; ordered case-insensitive glob rules use the first match. `All` and `Other` are reserved; unmatched hosts appear in Other. `exclude` hides matching aliases from the picker. Hostname-based grouping can update as `ssh -G` results arrive. Missing default group files mean no custom groups; invalid or unreadable files show a warning and use that same default.

### State

State lives in `$XDG_STATE_HOME/scpto`, or `~/.local/state/scpto` when the variable is unset or not absolute. `state.json` stores bookmarks, last directories, and sorting; `partials.json` stores resume metadata. Concurrent instances use last-writer-wins state. A corrupt `state.json` is backed up as `state.json.bak` and reported before starting with empty state.

## Terminal notes

Ghostty, iTerm2, Terminal.app, and tmux need to pass mouse events and bracketed paste through to scpto. File-drop escaping and modifier handling vary by terminal and configuration; use the [manual checklist](docs/finder-drop-checklist.md) to verify your setup.

- Some Mac terminals need **Option as Meta/Alt** enabled for Option+click. `x` and the button bar provide keyboard and mouse alternatives.
- Terminal.app does not report Cmd; iTerm2 reserves Cmd+click for URLs. Use Ctrl+click or Space for selection. Mac keyboards without Insert can use Space.
- With mouse capture enabled, native text selection usually needs Option in Terminal.app or Shift in other terminals.
- Inside herdr/tmux, right-click belongs to the multiplexer. scpto ignores it.
- macOS TCC can block Desktop, Documents, Downloads, and other protected folders. Grant the terminal app **Files and Folders** access or **Full Disk Access** in System Settings, then restart the terminal app and multiplexers such as tmux/herdr for it to take effect.
- Below 60 columns, one pane is shown at a time; Tab or the pane tabs switches it. Copy/move still targets the other pane. While dragging, hover over its tab to switch, or drop on the tab to use that directory. Below 40 columns or 10 rows, enlarge the terminal to continue.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup and checks, [AGENTS.md](AGENTS.md) for module contracts, and [CHANGELOG.md](CHANGELOG.md) for release history.

## License

[MIT](LICENSE). Copyright (c) 2026 thisishugow.
