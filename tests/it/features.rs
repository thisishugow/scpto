//! Remote edit, preview and directory synchronization through public APIs.
use super::support::*;
use scpto::{
    connections::{Connections, FsChannel},
    edit::{self, EditOutcome},
    preview::{self, PreviewData},
    sync::{self, DiffStatus, SyncDirection},
    vfs::Endpoint,
};
use std::{os::unix::fs::PermissionsExt, path::PathBuf, time::Instant};

#[test]
fn modern() {
    isolated("features::modern", || features("modern"));
}
#[test]
fn legacy() {
    isolated("features::legacy", || features("legacy"));
}

async fn features(image: &str) {
    let mut f = Fixture::new();
    let ep = endpoint(image, "plain", false);
    f.connect(&ep).await;
    let root = f.root(&ep, "features").await;
    let browse = f.sessions.fs(&ep, FsChannel::Browse).await.unwrap();
    let transfer = f.fs(&ep).await;
    let path = root.join("readme.md");
    write(transfer.as_ref(), &path, b"# Original\n").await;
    let picker = ratatui_image::picker::Picker::halfblocks();
    match preview::load(browse.as_ref(), &path, &picker)
        .await
        .unwrap()
    {
        PreviewData::Text { bytes, truncated } => {
            assert_eq!(bytes, b"# Original\n");
            assert!(!truncated);
        }
        other => panic!("text preview: {other:?}"),
    }
    let session = edit::prepare(
        browse.as_ref(),
        location(&ep, &path),
        &f.temp.path().join("edits"),
    )
    .await
    .unwrap();
    let edit_file = session.file.clone();
    assert_eq!(
        std::fs::metadata(&edit_file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let stub = f.temp.path().join("stub editor");
    std::fs::write(&stub, "#!/bin/sh\nprintf '# Edited\\n' > \"$1\"\n").unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
    let editor = format!("'{}'", stub.display());
    let start = Instant::now();
    assert!(
        edit::editor_command(None, Some(editor.as_ref()), &edit_file)
            .unwrap()
            .status()
            .await
            .unwrap()
            .success()
    );
    assert_eq!(
        edit::finish(session, transfer.as_ref(), start.elapsed())
            .await
            .unwrap(),
        EditOutcome::Uploaded
    );
    assert_eq!(read(browse.as_ref(), &path).await, b"# Edited\n");
    assert!(!edit_file.exists());

    let local_dir = f.temp.path().join("sync");
    std::fs::create_dir(&local_dir).unwrap();
    std::fs::write(local_dir.join("#selected.txt"), "sync this file").unwrap();
    std::fs::write(local_dir.join("untouched.txt"), "not selected").unwrap();
    let left = location(&Endpoint::Local, &local_dir);
    let right = location(&ep, &root);
    let local = f.fs(&Endpoint::Local).await;
    let comparison = sync::compare(
        local.as_ref(),
        browse.as_ref(),
        left.clone(),
        right.clone(),
        0,
    )
    .await
    .unwrap();
    assert!(comparison
        .entries
        .iter()
        .any(|e| e.relative == std::path::Path::new("#selected.txt")
            && e.status == DiffStatus::LeftOnly));
    let paths = vec![PathBuf::from("#selected.txt")];
    let tools = f.sessions.tools(&ep).await.unwrap();
    let local_ok = sync::local_rsync_ok().await;
    if sync::rsync_eligible(&left.endpoint, &right.endpoint, tools, local_ok) {
        let ssh = f.sessions.ssh_command(&ep, false).await.unwrap();
        sync::run_rsync(
            &ssh,
            SyncDirection::LeftToRight,
            &left,
            &right,
            &paths,
            false,
        )
        .await
        .unwrap();
        std::fs::remove_file(local_dir.join("#selected.txt")).unwrap();
        sync::run_rsync(
            &ssh,
            SyncDirection::RightToLeft,
            &left,
            &right,
            &paths,
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read(local_dir.join("#selected.txt")).unwrap(),
            b"sync this file"
        );
    } else {
        assert!(
            (local_ok && tools.rsync_ok) || std::env::consts::OS != "linux",
            "Linux CI must install qualifying rsync >= 3.1 on both sides"
        );
        eprintln!("SKIP rsync {image}: local openrsync/rsync < 3.1 does not qualify; exercising engine sync");
        let mut engine = Engine::new(&f, usize::MAX);
        let (summary, _) = engine
            .run(
                sync::engine_job(
                    scpto::transfer::model::JobId(1),
                    SyncDirection::LeftToRight,
                    &comparison,
                    &paths,
                ),
                None,
            )
            .await;
        assert_eq!(summary.done, 1);
        engine.finish().await;
    }
    assert_eq!(
        read(browse.as_ref(), &root.join("#selected.txt")).await,
        b"sync this file"
    );
    assert!(matches!(
        browse.stat(&root.join("untouched.txt")).await,
        Err(scpto::vfs::FsError::NotFound { .. })
    ));
    f.finish().await;
}
