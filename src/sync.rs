//! Directory comparison and one-way sync.
//!
//! Compares two directory trees by NFC-normalized name, size, and mtime in
//! whole seconds (honoring `mtime-tolerance`), producing a [`Comparison`].
//! A sync copies the selected entries in one direction. It runs via rsync
//! when one side is local, the other is a non-sudo remote, and both report
//! rsync 3.1 or newer; otherwise it runs as a transfer engine job of file
//! pairs.
//!
//! The types below are the payload of `BgEvent::Compared` and of the sync
//! effect.
//!
//! # Contract
//!
//! - Comparison walks both trees with `list` on the Browse channel
//!   (symlinks are compared as entries, never followed). Names are matched
//!   after NFC normalization; `relative` uses the left name when both exist.
//!   When either side holds several names with the same NFC form, those
//!   names are matched byte for byte on both sides instead. A directory present on both sides is
//!   descended into and also listed as `Same` (directories carry no size or
//!   mtime status). A directory present on one side only is descended into
//!   as well, so every file beneath it is listed as `LeftOnly` /
//!   `RightOnly` and can be selected: neither backend recurses on its own.
//! - Status: one side missing → `LeftOnly` / `RightOnly`; directory vs.
//!   non-directory → `KindDiffers`; mtimes differing by more than
//!   `mtime-tolerance` whole seconds → `LeftNewer` / `RightNewer`; else
//!   different sizes → `SizeDiffers`; else `Same`. Unknown attributes on
//!   either side count as different (`SizeDiffers`).
//! - rsync path: eligible when exactly one side is `Local`, the other is a
//!   non-sudo remote with `RemoteTools::rsync_ok`, and the local rsync
//!   passes `connections::rsync_version_ok` ([`local_rsync_ok`], called by
//!   the sync executor). It runs `rsync -a -s --from0 --files-from=- -e
//!   <ssh> <src>/ <dst>/`, where `<ssh>` is `Connections::ssh_command(remote,
//!   false)` encoded for rsync's own `-e` parser, the remote side is written
//!   `host:dir` (`[host]:dir` when the host contains `:`), and the selected
//!   relative paths are fed on stdin, sorted and NUL-terminated.
//! - The `-e` string is split by rsync itself (`do_cmd` in rsync's
//!   `main.c`), not by a shell: it splits on spaces, a quote opens a quoted
//!   run, and inside it a doubled quote is a literal quote; there is no
//!   backslash escape. The session's shell quoting helper writes `'` as
//!   `'\''`, which that parser rejects, so each word is written as `'...'`
//!   with every `'` doubled instead.
//! - The file list uses `--from0` because rsync ends newline-separated
//!   entries at `\r` as well as `\n`, so names containing either would be
//!   cut. rsync drops entries starting with `#` or `;` as comments even with
//!   `--from0`, so such entries are written with a leading `/`, which rsync
//!   strips (a leading `./` would make rsync also copy the source root's
//!   attributes onto the destination root).
//! - The child runs with `process_group(0)` and `kill_on_drop`; cancel drops
//!   the future, which kills rsync's whole process group (rsync and its ssh).
//! - Engine path: a `JobRequest` with `JobTargets::Pairs` of
//!   `(source_dir/rel, destination_dir/rel)` for every selected path that is
//!   a file or symlink on the source side, `JobKind::Copy`, preset
//!   `Overwrite`, under the given job id. Selected directories and paths the
//!   comparison does not know produce no pair.
//!
//! # Public API (Wave 3)
//!
//! ```text
//! pub async fn compare(
//!     left_fs: &dyn Fs,
//!     right_fs: &dyn Fs,
//!     left: Location,
//!     right: Location,
//!     mtime_tolerance: u64,
//! ) -> Result<Comparison, Error>;
//!
//! /// Pure: the status of one path from both sides' attributes.
//! pub fn diff_status(left: Option<&Entry>, right: Option<&Entry>, mtime_tolerance: u64) -> DiffStatus;
//!
//! pub fn rsync_eligible(left: &Endpoint, right: &Endpoint, remote: RemoteTools, local_rsync_ok: bool) -> bool;
//!
//! /// Whether the local `rsync` in `PATH` is rsync 3.1 or newer.
//! pub async fn local_rsync_ok() -> bool;
//!
//! /// Runs rsync to completion as described above.
//! pub async fn run_rsync(
//!     ssh: &[String],
//!     direction: SyncDirection,
//!     left: &Location,
//!     right: &Location,
//!     paths: &[PathBuf],
//! ) -> Result<(), Error>;
//!
//! /// The engine job for a sync. `comparison` supplies the source kinds.
//! pub fn engine_job(job: JobId, direction: SyncDirection, comparison: &Comparison, paths: &[PathBuf]) -> JobRequest;
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use unicode_normalization::UnicodeNormalization;

use crate::connections::{rsync_version_ok, RemoteTools};
use crate::error::{Error, ExitStatus};
use crate::transfer::model::{ConflictChoice, JobId, JobKind, JobRequest, JobTargets};
use crate::vfs::{Endpoint, Entry, EntryKind, Fs, Location};

/// Which way a one-way sync copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncDirection {
    /// From the left pane's directory to the right pane's.
    LeftToRight,
    /// From the right pane's directory to the left pane's.
    RightToLeft,
}

/// How one path differs between the two trees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffStatus {
    /// Exists only on the left.
    LeftOnly,
    /// Exists only on the right.
    RightOnly,
    /// Both exist; the left mtime is newer.
    LeftNewer,
    /// Both exist; the right mtime is newer.
    RightNewer,
    /// Same mtime but different size.
    SizeDiffers,
    /// One side is a directory and the other is not.
    KindDiffers,
    /// Same size and mtime.
    Same,
}

/// One compared path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComparisonEntry {
    /// Path relative to both compared directories.
    pub relative: PathBuf,
    /// How the two sides differ.
    pub status: DiffStatus,
    /// The left side's attributes, when it exists.
    pub left: Option<Entry>,
    /// The right side's attributes, when it exists.
    pub right: Option<Entry>,
}

/// The result of comparing two directory trees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    /// The left directory.
    pub left: Location,
    /// The right directory.
    pub right: Location,
    /// Every path present on either side, sorted by relative path.
    pub entries: Vec<ComparisonEntry>,
}

/// Number of trailing rsync stderr lines kept in `Error::CommandFailed`.
const STDERR_TAIL_LINES: usize = 20;

/// A directory still to be listed: its path relative to both roots and its
/// absolute path on each side that has it.
struct PendingDir {
    relative: PathBuf,
    left: Option<PathBuf>,
    right: Option<PathBuf>,
}

/// Compares the trees under `left` and `right`, listing `left` with
/// `left_fs` and `right` with `right_fs` (the Browse channels). Fails with
/// the first listing error.
pub async fn compare(
    left_fs: &dyn Fs,
    right_fs: &dyn Fs,
    left: Location,
    right: Location,
    mtime_tolerance: u64,
) -> Result<Comparison, Error> {
    let mut entries = Vec::new();
    let mut pending = vec![PendingDir {
        relative: PathBuf::new(),
        left: Some(left.path.clone()),
        right: Some(right.path.clone()),
    }];
    while let Some(dir) = pending.pop() {
        let left_list = match &dir.left {
            Some(path) => left_fs.list(path).await?,
            None => Vec::new(),
        };
        let right_list = match &dir.right {
            Some(path) => right_fs.list(path).await?,
            None => Vec::new(),
        };
        for (name, left_entry, right_entry) in match_names(left_list, right_list) {
            let relative = dir.relative.join(&name);
            let left_dir = subdir(&dir.left, left_entry.as_ref());
            let right_dir = subdir(&dir.right, right_entry.as_ref());
            // A directory facing a non-directory is one `KindDiffers` row;
            // its contents would collide with the file on the other side.
            let kind_differs = left_entry.is_some()
                && right_entry.is_some()
                && left_dir.is_some() != right_dir.is_some();
            if (left_dir.is_some() || right_dir.is_some()) && !kind_differs {
                pending.push(PendingDir {
                    relative: relative.clone(),
                    left: left_dir,
                    right: right_dir,
                });
            }
            entries.push(ComparisonEntry {
                status: diff_status(left_entry.as_ref(), right_entry.as_ref(), mtime_tolerance),
                relative,
                left: left_entry,
                right: right_entry,
            });
        }
    }
    entries.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(Comparison {
        left,
        right,
        entries,
    })
}

/// The absolute path of `entry` below `parent` when `entry` is a directory
/// (never a symlink to one).
fn subdir(parent: &Option<PathBuf>, entry: Option<&Entry>) -> Option<PathBuf> {
    match (parent, entry) {
        (Some(parent), Some(entry)) if entry.kind == EntryKind::Dir => {
            Some(parent.join(&entry.name))
        }
        _ => None,
    }
}

/// One name of a directory listing, matched across both sides.
type MatchedName = (OsString, Option<Entry>, Option<Entry>);

/// Pairs the entries of one directory on both sides by match key. The name
/// of a pair is the left name when both sides have the entry.
///
/// The key is the NFC name, except for names whose NFC form is shared by
/// several entries on either side (possible on file systems that do not
/// normalize, such as ext4): all of those are keyed by their raw bytes on
/// both sides, so byte-identical names still pair up.
fn match_names(left: Vec<Entry>, right: Vec<Entry>) -> Vec<MatchedName> {
    let ambiguous: HashSet<OsString> = ambiguous_nfc(&left)
        .into_iter()
        .chain(ambiguous_nfc(&right))
        .collect();
    let key = |entry: &Entry| {
        let nfc = nfc_name(&entry.name);
        if ambiguous.contains(&nfc) {
            entry.name.clone()
        } else {
            nfc
        }
    };
    let mut paired: BTreeMap<OsString, MatchedName> = BTreeMap::new();
    for entry in left {
        paired.insert(key(&entry), (entry.name.clone(), Some(entry), None));
    }
    for entry in right {
        paired
            .entry(key(&entry))
            .and_modify(|pair| pair.2 = Some(entry.clone()))
            .or_insert_with(|| (entry.name.clone(), None, Some(entry)));
    }
    paired.into_values().collect()
}

/// The NFC names shared by more than one entry of one directory listing.
fn ambiguous_nfc(entries: &[Entry]) -> HashSet<OsString> {
    let mut seen = HashSet::new();
    let mut ambiguous = HashSet::new();
    for entry in entries {
        let nfc = nfc_name(&entry.name);
        if !seen.insert(nfc.clone()) {
            ambiguous.insert(nfc);
        }
    }
    ambiguous
}

/// The NFC form of a name; names that are not UTF-8 are kept unchanged.
fn nfc_name(name: &OsStr) -> OsString {
    match name.to_str() {
        Some(text) => OsString::from(text.nfc().collect::<String>()),
        None => name.to_os_string(),
    }
}

/// The status of one path from both sides' attributes. `(None, None)`
/// describes no path at all and reports `Same` (nothing to sync).
pub fn diff_status(
    left: Option<&Entry>,
    right: Option<&Entry>,
    mtime_tolerance: u64,
) -> DiffStatus {
    let (left, right) = match (left, right) {
        (None, None) => return DiffStatus::Same,
        (Some(_), None) => return DiffStatus::LeftOnly,
        (None, Some(_)) => return DiffStatus::RightOnly,
        (Some(left), Some(right)) => (left, right),
    };
    match (left.kind == EntryKind::Dir, right.kind == EntryKind::Dir) {
        (true, true) => return DiffStatus::Same,
        (true, false) | (false, true) => return DiffStatus::KindDiffers,
        (false, false) => {}
    }
    let (Some(left_mtime), Some(right_mtime), Some(left_size), Some(right_size)) =
        (left.mtime, right.mtime, left.size, right.size)
    else {
        return DiffStatus::SizeDiffers;
    };
    if left_mtime.abs_diff(right_mtime) > mtime_tolerance {
        if left_mtime > right_mtime {
            DiffStatus::LeftNewer
        } else {
            DiffStatus::RightNewer
        }
    } else if left_size != right_size {
        DiffStatus::SizeDiffers
    } else {
        DiffStatus::Same
    }
}

/// Whether a sync between `left` and `right` may run via rsync: exactly one
/// side is local, the other is a non-sudo remote whose probed tools
/// (`remote`) report a usable rsync, and the local rsync is usable too
/// (`local_rsync_ok`, see [`local_rsync_ok`]).
pub fn rsync_eligible(
    left: &Endpoint,
    right: &Endpoint,
    remote: RemoteTools,
    local_rsync_ok: bool,
) -> bool {
    let remote_is_plain =
        |endpoint: &Endpoint| matches!(endpoint, Endpoint::Remote { sudo: false, .. });
    let one_local_one_plain_remote =
        (left.is_local() && remote_is_plain(right)) || (right.is_local() && remote_is_plain(left));
    one_local_one_plain_remote && remote.rsync_ok && local_rsync_ok
}

/// Whether the local `rsync` found in `PATH` (the one [`run_rsync`] runs) is
/// rsync 3.1 or newer, judged by `connections::rsync_version_ok` on its
/// `rsync --version` output; macOS's openrsync does not qualify. An rsync
/// that cannot be started (typically not installed) or exits with failure
/// is not usable, so the answer is `false` and the sync takes the engine
/// path; that is the decision this probe exists to make, not an error.
pub async fn local_rsync_ok() -> bool {
    let output = Command::new("rsync")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true)
        .output()
        .await;
    match output {
        Ok(output) => {
            output.status.success() && rsync_version_ok(&String::from_utf8_lossy(&output.stdout))
        }
        Err(_) => false,
    }
}

/// Runs rsync to completion, copying `paths` (relative to both directories)
/// from the source to the destination side of `direction`. `ssh` is
/// `Connections::ssh_command(remote, false)` for the remote side. Fails with
/// `Error::CommandFailed` when rsync cannot be started or does not succeed.
/// Dropping the future kills rsync and its ssh.
pub async fn run_rsync(
    ssh: &[String],
    direction: SyncDirection,
    left: &Location,
    right: &Location,
    paths: &[PathBuf],
) -> Result<(), Error> {
    let mut child = Command::new("rsync")
        .args(rsync_args(ssh, direction, left, right))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| Error::CommandFailed {
            program: "rsync".into(),
            status: ExitStatus::NotStarted,
            stderr: error.to_string(),
        })?;
    let group = child.id().map(ProcessGroupGuard);
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let list = files_from(paths);
    let write = async move {
        stdin.write_all(&list).await?;
        stdin.shutdown().await
    };
    // Write the list while rsync runs and its stderr is drained, so neither
    // side can block on a full pipe.
    let (written, output) = tokio::join!(write, child.wait_with_output());
    let output = output.map_err(|error| Error::CommandFailed {
        program: "rsync".into(),
        status: ExitStatus::NotStarted,
        stderr: error.to_string(),
    })?;
    if let Some(group) = group {
        group.disarm();
    }
    let stderr = stderr_tail(&output.stderr);
    if !output.status.success() {
        let status = match (output.status.code(), output.status.signal()) {
            (Some(code), _) => ExitStatus::Code(code),
            (None, Some(signal)) => ExitStatus::Signal(signal),
            (None, None) => unreachable!("a finished process has a code or a signal"),
        };
        return Err(Error::CommandFailed {
            program: "rsync".into(),
            status,
            stderr,
        });
    }
    written.map_err(|error| Error::CommandFailed {
        program: "rsync".into(),
        status: ExitStatus::Code(0),
        stderr: format!("the file list could not be sent: {error}"),
    })
}

/// Kills a child's whole process group when dropped before
/// [`ProcessGroupGuard::disarm`]. The child is the group leader
/// (`process_group(0)`), so the group id is its pid; `kill_on_drop` alone
/// would leave rsync's ssh child running.
struct ProcessGroupGuard(u32);

impl ProcessGroupGuard {
    /// Forgets the group once the leader has been reaped (its id may then be
    /// reused).
    fn disarm(self) {
        std::mem::forget(self);
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        let Ok(group) = libc::pid_t::try_from(self.0) else {
            return;
        };
        // SAFETY: killpg only sends a signal. The leader has not been reaped
        // yet (the guard is disarmed right after reaping), so the group id
        // still names our group. ESRCH (group already gone) is the only
        // expected failure and leaves nothing to do in a destructor.
        unsafe {
            libc::killpg(group, libc::SIGKILL);
        }
    }
}

/// The rsync arguments after the program name.
fn rsync_args(
    ssh: &[String],
    direction: SyncDirection,
    left: &Location,
    right: &Location,
) -> Vec<OsString> {
    let (source, destination) = match direction {
        SyncDirection::LeftToRight => (left, right),
        SyncDirection::RightToLeft => (right, left),
    };
    vec![
        "-a".into(),
        "-s".into(),
        "--from0".into(),
        "--files-from=-".into(),
        "-e".into(),
        rsync_rsh(ssh).into(),
        rsync_location(source),
        rsync_location(destination),
    ]
}

/// Encodes an argv for rsync's `-e` parser: every word in single quotes,
/// with each `'` doubled.
fn rsync_rsh(words: &[String]) -> String {
    words
        .iter()
        .map(|word| format!("'{}'", word.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A transfer root as rsync expects it: `dir/` locally (an absolute path, so
/// rsync never mistakes a `:` in it for a host separator) and `host:dir/`
/// remotely. The trailing slash makes rsync use the directory's contents.
fn rsync_location(location: &Location) -> OsString {
    let mut path = location.path.as_os_str().to_os_string();
    if !path.as_bytes().ends_with(b"/") {
        path.push("/");
    }
    match &location.endpoint {
        Endpoint::Local => path,
        Endpoint::Remote { host, .. } => {
            let mut spec = if host.contains(':') {
                OsString::from(format!("[{host}]:"))
            } else {
                OsString::from(format!("{host}:"))
            };
            spec.push(path);
            spec
        }
    }
}

/// The `--from0 --files-from=-` input: sorted, deduplicated relative paths,
/// each NUL-terminated. rsync drops entries starting with `#` or `;` as
/// comments, so those get a leading `/`, which rsync strips again. (A
/// leading `./` would also send `.`, stamping the source root's mode and
/// mtime onto the destination root.)
fn files_from(paths: &[PathBuf]) -> Vec<u8> {
    let mut sorted: Vec<&PathBuf> = paths.iter().collect();
    sorted.sort();
    sorted.dedup();
    let mut list = Vec::new();
    for path in sorted {
        let bytes = path.as_os_str().as_bytes();
        if matches!(bytes.first(), Some(b'#' | b';')) {
            list.push(b'/');
        }
        list.extend_from_slice(bytes);
        list.push(0);
    }
    list
}

/// The last [`STDERR_TAIL_LINES`] non-empty lines of rsync's stderr.
fn stderr_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.is_empty())
        .collect();
    lines[lines.len().saturating_sub(STDERR_TAIL_LINES)..].join("\n")
}

/// The engine job for a sync: one `(source, destination)` pair for every
/// selected path that is a file or symlink on the source side according to
/// `comparison`. Directories need no pair (the engine creates parent
/// directories), and paths the comparison does not list are left out.
pub fn engine_job(
    job: JobId,
    direction: SyncDirection,
    comparison: &Comparison,
    paths: &[PathBuf],
) -> JobRequest {
    let (source, destination) = match direction {
        SyncDirection::LeftToRight => (&comparison.left, &comparison.right),
        SyncDirection::RightToLeft => (&comparison.right, &comparison.left),
    };
    let by_path: HashMap<&Path, &ComparisonEntry> = comparison
        .entries
        .iter()
        .map(|entry| (entry.relative.as_path(), entry))
        .collect();
    let pairs = paths
        .iter()
        .filter(|path| {
            let Some(entry) = by_path.get(path.as_path()) else {
                return false;
            };
            let source_side = match direction {
                SyncDirection::LeftToRight => entry.left.as_ref(),
                SyncDirection::RightToLeft => entry.right.as_ref(),
            };
            matches!(
                source_side.map(|entry| &entry.kind),
                Some(EntryKind::File | EntryKind::Symlink { .. })
            )
        })
        .map(|path| (source.path.join(path), destination.path.join(path)))
        .collect();
    JobRequest {
        id: job,
        kind: JobKind::Copy,
        source: source.endpoint.clone(),
        destination: destination.endpoint.clone(),
        targets: JobTargets::Pairs(pairs),
        preset: Some(ConflictChoice::Overwrite),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::FsError;
    use crate::vfs::{Capabilities, FsReader, FsResult, FsWriter, WriteMode};
    use futures_util::future::BoxFuture;

    /// An in-memory tree that serves `list` only; compare needs nothing else.
    struct TreeFs {
        endpoint: Endpoint,
        dirs: HashMap<PathBuf, Vec<Entry>>,
    }

    impl TreeFs {
        fn new(endpoint: Endpoint, dirs: &[(&str, Vec<Entry>)]) -> TreeFs {
            TreeFs {
                endpoint,
                dirs: dirs
                    .iter()
                    .map(|(path, entries)| (PathBuf::from(path), entries.clone()))
                    .collect(),
            }
        }
    }

    fn unsupported<T: Send + 'static>(operation: &str) -> BoxFuture<'static, FsResult<T>> {
        let error = FsError::Unsupported {
            operation: operation.into(),
            reason: "not part of the comparison test tree".into(),
        };
        Box::pin(async move { Err(error) })
    }

    impl Fs for TreeFs {
        fn endpoint(&self) -> &Endpoint {
            &self.endpoint
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                posix_rename: false,
                copy_within: false,
                symlink: false,
                trash: false,
            }
        }
        fn home(&self) -> BoxFuture<'_, FsResult<PathBuf>> {
            unsupported("home")
        }
        fn canonicalize<'a>(&'a self, _: &'a Path) -> BoxFuture<'a, FsResult<PathBuf>> {
            unsupported("canonicalize")
        }
        fn list<'a>(&'a self, dir: &'a Path) -> BoxFuture<'a, FsResult<Vec<Entry>>> {
            Box::pin(async move {
                self.dirs
                    .get(dir)
                    .cloned()
                    .ok_or_else(|| FsError::NotFound { path: dir.into() })
            })
        }
        fn stat<'a>(&'a self, _: &'a Path) -> BoxFuture<'a, FsResult<Entry>> {
            unsupported("stat")
        }
        fn lstat<'a>(&'a self, _: &'a Path) -> BoxFuture<'a, FsResult<Entry>> {
            unsupported("lstat")
        }
        fn mkdir<'a>(&'a self, _: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            unsupported("mkdir")
        }
        fn rename<'a>(&'a self, _: &'a Path, _: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            unsupported("rename")
        }
        fn remove_file<'a>(&'a self, _: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            unsupported("remove_file")
        }
        fn remove_dir<'a>(&'a self, _: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            unsupported("remove_dir")
        }
        fn trash<'a>(&'a self, _: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            unsupported("trash")
        }
        fn chmod<'a>(&'a self, _: &'a Path, _: u32) -> BoxFuture<'a, FsResult<()>> {
            unsupported("chmod")
        }
        fn set_times<'a>(&'a self, _: &'a Path, _: i64, _: i64) -> BoxFuture<'a, FsResult<()>> {
            unsupported("set_times")
        }
        fn read_link<'a>(&'a self, _: &'a Path) -> BoxFuture<'a, FsResult<PathBuf>> {
            unsupported("read_link")
        }
        fn symlink<'a>(&'a self, _: &'a Path, _: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            unsupported("symlink")
        }
        fn open_read<'a>(&'a self, _: &'a Path, _: u64) -> BoxFuture<'a, FsResult<FsReader>> {
            unsupported("open_read")
        }
        fn open_write<'a>(
            &'a self,
            _: &'a Path,
            _: WriteMode,
        ) -> BoxFuture<'a, FsResult<FsWriter>> {
            unsupported("open_write")
        }
        fn copy_within<'a>(&'a self, _: &'a Path, _: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            unsupported("copy_within")
        }
    }

    fn file(name: &str, size: u64, mtime: i64) -> Entry {
        Entry {
            name: name.into(),
            kind: EntryKind::File,
            size: Some(size),
            mtime: Some(mtime),
            mode: Some(0o644),
        }
    }

    fn dir(name: &str, mtime: i64) -> Entry {
        Entry {
            name: name.into(),
            kind: EntryKind::Dir,
            size: Some(4096),
            mtime: Some(mtime),
            mode: Some(0o755),
        }
    }

    fn link(name: &str, target: &str) -> Entry {
        Entry {
            name: name.into(),
            kind: EntryKind::Symlink {
                target: target.into(),
                target_is_dir: true,
            },
            size: Some(target.len() as u64),
            mtime: Some(100),
            mode: Some(0o777),
        }
    }

    fn remote() -> Endpoint {
        Endpoint::Remote {
            host: "web".into(),
            sudo: false,
        }
    }

    fn location(endpoint: Endpoint, path: &str) -> Location {
        Location {
            endpoint,
            path: path.into(),
        }
    }

    fn statuses(comparison: &Comparison) -> Vec<(String, DiffStatus)> {
        comparison
            .entries
            .iter()
            .map(|entry| (entry.relative.to_string_lossy().into_owned(), entry.status))
            .collect()
    }

    #[test]
    fn diff_status_covers_every_status() {
        let a = file("a", 10, 100);
        assert_eq!(diff_status(Some(&a), None, 0), DiffStatus::LeftOnly);
        assert_eq!(diff_status(None, Some(&a), 0), DiffStatus::RightOnly);
        assert_eq!(diff_status(None, None, 0), DiffStatus::Same);
        assert_eq!(
            diff_status(Some(&a), Some(&file("a", 10, 100)), 0),
            DiffStatus::Same
        );
        assert_eq!(
            diff_status(Some(&file("a", 10, 101)), Some(&a), 0),
            DiffStatus::LeftNewer
        );
        assert_eq!(
            diff_status(Some(&a), Some(&file("a", 10, 101)), 0),
            DiffStatus::RightNewer
        );
        assert_eq!(
            diff_status(Some(&a), Some(&file("a", 11, 100)), 0),
            DiffStatus::SizeDiffers
        );
        assert_eq!(
            diff_status(Some(&dir("a", 1)), Some(&a), 0),
            DiffStatus::KindDiffers
        );
        assert_eq!(
            diff_status(Some(&a), Some(&dir("a", 1)), 0),
            DiffStatus::KindDiffers
        );
        assert_eq!(
            diff_status(Some(&link("a", "/srv")), Some(&dir("a", 1)), 0),
            DiffStatus::KindDiffers
        );
    }

    #[test]
    fn mtime_wins_over_size_and_tolerance_is_inclusive() {
        let old = file("a", 10, 100);
        let newer_and_bigger = file("a", 99, 103);
        assert_eq!(
            diff_status(Some(&newer_and_bigger), Some(&old), 0),
            DiffStatus::LeftNewer
        );
        assert_eq!(
            diff_status(Some(&newer_and_bigger), Some(&old), 2),
            DiffStatus::LeftNewer
        );
        // Exactly the tolerance apart counts as equal mtimes.
        assert_eq!(
            diff_status(Some(&newer_and_bigger), Some(&old), 3),
            DiffStatus::SizeDiffers
        );
        assert_eq!(
            diff_status(Some(&file("a", 10, 102)), Some(&old), 2),
            DiffStatus::Same
        );
        // Extreme mtimes do not overflow.
        assert_eq!(
            diff_status(
                Some(&file("a", 1, i64::MAX)),
                Some(&file("a", 1, i64::MIN)),
                u64::MAX
            ),
            DiffStatus::Same
        );
    }

    #[test]
    fn directories_ignore_attributes_and_unknown_attributes_differ() {
        assert_eq!(
            diff_status(Some(&dir("d", 1)), Some(&dir("d", 999)), 0),
            DiffStatus::Same
        );
        let mut unknown_size = file("a", 10, 100);
        unknown_size.size = None;
        assert_eq!(
            diff_status(Some(&unknown_size), Some(&file("a", 10, 100)), 0),
            DiffStatus::SizeDiffers
        );
        let mut unknown_mtime = file("a", 10, 100);
        unknown_mtime.mtime = None;
        assert_eq!(
            diff_status(Some(&file("a", 10, 100)), Some(&unknown_mtime), 0),
            DiffStatus::SizeDiffers
        );
    }

    #[tokio::test]
    async fn compare_walks_both_trees() {
        let left_fs = TreeFs::new(
            Endpoint::Local,
            &[
                (
                    "/l",
                    vec![
                        file("same.txt", 5, 100),
                        file("newer.txt", 5, 200),
                        file("left.txt", 1, 1),
                        dir("both", 1),
                        dir("new", 1),
                        dir("clash", 1),
                        link("link", "/srv"),
                    ],
                ),
                ("/l/both", vec![file("inner.txt", 3, 50)]),
                ("/l/new", vec![dir("deep", 1)]),
                ("/l/new/deep", vec![file("x.bin", 7, 7)]),
                ("/l/clash", vec![file("hidden", 1, 1)]),
            ],
        );
        let right_fs = TreeFs::new(
            remote(),
            &[
                (
                    "/r",
                    vec![
                        file("same.txt", 5, 100),
                        file("newer.txt", 5, 100),
                        file("right.txt", 2, 2),
                        dir("both", 9),
                        file("clash", 1, 1),
                        link("link", "/srv"),
                    ],
                ),
                (
                    "/r/both",
                    vec![file("inner.txt", 4, 50), file("extra", 1, 1)],
                ),
            ],
        );
        let comparison = compare(
            &left_fs,
            &right_fs,
            location(Endpoint::Local, "/l"),
            location(remote(), "/r"),
            0,
        )
        .await
        .expect("compare succeeds");
        assert_eq!(
            statuses(&comparison),
            vec![
                ("both".into(), DiffStatus::Same),
                ("both/extra".into(), DiffStatus::RightOnly),
                ("both/inner.txt".into(), DiffStatus::SizeDiffers),
                ("clash".into(), DiffStatus::KindDiffers),
                ("left.txt".into(), DiffStatus::LeftOnly),
                ("link".into(), DiffStatus::Same),
                ("new".into(), DiffStatus::LeftOnly),
                ("new/deep".into(), DiffStatus::LeftOnly),
                ("new/deep/x.bin".into(), DiffStatus::LeftOnly),
                ("newer.txt".into(), DiffStatus::LeftNewer),
                ("right.txt".into(), DiffStatus::RightOnly),
                ("same.txt".into(), DiffStatus::Same),
            ]
        );
        assert_eq!(comparison.left, location(Endpoint::Local, "/l"));
        assert_eq!(comparison.right, location(remote(), "/r"));
        let right_only = &comparison.entries[1];
        assert_eq!(right_only.left, None);
        assert_eq!(right_only.right, Some(file("extra", 1, 1)));
    }

    #[tokio::test]
    async fn compare_matches_names_after_nfc_and_keeps_the_left_spelling() {
        let decomposed = "caf\u{65}\u{301}";
        let composed = "caf\u{e9}";
        let left_fs = TreeFs::new(Endpoint::Local, &[("/l", vec![file(decomposed, 1, 1)])]);
        let right_fs = TreeFs::new(remote(), &[("/r", vec![file(composed, 1, 1)])]);
        let comparison = compare(
            &left_fs,
            &right_fs,
            location(Endpoint::Local, "/l"),
            location(remote(), "/r"),
            0,
        )
        .await
        .expect("compare succeeds");
        assert_eq!(comparison.entries.len(), 1);
        assert_eq!(comparison.entries[0].relative, PathBuf::from(decomposed));
        assert_eq!(comparison.entries[0].status, DiffStatus::Same);
    }

    async fn compare_listings(left: Vec<Entry>, right: Vec<Entry>) -> Vec<(PathBuf, DiffStatus)> {
        let left_fs = TreeFs::new(Endpoint::Local, &[("/l", left)]);
        let right_fs = TreeFs::new(remote(), &[("/r", right)]);
        compare(
            &left_fs,
            &right_fs,
            location(Endpoint::Local, "/l"),
            location(remote(), "/r"),
            0,
        )
        .await
        .expect("compare succeeds")
        .entries
        .into_iter()
        .map(|entry| (entry.relative, entry.status))
        .collect()
    }

    #[tokio::test]
    async fn names_sharing_an_nfc_form_match_byte_for_byte_on_both_sides() {
        let decomposed = "\u{65}\u{301}";
        let composed = "\u{e9}";
        // Sorted by relative path: `e` (0x65) sorts before the UTF-8 lead
        // byte 0xC3 of the composed form.
        assert_eq!(
            compare_listings(
                vec![file(decomposed, 1, 1), file(composed, 2, 2)],
                vec![file(decomposed, 1, 1)],
            )
            .await,
            vec![
                (PathBuf::from(decomposed), DiffStatus::Same),
                (PathBuf::from(composed), DiffStatus::LeftOnly),
            ]
        );
        assert_eq!(
            compare_listings(
                vec![file(composed, 2, 2)],
                vec![file(decomposed, 1, 1), file(composed, 2, 2)],
            )
            .await,
            vec![
                (PathBuf::from(decomposed), DiffStatus::RightOnly),
                (PathBuf::from(composed), DiffStatus::Same),
            ]
        );
    }

    #[tokio::test]
    async fn compare_reports_listing_failures() {
        let left_fs = TreeFs::new(Endpoint::Local, &[("/l", vec![dir("sub", 1)])]);
        let right_fs = TreeFs::new(remote(), &[("/r", vec![dir("sub", 1)])]);
        let error = compare(
            &left_fs,
            &right_fs,
            location(Endpoint::Local, "/l"),
            location(remote(), "/r"),
            0,
        )
        .await
        .expect_err("the subdirectory listing is missing");
        assert_eq!(
            error,
            Error::Fs(FsError::NotFound {
                path: "/l/sub".into()
            })
        );
    }

    fn sample_comparison() -> Comparison {
        let entry =
            |relative: &str, status, left: Option<Entry>, right: Option<Entry>| ComparisonEntry {
                relative: relative.into(),
                status,
                left,
                right,
            };
        Comparison {
            left: location(Endpoint::Local, "/l"),
            right: location(remote(), "/r"),
            entries: vec![
                entry(
                    "a.txt",
                    DiffStatus::LeftNewer,
                    Some(file("a.txt", 1, 9)),
                    Some(file("a.txt", 1, 1)),
                ),
                entry("d", DiffStatus::LeftOnly, Some(dir("d", 1)), None),
                entry(
                    "d/b.txt",
                    DiffStatus::LeftOnly,
                    Some(file("b.txt", 1, 1)),
                    None,
                ),
                entry(
                    "link",
                    DiffStatus::LeftOnly,
                    Some(link("link", "/srv")),
                    None,
                ),
                entry(
                    "r.txt",
                    DiffStatus::RightOnly,
                    None,
                    Some(file("r.txt", 1, 1)),
                ),
            ],
        }
    }

    #[test]
    fn engine_job_pairs_source_files_and_symlinks_left_to_right() {
        let paths: Vec<PathBuf> = ["a.txt", "d", "d/b.txt", "link", "r.txt", "unknown"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let request = engine_job(
            JobId(7),
            SyncDirection::LeftToRight,
            &sample_comparison(),
            &paths,
        );
        assert_eq!(
            request,
            JobRequest {
                id: JobId(7),
                kind: JobKind::Copy,
                source: Endpoint::Local,
                destination: remote(),
                targets: JobTargets::Pairs(vec![
                    ("/l/a.txt".into(), "/r/a.txt".into()),
                    ("/l/d/b.txt".into(), "/r/d/b.txt".into()),
                    ("/l/link".into(), "/r/link".into()),
                ]),
                preset: Some(ConflictChoice::Overwrite),
            }
        );
    }

    #[test]
    fn engine_job_right_to_left_uses_the_right_side_kinds() {
        let paths: Vec<PathBuf> = ["a.txt", "d/b.txt", "r.txt"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let request = engine_job(
            JobId(8),
            SyncDirection::RightToLeft,
            &sample_comparison(),
            &paths,
        );
        assert_eq!(request.source, remote());
        assert_eq!(request.destination, Endpoint::Local);
        assert_eq!(
            request.targets,
            JobTargets::Pairs(vec![
                ("/r/a.txt".into(), "/l/a.txt".into()),
                ("/r/r.txt".into(), "/l/r.txt".into()),
            ])
        );
    }

    #[test]
    fn rsync_needs_one_local_side_a_plain_remote_and_both_rsyncs() {
        let ok = RemoteTools {
            tar: true,
            rsync_ok: true,
        };
        let no_rsync = RemoteTools {
            tar: true,
            rsync_ok: false,
        };
        let sudo = Endpoint::Remote {
            host: "web".into(),
            sudo: true,
        };
        let other = Endpoint::Remote {
            host: "db".into(),
            sudo: false,
        };
        assert!(rsync_eligible(&Endpoint::Local, &remote(), ok, true));
        assert!(rsync_eligible(&remote(), &Endpoint::Local, ok, true));
        assert!(!rsync_eligible(&Endpoint::Local, &remote(), ok, false));
        assert!(!rsync_eligible(&Endpoint::Local, &remote(), no_rsync, true));
        assert!(!rsync_eligible(&Endpoint::Local, &sudo, ok, true));
        assert!(!rsync_eligible(&sudo, &Endpoint::Local, ok, true));
        assert!(!rsync_eligible(
            &Endpoint::Local,
            &Endpoint::Local,
            ok,
            true
        ));
        assert!(!rsync_eligible(&other, &remote(), ok, true));
    }

    /// rsync's `-e` tokenizer, ported from `do_cmd` in rsync's `main.c`.
    fn rsync_split(command: &str) -> Vec<String> {
        let bytes = command.as_bytes();
        let mut words = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b' ' {
                index += 1;
                continue;
            }
            let mut word = Vec::new();
            let mut in_quote = None;
            while index < bytes.len() && (bytes[index] != b' ' || in_quote.is_some()) {
                let byte = bytes[index];
                if byte == b'\'' || byte == b'"' {
                    match in_quote {
                        None => {
                            in_quote = Some(byte);
                            index += 1;
                            continue;
                        }
                        Some(quote) if quote == byte => {
                            index += 1;
                            if bytes.get(index) != Some(&quote) {
                                in_quote = None;
                                continue;
                            }
                        }
                        Some(_) => {}
                    }
                }
                word.push(bytes[index]);
                index += 1;
            }
            assert!(in_quote.is_none(), "missing trailing quote in {command:?}");
            words.push(String::from_utf8(word).expect("UTF-8 test words"));
        }
        words
    }

    #[test]
    fn rsh_round_trips_through_the_rsync_parser() {
        let words: Vec<String> = [
            "ssh",
            "-F",
            "/Users/me/my config",
            "-o",
            "ControlPath=/tmp/scpto-501/42/%C",
            "it's",
            "trailing'",
            "'leading",
            "double\"quote",
            "",
            "two  spaces",
        ]
        .iter()
        .map(|word| word.to_string())
        .collect();
        assert_eq!(rsync_split(&rsync_rsh(&words)), words);
    }

    #[test]
    fn rsync_arguments_follow_the_direction() {
        let ssh = vec![
            "ssh".to_string(),
            "-o".to_string(),
            "BatchMode=yes".to_string(),
        ];
        let local = location(Endpoint::Local, "/Users/me/site");
        let server = location(remote(), "/var/www/");
        let expected_rsh = OsString::from("'ssh' '-o' 'BatchMode=yes'");
        let up = rsync_args(&ssh, SyncDirection::LeftToRight, &local, &server);
        assert_eq!(
            up,
            vec![
                OsString::from("-a"),
                "-s".into(),
                "--from0".into(),
                "--files-from=-".into(),
                "-e".into(),
                expected_rsh,
                "/Users/me/site/".into(),
                "web:/var/www/".into(),
            ]
        );
        let down = rsync_args(&ssh, SyncDirection::RightToLeft, &local, &server);
        assert_eq!(
            down[6..],
            [OsString::from("web:/var/www/"), "/Users/me/site/".into()]
        );
        let v6 = location(
            Endpoint::Remote {
                host: "fe80::1".into(),
                sudo: false,
            },
            "/srv",
        );
        assert_eq!(rsync_location(&v6), OsString::from("[fe80::1]:/srv/"));
        assert_eq!(
            rsync_location(&location(Endpoint::Local, "/")),
            OsString::from("/")
        );
    }

    #[test]
    fn files_from_is_sorted_nul_terminated_and_protects_comment_names() {
        let paths: Vec<PathBuf> = [
            "b/line\nbreak",
            "#autosave#",
            "a.txt",
            ";x",
            "a.txt",
            "dir/#y",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(
            files_from(&paths),
            b"/#autosave#\0/;x\0a.txt\0b/line\nbreak\0dir/#y\0".to_vec()
        );
    }

    #[test]
    fn stderr_tail_keeps_the_last_lines() {
        let stderr: String = (0..30).map(|n| format!("line {n}\r\n")).collect();
        let tail = stderr_tail(stderr.as_bytes());
        assert!(tail.starts_with("line 10\nline 11"));
        assert!(tail.ends_with("line 29"));
        assert_eq!(stderr_tail(b"\n\n"), "");
    }

    #[tokio::test]
    async fn rsync_failure_reports_exit_status() {
        // A local-to-local run from a missing source fails inside the local
        // rsync without touching the network.
        let missing = location(Endpoint::Local, "/nonexistent-scpto-sync-source");
        let target = tempfile::tempdir().expect("temp dir");
        let destination = location(
            Endpoint::Local,
            target.path().to_str().expect("UTF-8 temp path"),
        );
        let error = run_rsync(
            &["ssh".to_string()],
            SyncDirection::LeftToRight,
            &missing,
            &destination,
            &[PathBuf::from("a.txt")],
        )
        .await
        .expect_err("the source does not exist");
        match error {
            Error::CommandFailed {
                program, status, ..
            } => {
                assert_eq!(program, "rsync");
                assert!(
                    matches!(status, ExitStatus::Code(code) if code != 0),
                    "{status:?}"
                );
            }
            other => panic!("unexpected error {other:?}"),
        }
    }
}
