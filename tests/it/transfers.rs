//! Transfers over real SFTP channels, including resume and archive mode.
use super::support::*;
use scpto::{
    state::{load_partials, PartialStore},
    transfer::{model::*, stream::part_path},
    vfs::{Endpoint, FsError},
};
use std::{
    os::unix::{fs::symlink, net::UnixListener},
    path::Path,
};

#[test]
fn modern() {
    isolated("transfers::modern", || roundtrip("modern", "legacy"));
}
#[test]
fn legacy() {
    isolated("transfers::legacy", || roundtrip("legacy", "modern"));
}

async fn roundtrip(image: &str, other: &str) {
    let mut f = Fixture::new();
    let ep = endpoint(image, "plain", false);
    let peer = endpoint(other, "plain", false);
    f.connect(&ep).await;
    f.connect(&peer).await;
    let root = f.root(&ep, "transfer").await;
    let peer_root = f.root(&peer, "relay").await;
    let local = Endpoint::Local;
    let local_fs = f.fs(&local).await;
    let content: Vec<u8> = (0..1_048_576).map(|i| (i % 251) as u8).collect();
    let source = f.temp.path().join("payload");
    write(local_fs.as_ref(), &source, &content).await;
    let mut engine = Engine::new(&f, usize::MAX);
    let download = f.temp.path().join("download");
    std::fs::create_dir(&download).unwrap();
    for (id, src_ep, dst_ep, src, dir, kill) in [
        (
            1,
            local.clone(),
            ep.clone(),
            source,
            root.clone(),
            ep.clone(),
        ),
        (
            2,
            ep.clone(),
            peer.clone(),
            root.join("payload"),
            peer_root.clone(),
            peer.clone(),
        ),
        (
            3,
            peer.clone(),
            local.clone(),
            peer_root.join("payload"),
            download.clone(),
            peer.clone(),
        ),
    ] {
        let src_fs = f.fs(&src_ep).await;
        let dst_fs = f.fs(&dst_ep).await;
        let dst = dir.join("payload");
        let attrs = src_fs.lstat(&src).await.unwrap();
        let record = PartialRecord {
            source_size: attrs.size.unwrap(),
            source_mtime: attrs.mtime.unwrap(),
        };
        let destination = location(&dst_ep, &dst);
        write(dst_fs.as_ref(), &part_path(&dst), &content[..131_072]).await;
        engine
            .partials
            .set(destination.clone(), record)
            .await
            .unwrap();
        assert_eq!(
            load_partials(&f.temp.path().join("state"))
                .unwrap()
                .get(&destination),
            Some(record)
        );
        f.kill_master(&kill).await;
        f.reconnected(&kill).await;
        let (summary, _) = engine
            .run(job(id, &src_ep, &dst_ep, vec![src], &dir), None)
            .await;
        assert_eq!(summary.done, 1);
        let fresh = f.fs(&dst_ep).await;
        assert_eq!(read(fresh.as_ref(), &dst).await, content);
        assert!(matches!(
            fresh.lstat(&part_path(&dst)).await,
            Err(FsError::NotFound { .. })
        ));
        assert_eq!(engine.partials.get(&destination), None);
        assert!(load_partials(&f.temp.path().join("state"))
            .unwrap()
            .entries
            .is_empty());
    }
    let remote_fs = f.fs(&ep).await;
    let copied_dir = root.join("same-host-copy");
    remote_fs.mkdir(&copied_dir).await.unwrap();
    remote_fs
        .chmod(&root.join("payload"), 0o6755)
        .await
        .unwrap();
    assert_eq!(remote_fs.capabilities().copy_within, image == "modern");
    let (summary, _) = engine
        .run(
            job(4, &ep, &ep, vec![root.join("payload")], &copied_dir),
            None,
        )
        .await;
    assert_eq!(summary.done, 1);
    assert_eq!(
        read(remote_fs.as_ref(), &copied_dir.join("payload")).await,
        content
    );
    assert_eq!(
        remote_fs
            .stat(&copied_dir.join("payload"))
            .await
            .unwrap()
            .mode
            .unwrap()
            & 0o7777,
        0o755
    );
    engine.finish().await;
    f.finish().await;
}

#[test]
fn conflicts_modern() {
    isolated("transfers::conflicts_modern", || conflicts("modern"));
}
#[test]
fn conflicts_legacy() {
    isolated("transfers::conflicts_legacy", || conflicts("legacy"));
}

async fn conflicts(image: &str) {
    let mut f = Fixture::new();
    let ep = endpoint(image, "plain", false);
    f.connect(&ep).await;
    let root = f.root(&ep, "conflicts").await;
    let fs = f.fs(&ep).await;
    let local = f.fs(&Endpoint::Local).await;
    let src = f.temp.path().join("entry.txt");
    write(local.as_ref(), &src, b"new contents").await;
    // Relative metadata avoids wall-clock assumptions and timestamp granularity.
    let mtime = local.stat(&src).await.unwrap().mtime.unwrap();
    let mut engine = Engine::new(&f, usize::MAX);
    for (index, choice) in [
        ConflictChoice::Overwrite,
        ConflictChoice::Skip,
        ConflictChoice::Rename,
        ConflictChoice::OverwriteIfNewer,
        ConflictChoice::Resume,
    ]
    .into_iter()
    .enumerate()
    {
        let dst = root.join("entry.txt");
        write(fs.as_ref(), &dst, b"original").await;
        fs.set_times(&dst, mtime - 10, mtime - 10).await.unwrap();
        if choice == ConflictChoice::Resume {
            write(fs.as_ref(), &part_path(&dst), b"new ").await;
            engine
                .partials
                .set(
                    location(&ep, &dst),
                    PartialRecord {
                        source_size: 12,
                        source_mtime: mtime,
                    },
                )
                .await
                .unwrap();
        }
        let (summary, events) = engine
            .run(
                job(
                    index as u64,
                    &Endpoint::Local,
                    &ep,
                    vec![src.clone()],
                    &root,
                ),
                Some(choice),
            )
            .await;
        let prompts: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                TransferEvent::ConflictRaised(conflict) => Some(conflict),
                _ => None,
            })
            .collect();
        assert_eq!(prompts.len(), 1, "the real conflict must reach the caller");
        if choice == ConflictChoice::Resume {
            assert_eq!(prompts[0].partial.unwrap().size, 4);
        }
        assert_eq!(summary.skipped, usize::from(choice == ConflictChoice::Skip));
        let expected: &[u8] = if matches!(choice, ConflictChoice::Skip | ConflictChoice::Rename) {
            b"original"
        } else {
            b"new contents"
        };
        assert_eq!(read(fs.as_ref(), &dst).await, expected);
        if choice == ConflictChoice::Rename {
            assert_eq!(
                read(fs.as_ref(), &root.join("entry (1).txt")).await,
                b"new contents"
            );
        }
    }
    let dst = root.join("entry.txt");
    write(fs.as_ref(), &dst, b"keep newer destination").await;
    fs.set_times(&dst, mtime + 10, mtime + 10).await.unwrap();
    let (summary, events) = engine
        .run(
            job(10, &Endpoint::Local, &ep, vec![src], &root),
            Some(ConflictChoice::OverwriteIfNewer),
        )
        .await;
    assert_eq!(summary.skipped, 1);
    assert!(events.iter().any(|e| matches!(
        e,
        TransferEvent::ItemChanged {
            state: ItemState::Skipped {
                reason: SkipReason::NotNewer
            },
            ..
        }
    )));
    assert_eq!(read(fs.as_ref(), &dst).await, b"keep newer destination");
    engine.finish().await;
    f.finish().await;
}

#[test]
fn trees_modern() {
    isolated("transfers::trees_modern", || trees("modern"));
}
#[test]
fn trees_legacy() {
    isolated("transfers::trees_legacy", || trees("legacy"));
}

async fn trees(image: &str) {
    let mut f = Fixture::new();
    let ep = endpoint(image, "plain", false);
    f.connect(&ep).await;
    let root = f.root(&ep, "trees").await;
    let fs = f.fs(&ep).await;
    let tree = f.temp.path().join("small files");
    std::fs::create_dir(&tree).unwrap();
    for i in 0..200 {
        std::fs::write(tree.join(format!("file-{i:03}")), format!("contents {i}\n")).unwrap();
    }
    let unicode = ["\u{7a7a}\u{767d} \u{6a94}\u{6848}.txt", "cafe\u{301}.txt"];
    for name in unicode {
        std::fs::write(tree.join(name), name.as_bytes()).unwrap();
    }
    symlink("file-000", tree.join("link")).unwrap();
    let mut engine = Engine::new(&f, 200);
    let (summary, events) = engine
        .run(
            job(1, &Endpoint::Local, &ep, vec![tree.clone()], &root),
            None,
        )
        .await;
    assert_eq!(summary.failed, 0);
    assert!(events.iter().any(|e| matches!(
        e,
        TransferEvent::JobCreated {
            mode: TransferMode::Tar,
            ..
        }
    )));
    let remote = root.join("small files");
    for i in 0..200 {
        assert_eq!(
            read(fs.as_ref(), &remote.join(format!("file-{i:03}"))).await,
            format!("contents {i}\n").as_bytes()
        );
    }
    for name in unicode {
        assert_eq!(read(fs.as_ref(), &remote.join(name)).await, name.as_bytes());
    }
    assert_eq!(
        fs.read_link(&remote.join("link")).await.unwrap(),
        Path::new("file-000")
    );
    let download = f.temp.path().join("archive download");
    std::fs::create_dir(&download).unwrap();
    let (_, events) = engine
        .run(job(2, &ep, &Endpoint::Local, vec![remote], &download), None)
        .await;
    assert!(events.iter().any(|e| matches!(
        e,
        TransferEvent::JobCreated {
            mode: TransferMode::Tar,
            ..
        }
    )));
    for entry in std::fs::read_dir(&tree).unwrap() {
        let entry = entry.unwrap();
        let copied = download.join("small files").join(entry.file_name());
        if entry.file_type().unwrap().is_symlink() {
            assert_eq!(std::fs::read_link(copied).unwrap(), Path::new("file-000"));
        } else {
            assert_eq!(
                std::fs::read(copied).unwrap(),
                std::fs::read(entry.path()).unwrap()
            );
        }
    }
    std::fs::write(tree.join("file-000"), b"replacement").unwrap();
    for (id, choice) in [
        (4, TarConflictChoice::Cancel),
        (5, TarConflictChoice::OverwriteAll),
    ] {
        engine
            .commands
            .send(TransferCommand::Submit(job(
                id,
                &Endpoint::Local,
                &ep,
                vec![tree.clone()],
                &root,
            )))
            .unwrap();
        loop {
            match engine.next().await {
                TransferEvent::TarConflictRaised { job, existing } => {
                    assert_eq!(existing, vec![root.join("small files")]);
                    engine
                        .commands
                        .send(TransferCommand::ResolveTarConflict { job, choice })
                        .unwrap();
                    break;
                }
                TransferEvent::JobFinished { .. } => {
                    panic!("tar overwrite must require a decision")
                }
                _ => {}
            }
        }
        let (summary, _) = engine.complete(None).await;
        if choice == TarConflictChoice::Cancel {
            assert!(summary.skipped > 0);
            assert_eq!(
                read(fs.as_ref(), &root.join("small files/file-000")).await,
                b"contents 0\n"
            );
        } else {
            assert_eq!(summary.skipped, 0);
            assert_eq!(
                read(fs.as_ref(), &root.join("small files/file-000")).await,
                b"replacement"
            );
        }
    }
    engine.finish().await;
    // Specials are exercised in the per-file engine, including a live socket.
    let specials = f.temp.path().join("specials");
    std::fs::create_dir(&specials).unwrap();
    assert!(std::process::Command::new("mkfifo")
        .arg(specials.join("fifo"))
        .status()
        .unwrap()
        .success());
    let _socket = UnixListener::bind(specials.join("socket")).unwrap();
    symlink("missing", specials.join("dangling")).unwrap();
    for name in unicode {
        std::fs::write(specials.join(name), name.as_bytes()).unwrap();
    }
    let mut engine = Engine::new(&f, usize::MAX);
    let (summary, events) = engine
        .run(job(3, &Endpoint::Local, &ep, vec![specials], &root), None)
        .await;
    assert_eq!(summary.skipped, 2);
    assert!(events.iter().any(|e| matches!(e, TransferEvent::ItemsAdded { items, .. } if items.iter().filter(|i| matches!(i.state, ItemState::Skipped { reason: SkipReason::SpecialFile })).count() == 2)));
    assert_eq!(
        fs.read_link(&root.join("specials/dangling")).await.unwrap(),
        Path::new("missing")
    );
    assert_eq!(fs.list(&root.join("specials")).await.unwrap().len(), 3);
    for name in unicode {
        assert_eq!(
            read(fs.as_ref(), &root.join("specials").join(name)).await,
            name.as_bytes()
        );
    }
    engine.finish().await;
    f.finish().await;
}

#[test]
fn interrupted_modern() {
    isolated("transfers::interrupted_modern", || interrupted("modern"));
}

#[test]
#[ignore = "bug: killing the legacy master mid-upload can resume a corrupt partial and report success"]
fn interrupted_legacy() {
    isolated("transfers::interrupted_legacy", || interrupted("legacy"));
}

async fn interrupted(image: &str) {
    let mut f = Fixture::new();
    let ep = endpoint(image, "plain", false);
    f.connect(&ep).await;
    let root = f.root(&ep, "interrupted").await;
    let source = f.temp.path().join("large.bin");
    // A patterned payload catches wrong offsets and duplicated or missing chunks.
    let content: Vec<u8> = (0..32 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    std::fs::write(&source, &content).unwrap();
    let mut engine = Engine::new(&f, usize::MAX);
    engine
        .commands
        .send(TransferCommand::Submit(job(
            1,
            &Endpoint::Local,
            &ep,
            vec![source],
            &root,
        )))
        .unwrap();
    loop {
        match engine.next().await {
            TransferEvent::ItemChanged {
                state: ItemState::Running { bytes },
                ..
            } if bytes > 0 && bytes < content.len() as u64 => break,
            TransferEvent::JobFinished { .. } => {
                panic!("transfer finished before interruption could be injected")
            }
            _ => {}
        }
    }
    f.kill_master(&ep).await;
    loop {
        match engine.next().await {
            TransferEvent::ItemChanged {
                state: ItemState::WaitingForReconnect,
                ..
            } => break,
            TransferEvent::JobFinished { .. } => {
                panic!("interrupted transfer must wait for reconnect")
            }
            _ => {}
        }
    }
    f.reconnected(&ep).await;
    let dst = root.join("large.bin");
    let fs = f.fs(&ep).await;
    let part_size = fs.stat(&part_path(&dst)).await.unwrap().size.unwrap();
    assert!(part_size > 0 && part_size < content.len() as u64);
    assert_eq!(
        engine
            .partials
            .get(&location(&ep, &dst))
            .unwrap()
            .source_size,
        content.len() as u64
    );
    engine
        .commands
        .send(TransferCommand::EndpointReconnected(ep.clone()))
        .unwrap();
    let (summary, _) = engine.complete(None).await;
    assert_eq!(summary.done, 1);
    let actual = read(fs.as_ref(), &dst).await;
    let mismatch = actual.iter().zip(&content).position(|(a, b)| a != b);
    assert!(actual == content, "resumed content differs: length {} vs {}, first mismatch {mismatch:?}, partial size {part_size}", actual.len(), content.len());
    assert!(engine.partials.get(&location(&ep, &dst)).is_none());
    assert!(matches!(
        fs.stat(&part_path(&dst)).await,
        Err(FsError::NotFound { .. })
    ));
    engine.finish().await;
    f.finish().await;
}
