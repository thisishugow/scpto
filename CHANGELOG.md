# Changelog

All notable changes are recorded here. This file follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-10-26

### Added

- Dual-pane terminal file management on macOS and Linux, with local, remote, and remote-to-remote endpoints. Windows users can run under WSL.
- SFTP over system OpenSSH, per-host ControlMaster connections, SSH Include discovery, sshto groups, interactive authentication, reconnect, and optional VPN commands.
- Keyboard and mouse navigation, explicit parent rows, multi-selection, sorting, context menus, a host title button, clickable action bars, and responsive layouts.
- Copy, move, rename, mkdir, chmod, trash/permanent-delete confirmation, bookmarks, and shells in the current directory.
- Transfer queue controls, conflict choices, metadata-checked partial-file resume, and tar acceleration for eligible batches of at least 200 files by default.
- Sudo file access with masked, zeroized secrets and explicit policy errors.
- External-editor integration with remote change checks and recovery paths for failed uploads.
- Syntax-highlighted text, rendered/raw Markdown, image preview, and quick view.
- Directory comparison and selected one-way sync, using rsync >= 3.1 when eligible and the transfer engine otherwise.
- Bracketed-paste file uploads and remote download-and-reveal to `~/Downloads`.
- XDG settings/state, configurable concurrency and timestamp tolerance, and persistent bookmarks/directories/sorts.
- English and Traditional Chinese READMEs, contributor and coding-agent guides, an SVG icon, a manual Finder-drop checklist, and the MIT license.

[0.1.0]: https://github.com/thisishugow/scpto/releases/tag/v0.1.0
