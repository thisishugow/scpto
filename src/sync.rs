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

use std::path::PathBuf;

use crate::vfs::{Entry, Location};

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
