//! Persistent state under `$XDG_STATE_HOME/scpto` (a non-absolute
//! `$XDG_STATE_HOME` is ignored per the XDG spec; default
//! `~/.local/state/scpto`):
//!
//! - `state.json`: [`PersistedState`], the last directory per endpoint and
//!   the bookmarks.
//! - `partials.json`: [`Partials`], the source size and mtime recorded for
//!   each `.scpto-part` file (plan 7.5 S12).
//!
//! Both files are JSON written by `serde_json`. A write goes to a temp file
//! in the same directory (created with mode 0700 when missing) and is
//! renamed over the target, so readers never see a half-written file.
//! Concurrent scpto instances are not coordinated: the last writer wins.
//! A missing file loads as the empty value; an unreadable or unparsable file
//! is `Error::StateRead` (the caller reports it and continues with the empty
//! value, so the next save replaces the bad file). Failed writes are
//! `Error::StateWrite`.
//!
//! # Public API (Wave 2)
//!
//! ```text
//! /// `$XDG_STATE_HOME/scpto` when that value is absolute, else
//! /// `<home>/.local/state/scpto`; `None` when neither is usable (no
//! /// absolute XDG value and no home), in which case nothing is persisted
//! /// and startup says so in the status bar.
//! pub fn state_dir(xdg_state_home: Option<&OsStr>, home: Option<&Path>) -> Option<PathBuf>;
//!
//! /// Blocking file I/O; async callers use `spawn_blocking`.
//! pub fn load_state(dir: &Path) -> Result<PersistedState, Error>;
//! pub fn save_state(dir: &Path, state: &PersistedState) -> Result<(), Error>;
//! pub fn load_partials(dir: &Path) -> Result<Partials, Error>;
//! pub fn save_partials(dir: &Path, partials: &Partials) -> Result<(), Error>;
//!
//! impl PersistedState {
//!     /// Replaces the entry for `location.endpoint` (one per endpoint).
//!     pub fn remember_directory(&mut self, location: Location);
//!     pub fn last_directory(&self, endpoint: &Endpoint) -> Option<&Path>;
//!     /// Appends unless already present; returns whether it was added.
//!     pub fn add_bookmark(&mut self, location: Location) -> bool;
//!     /// Returns whether it was present.
//!     pub fn remove_bookmark(&mut self, location: &Location) -> bool;
//! }
//!
//! impl Partials {
//!     pub fn get(&self, destination: &Location) -> Option<PartialRecord>;
//!     /// Replaces any record for the same destination.
//!     pub fn insert(&mut self, destination: Location, record: PartialRecord);
//!     pub fn remove(&mut self, destination: &Location) -> Option<PartialRecord>;
//! }
//!
//! /// The `PartialStore` used by the product: `Partials` in memory behind a
//! /// mutex, saved with `save_partials` (in `spawn_blocking`) after every
//! /// change.
//! pub struct PartialsFile { /* private */ }
//!
//! impl PartialsFile {
//!     /// Loads `partials.json` from `dir`, or holds records in memory only
//!     /// when `dir` is `None`. A load failure is returned to the caller.
//!     pub fn open(dir: Option<PathBuf>) -> Result<PartialsFile, Error>;
//! }
//!
//! impl PartialStore for PartialsFile { /* ... */ }
//! ```
//!
//! `ui/effects/state.rs` keeps the loaded `PersistedState` in memory, applies
//! `RememberDirectory`, `SaveBookmark`, and `RemoveBookmark` to it, and saves
//! the whole value after each change.

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::transfer::model::PartialRecord;
use crate::vfs::Location;

/// The contents of `state.json`.
///
/// JSON objects only allow string keys, so per-endpoint data is stored as
/// lists of [`Location`]s rather than maps keyed by `Endpoint`. Unknown
/// fields are ignored and missing fields default, so older and newer
/// versions can read each other's files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedState {
    /// The last directory shown for each endpoint, at most one entry per
    /// endpoint (`Local`, each host, and each host with sudo are distinct).
    pub last_directories: Vec<Location>,
    /// Bookmarks in the order they were added, without names and without
    /// duplicates.
    pub bookmarks: Vec<Location>,
}

/// One record of `partials.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialEntry {
    /// The final destination (endpoint and path without `.scpto-part`).
    pub destination: Location,
    /// The source attributes when the partial was started.
    pub record: PartialRecord,
}

/// The contents of `partials.json`, at most one entry per destination.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Partials {
    /// The records.
    pub entries: Vec<PartialEntry>,
}

/// Access to the partial-file records for the transfer engine, which takes
/// an `Arc<dyn PartialStore>` so tests can substitute an in-memory store.
/// Methods that change records persist them before resolving.
pub trait PartialStore: Send + Sync {
    /// The record for a destination, if any.
    fn get(&self, destination: &Location) -> Option<PartialRecord>;

    /// Records the source attributes for a destination, replacing any
    /// earlier record, and saves.
    fn set(&self, destination: Location, record: PartialRecord)
        -> BoxFuture<'_, Result<(), Error>>;

    /// Forgets a destination's record (after completion or "Discard
    /// partial") and saves. Removing an absent record succeeds.
    fn remove<'a>(&'a self, destination: &'a Location) -> BoxFuture<'a, Result<(), Error>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::Endpoint;
    use std::sync::Arc;

    fn sudo_location() -> Location {
        Location {
            endpoint: Endpoint::Remote {
                host: "web".into(),
                sudo: true,
            },
            path: "/etc".into(),
        }
    }

    #[test]
    fn state_round_trips_through_json_with_remote_endpoints() {
        let state = PersistedState {
            last_directories: vec![
                sudo_location(),
                Location {
                    endpoint: Endpoint::Local,
                    path: "/Users/u".into(),
                },
            ],
            bookmarks: vec![sudo_location()],
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let back: PersistedState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, state);
    }

    #[test]
    fn missing_and_unknown_fields_are_tolerated() {
        let state: PersistedState =
            serde_json::from_str(r#"{"bookmarks": [], "future": 1}"#).expect("deserialize");
        assert_eq!(state, PersistedState::default());
    }

    #[test]
    fn partials_round_trip_through_json() {
        let partials = Partials {
            entries: vec![PartialEntry {
                destination: sudo_location(),
                record: PartialRecord {
                    source_size: 10,
                    source_mtime: -1,
                },
            }],
        };
        let json = serde_json::to_string(&partials).expect("serialize");
        let back: Partials = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, partials);
    }

    #[test]
    fn partial_store_is_dyn_compatible_and_shareable() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Arc<dyn PartialStore>>();
    }
}
