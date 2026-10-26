# Finder-drop manual checklist

Verify each terminal's real drag-to-paste behavior with scpto running. This is a manual test plan, not a compatibility claim. All cells below start unverified. Record terminal/OS/multiplexer versions, relevant mouse/paste settings, and the exact result for every checked cell.

## Prepare disposable files

Run from the repository root on macOS. This creates only temporary test data; escaped Unicode keeps this document and fixture code English-only.

```sh
probe_dir=$(mktemp -d "${TMPDIR:-/tmp}/scpto-drop.XXXXXX")
python3 - "$probe_dir" <<'PY'
from pathlib import Path
import sys

root = Path(sys.argv[1])
source = root / 'source'
source.mkdir()
(root / 'destination').mkdir()
(root / 'home').mkdir()
(root / 'config').mkdir()
(root / 'state').mkdir()
names = [
    'plain.txt',
    'two words.txt',
    'single\' and "double".txt',
    '\u6e2c\u8a66.txt',
    'cafe\u0301.txt',
    'second.txt',
]
for index, name in enumerate(names):
    (source / name).write_bytes(f'drop fixture {index}\n'.encode('ascii'))
print(source)
PY
cargo build
open "$probe_dir/source"
env HOME="$probe_dir/home" XDG_CONFIG_HOME="$probe_dir/config" \
  XDG_STATE_HOME="$probe_dir/state" SCPTO_CONFIG= SSHTO_CONFIG= \
  ./target/debug/scpto --left "$probe_dir/source" --right "$probe_dir/destination"
```

For the local probe, focus the right pane before each drop. Both panes are local, so the upload confirmation targets that focused local directory. For upload verification, repeat with a disposable directory on an isolated Docker fixture endpoint from [the fixture setup](../tests/it/fixtures/README.md). Use a test-only SSH configuration and isolated config/state; do not modify personal SSH files or test against real hosts.

The paste probe is the application itself: keep scpto in browse mode, focus the intended destination, and drag from Finder onto that pane. Do not test in a shell prompt, filter field, password field, or editor. The drop must become one bracketed-paste path list, not navigation keystrokes. If a terminal delivers ordinary keystrokes or unsupported escaping, record that failure; do not mark it supported because manually typing `c` works.

## Cases and expected results

| ID | What to drag from Finder | Expected result after Upload |
| --- | --- | --- |
| P | `plain.txt` | One file with that exact name and matching bytes |
| S | `two words.txt` | One file; the space does not split the path |
| Q | The file containing both single and double quotes | One file with both literal quotes preserved; no path-parse error |
| C | The file named by `\u6e2c\u8a66.txt` in the generator | One file; Unicode characters remain intact with matching bytes |
| N | `cafe\u0301.txt` from the generator (NFD: `e` plus U+0301) | One file with the decomposed name intact where the filesystem preserves it; no duplicate or missing entry from normalization |
| M | Select all six generated files and drag together | One confirmation for six items; six separate files with matching names and bytes |

Before Upload, every case must show the expected item count and destination. First choose Cancel and confirm nothing was copied; repeat and choose Upload. Use a fresh destination for each case so conflict dialogs do not mask parsing failures. Inspect names by Unicode code points or escaped representation when checking C/N; visual appearance alone cannot distinguish normalization.

## Terminal matrix

Each cell references the case above and is a separate check. Mark `[x]` only after both Cancel and Upload results pass; record failures or unavailable environments below. Repeat the same case expectations directly and through each multiplexer.

| Terminal / session | Plain | Spaces | Quotes | CJK | NFD-decomposed names | Multiple files |
| --- | --- | --- | --- | --- | --- | --- |
| Ghostty | [ ] P | [ ] S | [ ] Q | [ ] C | [ ] N | [ ] M |
| iTerm2 | [ ] P | [ ] S | [ ] Q | [ ] C | [ ] N | [ ] M |
| Terminal.app | [ ] P | [ ] S | [ ] Q | [ ] C | [ ] N | [ ] M |
| WezTerm | [ ] P | [ ] S | [ ] Q | [ ] C | [ ] N | [ ] M |
| kitty | [ ] P | [ ] S | [ ] Q | [ ] C | [ ] N | [ ] M |
| tmux inside Ghostty | [ ] P | [ ] S | [ ] Q | [ ] C | [ ] N | [ ] M |
| tmux inside iTerm2 | [ ] P | [ ] S | [ ] Q | [ ] C | [ ] N | [ ] M |
| tmux inside Terminal.app | [ ] P | [ ] S | [ ] Q | [ ] C | [ ] N | [ ] M |
| tmux inside WezTerm | [ ] P | [ ] S | [ ] Q | [ ] C | [ ] N | [ ] M |
| tmux inside kitty | [ ] P | [ ] S | [ ] Q | [ ] C | [ ] N | [ ] M |
| herdr (record outer terminal) | [ ] P | [ ] S | [ ] Q | [ ] C | [ ] N | [ ] M |

## Destination and terminal checks

- [ ] With a remote pane focused, the prompt names that remote directory.
- [ ] With the local pane focused and the other pane remote, the prompt names the remote directory. Pointer location does not override this rule.
- [ ] With both panes remote, focus determines the destination.
- [ ] A nonexistent absolute path is rejected with a status message and no transfer.
- [ ] Esc/Cancel leaves the destination unchanged; confirming copies bytes without executing pasted text as a command.
- [ ] Mouse reporting and bracketed paste survive entry to and exit from tmux/herdr. Right-click remains owned by the multiplexer.
- [ ] Option/Alt+click opens the context menu when the terminal sends the modifier; otherwise `x` works. Record any Option-as-Meta setting.
- [ ] For protected macOS folders, the terminal has the necessary TCC permission and was restarted along with tmux/herdr after the grant.
- [ ] `q` restores the terminal: normal shell typing and paste work, with no leaked mouse events.

## Result record

Copy one record per tested environment; keep filenames escaped and redact personal paths.

```text
Terminal and version:
OS version:
Multiplexer/version and outer terminal:
Mouse/paste/Option settings:
Local probe cases passed:
Fixture upload cases passed:
Failed case, observed result, and expected result:
Escaped pasted path representation, if captured safely:
Notes:
```

No matrix results have been recorded in this checklist. See the [README](../README.md) for the supported input model and destination rules.
