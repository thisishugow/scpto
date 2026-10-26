//! Persistent state under `$XDG_STATE_HOME/scpto` (a non-absolute
//! `$XDG_STATE_HOME` is ignored per the XDG spec; default
//! `~/.local/state/scpto`):
//!
//! - `state.json`: [`PersistedState`], the last directory and the last
//!   pane sort per endpoint, and the bookmarks.
//! - `partials.json`: [`Partials`], the source size and mtime recorded for
//!   each `.scpto-part` file (plan 7.5 S12).
//!
//! Both files are JSON written by `serde_json`. A write goes to a temp file
//! in the same directory (created with mode 0700 when missing) and is
//! renamed over the target, so readers never see a half-written file.
//! Concurrent scpto instances are not coordinated: the last writer wins.
//! A missing file loads as the empty value. `state.json` never stops
//! startup (plan 2.6): an unparsable file is renamed to `state.json.bak`
//! (replacing an older backup) and loads as the empty state with the
//! warning `Error::StateCorrupt`, so the user's bookmarks can still be
//! recovered by hand; an unreadable file loads as the empty state with
//! `Error::StateRead`. The caller shows the warning, and the next save
//! writes a fresh file. An unreadable or unparsable `partials.json` is
//! `Error::StateRead`. Failed writes are `Error::StateWrite`.
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
//! /// Never fails: the state to use plus a warning to show, if any.
//! pub fn load_state(dir: &Path) -> (PersistedState, Option<Error>);
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
//!     /// Replaces the sort remembered for `endpoint` (one per endpoint).
//!     pub fn remember_sort(&mut self, endpoint: Endpoint, sort: Sort);
//!     pub fn last_sort(&self, endpoint: &Endpoint) -> Option<Sort>;
//! }
//!
//! impl Sort {
//!     /// `key` in its natural order: names and extensions A first, sizes
//!     /// and times largest and newest first.
//!     pub fn by(key: SortKey) -> Sort;
//!     /// The same key in the other order.
//!     pub fn reversed(self) -> Sort;
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
//! `RememberDirectory`, `RememberSort`, `SaveBookmark`, and `RemoveBookmark`
//! to it, and saves the whole value after each change.

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
/// Name a corrupt [`STATE_FILE`] is moved to.
const STATE_BACKUP_FILE: &str = "state.json.bak";

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
    /// The last sort a pane used for each endpoint, at most one entry per
    /// endpoint.
    pub sorts: Vec<EndpointSort>,
}

/// What a pane sorts by (plan 8.9). Directories always come first,
/// whatever the key and order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortKey {
    /// File name.
    Name,
    /// File name extension (the text after the last `.`; none for
    /// dotfiles such as `.bashrc`), then name.
    Extension,
    /// Size in bytes (directories sort as 0 among themselves by name).
    Size,
    /// Modification time.
    Modified,
}

/// Sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    /// Smallest, oldest, or A first.
    Ascending,
    /// Largest, newest, or Z first.
    Descending,
}

/// A pane's sort setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Sort {
    /// What to sort by.
    pub key: SortKey,
    /// Which way.
    pub order: SortOrder,
}

impl Default for Sort {
    fn default() -> Self {
        Sort::by(SortKey::Name)
    }
}

impl Sort {
    /// `key` in its natural order: names and extensions A first, sizes and
    /// times largest and newest first.
    pub fn by(key: SortKey) -> Sort {
        let order = match key {
            SortKey::Name | SortKey::Extension => SortOrder::Ascending,
            SortKey::Size | SortKey::Modified => SortOrder::Descending,
        };
        Sort { key, order }
    }

    /// The same key in the other order.
    pub fn reversed(self) -> Sort {
        let order = match self.order {
            SortOrder::Ascending => SortOrder::Descending,
            SortOrder::Descending => SortOrder::Ascending,
        };
        Sort { order, ..self }
    }
}

/// One entry of [`PersistedState::sorts`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointSort {
    /// The endpoint.
    pub endpoint: Endpoint,
    /// Its last sort.
    pub sort: Sort,
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

    /// Replaces the sort remembered for `endpoint` (one per endpoint).
    pub fn remember_sort(&mut self, endpoint: Endpoint, sort: Sort) {
        match self
            .sorts
            .iter_mut()
            .find(|entry| entry.endpoint == endpoint)
        {
            Some(entry) => entry.sort = sort,
            None => self.sorts.push(EndpointSort { endpoint, sort }),
        }
    }

    /// The last remembered sort of an endpoint.
    pub fn last_sort(&self, endpoint: &Endpoint) -> Option<Sort> {
        self.sorts
            .iter()
            .find(|entry| entry.endpoint == *endpoint)
            .map(|entry| entry.sort)
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

/// Loads `state.json`: the state to use and a warning to show, if any. A
/// missing file is the empty state without a warning; an unparsable one is
/// moved to `state.json.bak` and gives the empty state with
/// `Error::StateCorrupt`; an unreadable one gives the empty state with
/// `Error::StateRead`. Blocking.
pub fn load_state(dir: &Path) -> (PersistedState, Option<Error>) {
    let path = dir.join(STATE_FILE);
    let message = match read_json(&path) {
        Ok(state) => return (state, None),
        Err(JsonFailure::Unreadable(message)) => {
            return (
                PersistedState::default(),
                Some(Error::StateRead { path, message }),
            )
        }
        Err(JsonFailure::Corrupt(message)) => message,
    };
    let backup = dir.join(STATE_BACKUP_FILE);
    let warning = match fs::rename(&path, &backup) {
        Ok(()) => Error::StateCorrupt {
            path,
            backup,
            message,
        },
        Err(error) => Error::StateRead {
            message: format!(
                "{message} (keeping a copy at {} failed: {error})",
                backup.display()
            ),
            path,
        },
    };
    (PersistedState::default(), Some(warning))
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

/// Why a JSON state file could not be loaded.
enum JsonFailure {
    /// Reading the file failed.
    Unreadable(String),
    /// The file was read but is not valid for its type.
    Corrupt(String),
}

/// Reads and parses a JSON file; a missing file is `T::default()`.
fn read_json<T: DeserializeOwned + Default>(path: &Path) -> Result<T, JsonFailure> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(T::default()),
        Err(error) => return Err(JsonFailure::Unreadable(error.to_string())),
    };
    serde_json::from_slice(&bytes).map_err(|error| JsonFailure::Corrupt(error.to_string()))
}

/// [`read_json`] with both failures as `Error::StateRead`.
fn load_json<T: DeserializeOwned + Default>(path: &Path) -> Result<T, Error> {
    read_json(path).map_err(|failure| Error::StateRead {
        path: path.to_path_buf(),
        message: match failure {
            JsonFailure::Unreadable(message) | JsonFailure::Corrupt(message) => message,
        },
    })
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
            sorts: vec![EndpointSort {
                endpoint: sudo_location().endpoint,
                sort: Sort::by(SortKey::Extension).reversed(),
            }],
        };
        let json = serde_json::to_string(&state).expect("serialize");
        assert!(json.contains(r#""sort":{"key":"extension","order":"descending"}"#));
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
    fn sorts_are_one_per_endpoint_with_natural_default_orders() {
        assert_eq!(Sort::default(), Sort::by(SortKey::Name));
        assert_eq!(Sort::by(SortKey::Name).order, SortOrder::Ascending);
        assert_eq!(Sort::by(SortKey::Extension).order, SortOrder::Ascending);
        assert_eq!(Sort::by(SortKey::Size).order, SortOrder::Descending);
        assert_eq!(Sort::by(SortKey::Modified).order, SortOrder::Descending);
        assert_eq!(
            Sort::by(SortKey::Size).reversed().reversed(),
            Sort::by(SortKey::Size)
        );

        let mut state = PersistedState::default();
        let web = sudo_location().endpoint;
        assert_eq!(state.last_sort(&web), None);
        state.remember_sort(web.clone(), Sort::by(SortKey::Size));
        state.remember_sort(Endpoint::Local, Sort::by(SortKey::Modified));
        state.remember_sort(web.clone(), Sort::by(SortKey::Name).reversed());
        assert_eq!(state.sorts.len(), 2);
        assert_eq!(
            state.last_sort(&web),
            Some(Sort::by(SortKey::Name).reversed())
        );
        assert_eq!(
            state.last_sort(&Endpoint::Local),
            Some(Sort::by(SortKey::Modified))
        );
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
        assert_eq!(load_state(dir.path()), (PersistedState::default(), None));
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
        assert_eq!(load_state(&dir), (state, None));
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
    fn corrupt_file_is_kept_as_a_backup_with_a_warning_and_the_next_save_replaces_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let backup = dir.path().join("state.json.bak");
        fs::write(&backup, "older backup").expect("write");
        fs::write(&path, "{not json").expect("write");
        let (state, warning) = load_state(dir.path());
        assert_eq!(state, PersistedState::default());
        assert!(
            matches!(
                &warning,
                Some(Error::StateCorrupt { path: reported, backup: kept, .. })
                    if *reported == path && *kept == backup
            ),
            "{warning:?}"
        );
        assert_eq!(fs::read(&backup).expect("backup"), b"{not json");
        assert!(!path.exists());

        save_state(dir.path(), &PersistedState::default()).expect("save");
        assert_eq!(load_state(dir.path()), (PersistedState::default(), None));
    }

    #[test]
    fn unreadable_state_is_a_warning_with_the_empty_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A directory where the file should be cannot be read as one.
        let path = dir.path().join("state.json");
        fs::create_dir(&path).expect("mkdir");
        let (state, warning) = load_state(dir.path());
        assert_eq!(state, PersistedState::default());
        assert!(
            matches!(&warning, Some(Error::StateRead { path: reported, .. }) if *reported == path),
            "{warning:?}"
        );
        assert!(path.is_dir(), "nothing is moved when nothing could be read");
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
