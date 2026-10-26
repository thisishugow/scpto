# Verification

Run the usual build, clippy, formatting, unit tests, and rustdoc checks from the
repository root. No new development dependencies are required.

## SSH integration

Start the fixtures as described in [their README](it/fixtures/README.md), then:

```sh
SCPTO_IT=1 cargo test --test it
```

Without `SCPTO_IT=1`, every integration scenario prints a skip message and returns.
Use `cargo test --test it -- --nocapture` to see those messages. Missing fixture
configuration is a failure when integration tests are enabled.

Each scenario runs in its own subprocess because production `Sessions` owns
process-scoped ControlMaster sockets. Tests can run concurrently without sharing
masters. Remote paths include the scenario and process ID; teardown removes only
registered test roots and closes the owned masters, including after assertions.
The suite terminates its own masters with `ssh -O exit`, never restarts shared
containers. All connection, transfer, tar, preview, edit, and sync work uses real
production APIs.

Both fixture versions cover independent browsing/transfer channels, CRUD,
reconnect, upload/download/relay with persisted partial records, every file
conflict choice, both tar conflict choices, 200-file archives in both directions,
symlinks, FIFO/socket skips, escaped Unicode names, sudo authorization failures
and successes, remote editing, previews, and selective synchronization.
Same-host copies require the modern server's copy-data capability and verify
permission masking; the legacy server exercises streaming instead.

On macOS, nonqualifying openrsync/rsync 2.x prints a skip message for rsync while
the engine sync path still runs. Linux CI must install rsync >= 3.1: lacking a
qualifying local or remote rsync fails the test instead of silently skipping it.

## Terminal driver

```sh
python3 tests/verify.py
SCPTO_IT=1 python3 tests/verify.py
```

Requires Python 3.8+, tmux, cargo, and ssh on PATH. The driver builds the binary,
creates its own tmux server, isolates HOME and XDG directories in a temporary
root, and creates an SSH Include config and wrapper. It never touches the
user's own SSH configuration. The optional VPN command and editor are temporary
interactive stubs; neither invokes a real VPN or user editor.

The local round verifies navigation, the parent row, sorting by menu/header,
filtering, hidden files, mkdir/rename/trash, text/Markdown previews, quick view,
help, keyboard copying, multi-selection, SGR mouse dragging, bracketed paste, responsive sizes,
and terminal restoration. The remote round verifies the host picker, content
round trips, conflicts, fixed eight-star sudo masking, master recovery, editor
key handoff, Ctrl+C in a remote shell, and VPN suspend/resume.

On macOS, NSFileManager needs permission to use the system trash; run outside a
filesystem sandbox that prohibits it. A trash failure is a test failure, not an
implicit permanent delete. Only the test-created temporary directory is trashed.
Failures print the current screen and stderr and exit nonzero.

## Benchmark

```sh
cargo test --release --test benchmark -- --ignored --nocapture
```

Creates 100,000 real files, measures LocalFs listing, Pane load and reverse sort,
and 20 full `render::draw` frames on a 160x45 TestBackend. Release builds require
listing < 3 seconds and the slowest frame < 16 milliseconds. Debug builds report
timings without enforcing release thresholds. Run without competing build or
filesystem workloads; file creation and teardown are outside the measurements.

## Known product regression: interrupted legacy upload

```sh
SCPTO_IT=1 cargo test --test it transfers::interrupted_legacy -- --ignored --exact --nocapture
```

This test is explicitly ignored with a `bug:` reason. It uploads a patterned
32 MiB file through the engine to OpenSSH 7.9, terminates its master after a
nonzero progress event, requires `WaitingForReconnect`, verifies a nonempty
`.scpto-part` and matching persisted record, then sends `EndpointReconnected`.

Expected: one completed item, exact original bytes, and no remaining partial or
record. Actual: the engine reports success and the final file has the correct
32 MiB size, but its contents differ. A repeat reproduced the first mismatch at
byte 61,440 with a partial size of 2,052,096. This suggests the retained prefix
cannot safely be assumed complete merely from its length; the source fix is
outside this task's ownership. The modern-server equivalent remains enabled and
passes. Pre-existing complete-prefix resume cases pass on both versions.
