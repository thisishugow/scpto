//! Executes `Effect::RememberDirectory`, `Effect::RememberSort`,
//! `Effect::SaveBookmark`, and `Effect::RemoveBookmark` using `state`.
//!
//! # Behavior
//!
//! Each entry point spawns a task that locks `ctx.persisted`, applies the
//! change (`PersistedState::remember_directory`, `remember_sort`,
//! `add_bookmark`, `remove_bookmark`), and, when `ctx.state_dir` is set, saves the whole
//! value with `state::save_state` in `spawn_blocking` while still holding
//! the lock, so saves never overtake each other. A failed save is sent as
//! `StateWriteFailed`. Without a state directory nothing is saved (startup
//! already said so in the status bar).
//!
//! # Public API (Wave 3, ui/effects/state)
//!
//! ```text
//! pub fn remember_directory(ctx: &Ctx, location: Location);
//! pub fn remember_sort(ctx: &Ctx, endpoint: Endpoint, sort: Sort);
//! pub fn save_bookmark(ctx: &Ctx, location: Location);
//! pub fn remove_bookmark(ctx: &Ctx, location: Location);
//! ```

use super::Ctx;
use crate::error::Error;
use crate::state::{self, PersistedState, Sort};
use crate::ui::action::BgEvent;
use crate::vfs::{Endpoint, Location};

/// Remembers `location` as its endpoint's last directory and saves the
/// state (`Effect::RememberDirectory`).
pub fn remember_directory(ctx: &Ctx, location: Location) {
    change(ctx, move |persisted| persisted.remember_directory(location));
}

/// Remembers `sort` as its endpoint's last pane sort and saves the state
/// (`Effect::RememberSort`).
pub fn remember_sort(ctx: &Ctx, endpoint: Endpoint, sort: Sort) {
    change(ctx, move |persisted| {
        persisted.remember_sort(endpoint, sort)
    });
}

/// Adds a bookmark for `location` unless present and saves the state
/// (`Effect::SaveBookmark`).
pub fn save_bookmark(ctx: &Ctx, location: Location) {
    change(ctx, move |persisted| {
        persisted.add_bookmark(location);
    });
}

/// Removes the bookmark for `location` if present and saves the state
/// (`Effect::RemoveBookmark`).
pub fn remove_bookmark(ctx: &Ctx, location: Location) {
    change(ctx, move |persisted| {
        persisted.remove_bookmark(&location);
    });
}

/// Applies `apply` to the executor's copy of `state.json` and saves the
/// whole value under the lock; a failed save is sent as `StateWriteFailed`.
fn change<F>(ctx: &Ctx, apply: F)
where
    F: FnOnce(&mut PersistedState) + Send + 'static,
{
    let persisted = ctx.persisted.clone();
    let state_dir = ctx.state_dir.clone();
    let events = ctx.events.clone();
    tokio::spawn(async move {
        let mut persisted = persisted.lock().await;
        apply(&mut persisted);
        let Some(dir) = state_dir else {
            return;
        };
        let snapshot = persisted.clone();
        let path = dir.clone();
        let saved = tokio::task::spawn_blocking(move || state::save_state(&dir, &snapshot))
            .await
            .unwrap_or_else(|error| {
                Err(Error::StateWrite {
                    path,
                    message: error.to_string(),
                })
            });
        // The guard is released only after the save, so the next change
        // saves a value that includes this one.
        drop(persisted);
        if let Err(error) = saved {
            if let Err(unsent) = events.send(BgEvent::StateWriteFailed(error)) {
                // The app has exited; nobody is left to report to.
                drop(unsent);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::super::session::testing::Harness;
    use super::*;
    use crate::connections::SshClient;
    use crate::state::{EndpointSort, SortKey};

    fn local(path: &str) -> Location {
        Location {
            endpoint: Endpoint::Local,
            path: PathBuf::from(path),
        }
    }

    fn remote(host: &str, path: &str) -> Location {
        Location {
            endpoint: Endpoint::Remote {
                host: host.into(),
                sudo: false,
            },
            path: PathBuf::from(path),
        }
    }

    /// Waits until `state.json` in `dir` holds `expected`.
    async fn saved_state_becomes(dir: &Path, expected: &PersistedState) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let (loaded, warning) = state::load_state(dir);
            if &loaded == expected {
                assert_eq!(warning, None);
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "saved {loaded:?}, expected {expected:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn changes_are_applied_and_saved_in_order() {
        let mut harness = Harness::new(SshClient::default()).await;
        let dir = harness.ctx.state_dir.clone().unwrap();

        remember_directory(&harness.ctx, local("/a"));
        remember_directory(&harness.ctx, remote("web", "/srv"));
        remember_directory(&harness.ctx, local("/b"));
        save_bookmark(&harness.ctx, local("/x"));
        save_bookmark(&harness.ctx, remote("web", "/y"));
        save_bookmark(&harness.ctx, local("/x"));
        remember_sort(&harness.ctx, Endpoint::Local, Sort::by(SortKey::Size));
        let expected = PersistedState {
            last_directories: vec![local("/b"), remote("web", "/srv")],
            bookmarks: vec![local("/x"), remote("web", "/y")],
            sorts: vec![EndpointSort {
                endpoint: Endpoint::Local,
                sort: Sort::by(SortKey::Size),
            }],
        };
        saved_state_becomes(&dir, &expected).await;
        assert_eq!(*harness.ctx.persisted.lock().await, expected);

        remove_bookmark(&harness.ctx, local("/x"));
        remove_bookmark(&harness.ctx, local("/not-a-bookmark"));
        let expected = PersistedState {
            bookmarks: vec![remote("web", "/y")],
            ..expected
        };
        saved_state_becomes(&dir, &expected).await;
        harness.expect_quiet(Duration::from_millis(100)).await;
        harness.finish().await;
    }

    #[tokio::test]
    async fn many_quick_changes_all_reach_the_file() {
        let harness = Harness::new(SshClient::default()).await;
        let dir = harness.ctx.state_dir.clone().unwrap();
        let bookmarks: Vec<Location> = (0..50).map(|n| local(&format!("/dir{n}"))).collect();
        for bookmark in &bookmarks {
            save_bookmark(&harness.ctx, bookmark.clone());
        }
        // Spawned tasks may lock in any order, so compare as sets.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let (loaded, _) = state::load_state(&dir);
            let mut saved = loaded.bookmarks.clone();
            saved.sort_by(|a, b| a.path.cmp(&b.path));
            let mut wanted = bookmarks.clone();
            wanted.sort_by(|a, b| a.path.cmp(&b.path));
            if saved == wanted {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "saved {} of 50",
                saved.len()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        harness.finish().await;
    }

    #[tokio::test]
    async fn a_failed_save_is_reported_and_the_change_kept() {
        let mut harness = Harness::new(SshClient::default()).await;
        // A regular file where the state directory should be.
        let blocker = harness.scratch.path().join("not-a-directory");
        std::fs::write(&blocker, b"").unwrap();
        harness.ctx.state_dir = Some(blocker);
        save_bookmark(&harness.ctx, local("/x"));
        match harness.next().await {
            BgEvent::StateWriteFailed(error) => {
                assert!(matches!(error, Error::StateWrite { .. }), "{error:?}");
            }
            other => panic!("expected StateWriteFailed, got {other:?}"),
        }
        assert_eq!(harness.ctx.persisted.lock().await.bookmarks, [local("/x")]);
        harness.finish().await;
    }

    #[tokio::test]
    async fn without_a_state_directory_changes_stay_in_memory() {
        let mut harness = Harness::new(SshClient::default()).await;
        harness.ctx.state_dir = None;
        remember_directory(&harness.ctx, local("/a"));
        save_bookmark(&harness.ctx, local("/x"));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            {
                let persisted = harness.ctx.persisted.lock().await;
                if persisted.bookmarks == [local("/x")] {
                    assert_eq!(persisted.last_directories, [local("/a")]);
                    break;
                }
            }
            assert!(tokio::time::Instant::now() < deadline, "change not applied");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        harness.expect_quiet(Duration::from_millis(100)).await;
        assert!(std::fs::read_dir(harness.scratch.path())
            .unwrap()
            .next()
            .is_none());
        harness.finish().await;
    }
}
