//! Opt-in production-scale listing, pane sorting and frame benchmark.
#![warn(missing_docs)]
use ratatui::{backend::TestBackend, Terminal};
use scpto::{
    preview::Background,
    state::{Sort, SortKey},
    ui::{
        action::{Action, RequestId},
        app::App,
        render,
    },
    vfs::{local::LocalFs, Endpoint, Fs},
};
use std::time::{Duration, Instant};

#[tokio::test]
#[ignore = "creates 100,000 files; run cargo test --release --test benchmark -- --ignored --nocapture"]
async fn hundred_thousand_entries() {
    let temp = tempfile::tempdir().unwrap();
    for index in 0..100_000 {
        std::fs::File::create(temp.path().join(format!("entry-{index:06}.txt"))).unwrap();
    }
    let start = Instant::now();
    let entries = LocalFs::new().list(temp.path()).await.unwrap();
    let listing = start.elapsed();
    assert_eq!(entries.len(), 100_000);
    let (mut app, _) = App::new(
        Default::default(),
        Default::default(),
        Default::default(),
        Default::default(),
        Background::Dark,
        (Endpoint::Local, Some(temp.path().into())),
        Some((Endpoint::Local, Some(temp.path().into()))),
    );
    app.handle_action(Action::Resize {
        width: 160,
        height: 45,
    });
    let pane = app.core.pane_mut(scpto::ui::action::PaneSide::Left);
    pane.begin_list(RequestId(1), temp.path().into(), None);
    let start = Instant::now();
    assert!(pane.finish_list(RequestId(1), Ok(entries)).unwrap());
    let load = start.elapsed();
    let start = Instant::now();
    pane.set_sort(Sort::by(SortKey::Name).reversed(), 40);
    let sort = start.elapsed();
    assert_eq!(pane.row_count(), 100_001);
    let mut terminal = Terminal::new(TestBackend::new(160, 45)).unwrap();
    let mut frames = Vec::new();
    for _ in 0..20 {
        let start = Instant::now();
        terminal
            .draw(|frame| {
                render::draw(frame, &mut app);
            })
            .unwrap();
        frames.push(start.elapsed());
    }
    frames.sort();
    let frame = frames[19];
    eprintln!("100k benchmark: listing={listing:?}, pane_load={load:?}, sort={sort:?}, frame_p50={:?}, frame_max={frame:?}", frames[10]);
    if !cfg!(debug_assertions) {
        assert!(listing < Duration::from_secs(3), "listing: {listing:?}");
        assert!(
            frame < Duration::from_millis(16),
            "slowest of 20 frames: {frame:?}"
        );
    }
}
