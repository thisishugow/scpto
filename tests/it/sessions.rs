//! Session lifecycle, channel independence, and real sudo authorization.
use super::support::*;
use scpto::{
    connections::{Connections, FsChannel},
    error::{Error, SudoError},
    ui::action::{BgEvent, Secret},
    vfs::FsError,
};
use std::{path::Path, sync::Arc};
use zeroize::Zeroizing;

#[test]
fn modern() {
    isolated("sessions::modern", || lifecycle("modern"));
}
#[test]
fn legacy() {
    isolated("sessions::legacy", || lifecycle("legacy"));
}

async fn lifecycle(image: &str) {
    let mut f = Fixture::new();
    let ep = endpoint(image, "plain", false);
    f.connect(&ep).await;
    let browse = f.sessions.fs(&ep, FsChannel::Browse).await.unwrap();
    let transfer = f.fs(&ep).await;
    assert!(!Arc::ptr_eq(&browse, &transfer));
    let root = f.root(&ep, "session").await;
    let nested = root.join("nested");
    browse.mkdir(&nested).await.unwrap();
    write(transfer.as_ref(), &nested.join("original"), b"two channels").await;
    browse
        .rename(&nested.join("original"), &nested.join("renamed"))
        .await
        .unwrap();
    let (left, right) = tokio::join!(browse.list(&nested), transfer.list(&nested));
    assert_eq!(left.unwrap(), right.unwrap());
    assert_eq!(
        browse.stat(&nested.join("renamed")).await.unwrap().size,
        Some(12)
    );
    assert!(browse
        .list(&root)
        .await
        .unwrap()
        .iter()
        .all(|e| e.name != "." && e.name != ".."));
    let tools = f.sessions.tools(&ep).await.unwrap();
    assert!(tools.tar && tools.rsync_ok);
    assert_eq!(transfer.capabilities().copy_within, image == "modern");
    f.kill_master(&ep).await;
    f.reconnected(&ep).await;
    assert!(matches!(
        browse.list(&root).await,
        Err(FsError::Disconnected)
    ));
    let fresh = f.sessions.fs(&ep, FsChannel::Browse).await.unwrap();
    assert!(!Arc::ptr_eq(&browse, &fresh));
    assert_eq!(
        read(fresh.as_ref(), &nested.join("renamed")).await,
        b"two channels"
    );
    f.fs(&ep)
        .await
        .remove_file(&nested.join("renamed"))
        .await
        .unwrap();
    fresh.remove_dir(&nested).await.unwrap();
    assert!(fresh.list(&root).await.unwrap().is_empty());
    f.finish().await;
}

#[test]
fn sudo_modern() {
    isolated("sessions::sudo_modern", || sudo("modern"));
}
#[test]
fn sudo_legacy() {
    isolated("sessions::sudo_legacy", || sudo("legacy"));
}

async fn sudo(image: &str) {
    let mut f = Fixture::new();
    for (user, expected) in [
        ("sudotty", SudoError::RequiresTty),
        ("plain", SudoError::NotInSudoers),
    ] {
        let ep = endpoint(image, user, true);
        f.sessions.connect(ep.clone());
        match f.next().await {
            BgEvent::ConnectFailed {
                endpoint,
                error: Error::Sudo(error),
            } => {
                assert_eq!(endpoint, ep);
                assert_eq!(error, expected);
            }
            event => panic!("sudo classification for {user}: {event:?}"),
        }
    }
    let ep = endpoint(image, "sudopw", true);
    f.sessions.connect(ep.clone());
    assert!(matches!(f.next().await, BgEvent::SudoPasswordRequired(got) if got == ep));
    f.sessions.sudo_password(
        ep.clone(),
        Secret::new(Zeroizing::new("wrong-password".into())),
    );
    match f.next().await {
        BgEvent::ConnectFailed {
            error: Error::Sudo(SudoError::WrongPassword),
            ..
        } => {}
        event => panic!("wrong password: {event:?}"),
    }
    f.sessions.connect(ep.clone());
    assert!(matches!(f.next().await, BgEvent::SudoPasswordRequired(got) if got == ep));
    f.sessions.sudo_password(
        ep.clone(),
        Secret::new(Zeroizing::new("scpto-test-pw".into())),
    );
    assert!(matches!(f.next().await, BgEvent::Connected { endpoint, .. } if endpoint == ep));
    for ep in [ep, endpoint(image, "sudonopw", true)] {
        if ep.to_string().contains("sudonopw") {
            f.connect(&ep).await;
        }
        let browse = f.sessions.fs(&ep, FsChannel::Browse).await.unwrap();
        let transfer = f.fs(&ep).await;
        assert!(
            Arc::ptr_eq(&browse, &transfer),
            "sudo shares one authenticated channel"
        );
        let path = Path::new("/srv/rootonly").join(format!("scpto-it-{}", std::process::id()));
        write(transfer.as_ref(), &path, b"root write").await;
        assert_eq!(read(browse.as_ref(), &path).await, b"root write");
        transfer.remove_file(&path).await.unwrap();
    }
    f.finish().await;
}
