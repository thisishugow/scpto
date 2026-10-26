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
//! # Public API
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

use std::ffi::OsStr;
use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use futures_util::future::BoxFuture;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::transfer::model::PartialRecord;
use crate::vfs::{Endpoint, Location};

/// Name of the per-user directory below the XDG state home.
const APP_DIR: &str = "scpto";
/// File name of [`PersistedState`].
const STATE_FILE: &str = "state.json";
/// File name of [`Partials`].
const PARTIALS_FILE: &str = "partials.json";

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

impl PersistedState {
    /// Replaces the entry for `location.endpoint` (one per endpoint).
    pub fn remember_directory(&mut self, location: Location) {
        match self
            .last_directories
            .iter_mut()
            .find(|entry| entry.endpoint == location.endpoint)
        {
            Some(entry) => entry.path = location.path,
            None => self.last_directories.push(location),
        }
    }

    /// The last remembered directory of an endpoint.
    pub fn last_directory(&self, endpoint: &Endpoint) -> Option<&Path> {
        self.last_directories
            .iter()
            .find(|entry| entry.endpoint == *endpoint)
            .map(|entry| entry.path.as_path())
    }

    /// Appends unless already present; returns whether it was added.
    pub fn add_bookmark(&mut self, location: Location) -> bool {
        if self.bookmarks.contains(&location) {
            return false;
        }
        self.bookmarks.push(location);
        true
    }

    /// Removes a bookmark; returns whether it was present.
    pub fn remove_bookmark(&mut self, location: &Location) -> bool {
        let before = self.bookmarks.len();
        self.bookmarks.retain(|entry| entry != location);
        self.bookmarks.len() != before
    }
}

impl Partials {
    /// The record for a destination, if any.
    pub fn get(&self, destination: &Location) -> Option<PartialRecord> {
        self.entries
            .iter()
            .find(|entry| entry.destination == *destination)
            .map(|entry| entry.record)
    }

    /// Replaces any record for the same destination.
    pub fn insert(&mut self, destination: Location, record: PartialRecord) {
        match self
            .entries
            .iter_mut()
            .find(|entry| entry.destination == destination)
        {
            Some(entry) => entry.record = record,
            None => self.entries.push(PartialEntry {
                destination,
                record,
            }),
        }
    }

    /// Removes and returns the record for a destination.
    pub fn remove(&mut self, destination: &Location) -> Option<PartialRecord> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.destination == *destination)?;
        Some(self.entries.remove(index).record)
    }
}

/// `$XDG_STATE_HOME/scpto` when that value is absolute, else
/// `<home>/.local/state/scpto`; `None` when neither is usable.
pub fn state_dir(xdg_state_home: Option<&OsStr>, home: Option<&Path>) -> Option<PathBuf> {
    let base = match xdg_state_home.map(Path::new) {
        Some(xdg) if xdg.is_absolute() => xdg.to_path_buf(),
        _ => home?.join(".local").join("state"),
    };
    Some(base.join(APP_DIR))
}

/// Loads `state.json`; a missing file gives the empty state. Blocking.
pub fn load_state(dir: &Path) -> Result<PersistedState, Error> {
    load_json(&dir.join(STATE_FILE))
}

/// Saves `state.json` atomically, creating `dir` (mode 0700) when missing.
/// Blocking.
pub fn save_state(dir: &Path, state: &PersistedState) -> Result<(), Error> {
    save_json(&dir.join(STATE_FILE), state)
}

/// Loads `partials.json`; a missing file gives no records. Blocking.
pub fn load_partials(dir: &Path) -> Result<Partials, Error> {
    load_json(&dir.join(PARTIALS_FILE))
}

/// Saves `partials.json` atomically, creating `dir` (mode 0700) when
/// missing. Blocking.
pub fn save_partials(dir: &Path, partials: &Partials) -> Result<(), Error> {
    save_json(&dir.join(PARTIALS_FILE), partials)
}

fn load_json<T: DeserializeOwned + Default>(path: &Path) -> Result<T, Error> {
    let read_error = |message: String| Error::StateRead {
        path: path.to_path_buf(),
        message,
    };
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(T::default()),
        Err(error) => return Err(read_error(error.to_string())),
    };
    serde_json::from_slice(&bytes).map_err(|error| read_error(error.to_string()))
}

fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<(), Error> {
    let write_error = |message: String| Error::StateWrite {
        path: path.to_path_buf(),
        message,
    };
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .map_err(|error| write_error(error.to_string()))?;
    let json = serde_json::to_vec_pretty(value).map_err(|error| write_error(error.to_string()))?;

    // Unique per process and call, so concurrent writers never share a temp.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let written = fs::File::create(&temp).and_then(|mut file| {
        file.write_all(&json)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    });
    written.map_err(|error| {
        // The write error is the one worth reporting; a leftover temp file
        // that cannot be removed adds nothing the user can act on.
        let _cleanup = fs::remove_file(&temp);
        write_error(error.to_string())
    })
}

/// The [`PartialStore`] used by the product: [`Partials`] in memory behind a
/// mutex, saved with [`save_partials`] (in `spawn_blocking`) after every
/// change.
pub struct PartialsFile {
    dir: Option<PathBuf>,
    partials: Mutex<Partials>,
    /// Serializes change-then-save so an older snapshot never overwrites a
    /// newer one on disk.
    writer: tokio::sync::Mutex<()>,
}

impl PartialsFile {
    /// Loads `partials.json` from `dir`, or holds records in memory only
    /// when `dir` is `None`. A load failure is returned to the caller.
    pub fn open(dir: Option<PathBuf>) -> Result<PartialsFile, Error> {
        let partials = match &dir {
            Some(dir) => load_partials(dir)?,
            None => Partials::default(),
        };
        Ok(PartialsFile {
            dir,
            partials: Mutex::new(partials),
            writer: tokio::sync::Mutex::new(()),
        })
    }

    fn records(&self) -> MutexGuard<'_, Partials> {
        // The records stay consistent even if a holder panicked.
        self.partials
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Applies `change`, then saves the result.
    async fn change(&self, change: impl FnOnce(&mut Partials)) -> Result<(), Error> {
        let _writer = self.writer.lock().await;
        let snapshot = {
            let mut records = self.records();
            change(&mut records);
            records.clone()
        };
        let Some(dir) = self.dir.clone() else {
            return Ok(());
        };
        let path = dir.join(PARTIALS_FILE);
        tokio::task::spawn_blocking(move || save_partials(&dir, &snapshot))
            .await
            .map_err(|error| Error::StateWrite {
                path,
                message: error.to_string(),
            })?
    }
}

impl PartialStore for PartialsFile {
    fn get(&self, destination: &Location) -> Option<PartialRecord> {
        self.records().get(destination)
    }

    fn set(
        &self,
        destination: Location,
        record: PartialRecord,
    ) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(self.change(move |records| records.insert(destination, record)))
    }

    fn remove<'a>(&'a self, destination: &'a Location) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(self.change(move |records| {
            records.remove(destination);
        }))
    }
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

    fn local(path: &str) -> Location {
        Location {
            endpoint: Endpoint::Local,
            path: path.into(),
        }
    }

    fn record(size: u64) -> PartialRecord {
        PartialRecord {
            source_size: size,
            source_mtime: 5,
        }
    }

    #[test]
    fn state_dir_prefers_absolute_xdg_then_home() {
        let home = Path::new("/home/u");
        assert_eq!(
            state_dir(Some(OsStr::new("/x")), Some(home)),
            Some(PathBuf::from("/x/scpto"))
        );
        for ignored in [None, Some(OsStr::new("")), Some(OsStr::new("rel"))] {
            assert_eq!(
                state_dir(ignored, Some(home)),
                Some(PathBuf::from("/home/u/.local/state/scpto"))
            );
            assert_eq!(state_dir(ignored, None), None);
        }
        assert_eq!(
            state_dir(Some(OsStr::new("/x")), None),
            Some(PathBuf::from("/x/scpto"))
        );
    }

    #[test]
    fn directories_are_one_per_endpoint() {
        let mut state = PersistedState::default();
        state.remember_directory(local("/a"));
        state.remember_directory(sudo_location());
        state.remember_directory(local("/b"));
        assert_eq!(state.last_directories.len(), 2);
        assert_eq!(
            state.last_directory(&Endpoint::Local),
            Some(Path::new("/b"))
        );
        assert_eq!(
            state.last_directory(&sudo_location().endpoint),
            Some(Path::new("/etc"))
        );
        let plain = Endpoint::Remote {
            host: "web".into(),
            sudo: false,
        };
        assert_eq!(state.last_directory(&plain), None);
    }

    #[test]
    fn bookmarks_dedupe_and_remove() {
        let mut state = PersistedState::default();
        assert!(state.add_bookmark(local("/a")));
        assert!(!state.add_bookmark(local("/a")));
        assert!(state.add_bookmark(local("/b")));
        assert!(state.remove_bookmark(&local("/a")));
        assert!(!state.remove_bookmark(&local("/a")));
        assert_eq!(state.bookmarks, vec![local("/b")]);
    }

    #[test]
    fn partials_insert_replaces_and_remove_returns_the_record() {
        let mut partials = Partials::default();
        partials.insert(local("/a"), record(1));
        partials.insert(local("/a"), record(2));
        assert_eq!(partials.entries.len(), 1);
        assert_eq!(partials.get(&local("/a")), Some(record(2)));
        assert_eq!(partials.remove(&local("/a")), Some(record(2)));
        assert_eq!(partials.remove(&local("/a")), None);
        assert_eq!(partials.get(&local("/a")), None);
    }

    #[test]
    fn missing_files_load_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            load_state(dir.path()).expect("state"),
            PersistedState::default()
        );
        assert_eq!(
            load_partials(dir.path()).expect("partials"),
            Partials::default()
        );
    }

    #[test]
    fn save_creates_a_private_directory_and_round_trips() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("a").join("scpto");
        let mut state = PersistedState::default();
        state.remember_directory(sudo_location());
        state.add_bookmark(local("/b"));
        save_state(&dir, &state).expect("save");
        assert_eq!(load_state(&dir).expect("load"), state);
        let mode = fs::metadata(&dir).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o700);

        let mut partials = Partials::default();
        partials.insert(local("/p"), record(9));
        save_partials(&dir, &partials).expect("save");
        assert_eq!(load_partials(&dir).expect("load"), partials);

        // No temp files are left behind.
        let mut names: Vec<_> = fs::read_dir(&dir)
            .expect("read_dir")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["partials.json", "state.json"]);
    }

    #[test]
    fn corrupt_file_is_a_read_error_and_the_next_save_replaces_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        fs::write(&path, "{not json").expect("write");
        assert!(matches!(
            load_state(dir.path()),
            Err(Error::StateRead { path: reported, .. }) if reported == path
        ));
        save_state(dir.path(), &PersistedState::default()).expect("save");
        assert_eq!(
            load_state(dir.path()).expect("load"),
            PersistedState::default()
        );
    }

    #[test]
    fn save_into_a_file_instead_of_a_directory_is_a_write_error() {
        let root = tempfile::tempdir().expect("tempdir");
        let blocker = root.path().join("blocker");
        fs::write(&blocker, "x").expect("write");
        assert!(matches!(
            save_state(&blocker, &PersistedState::default()),
            Err(Error::StateWrite { .. })
        ));
    }

    #[tokio::test]
    async fn partials_file_persists_every_change_and_reopens() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = PartialsFile::open(Some(dir.path().to_path_buf())).expect("open");
        store.set(local("/a"), record(1)).await.expect("set");
        store.set(local("/b"), record(2)).await.expect("set");
        store.remove(&local("/a")).await.expect("remove");
        store
            .remove(&local("/missing"))
            .await
            .expect("remove absent");
        assert_eq!(store.get(&local("/b")), Some(record(2)));

        let reopened = PartialsFile::open(Some(dir.path().to_path_buf())).expect("reopen");
        assert_eq!(reopened.get(&local("/a")), None);
        assert_eq!(reopened.get(&local("/b")), Some(record(2)));
    }

    #[tokio::test]
    async fn partials_file_without_a_directory_is_memory_only() {
        let store = PartialsFile::open(None).expect("open");
        store.set(local("/a"), record(1)).await.expect("set");
        assert_eq!(store.get(&local("/a")), Some(record(1)));
    }

    #[test]
    fn partials_file_open_reports_a_corrupt_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("partials.json"), "[").expect("write");
        assert!(matches!(
            PartialsFile::open(Some(dir.path().to_path_buf())),
            Err(Error::StateRead { .. })
        ));
    }
}
