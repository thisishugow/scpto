//! Persistent state: last directory per endpoint and bookmarks in
//! `$XDG_STATE_HOME/scpto/state.json` (default
//! `~/.local/state/scpto/state.json`), and resumable partial-file records in
//! `partials.json` next to it. Written atomically; the last writer wins.
