//! Executes `Effect::LoadPreview` using `preview`, and runs the image
//! resize worker that answers with `BgEvent::ImageResized`.
//!
//! # Behavior
//!
//! - `load` runs under `ctx.requests` (a superseded load is aborted by
//!   `CancelRequest`): `Connections::fs(endpoint, Browse)` (never the
//!   transfer channel, plan 8.4), then `preview::load(&*fs, &path,
//!   &ctx.image_picker)`, answered with `PreviewLoaded`.
//! - `spawn_resize_worker` starts one `std::thread` that receives
//!   `ResizeRequest`s from the app's image protocols, runs `preview::resize`
//!   on each, and sends `ImageResized`; it ends when every sender is gone
//!   (the app was dropped) or the event channel is closed.
//!
//! # Public API (Wave 3, ui/effects/preview)
//!
//! ```text
//! pub fn load(ctx: &Ctx, request: RequestId, location: Location);
//! pub fn spawn_resize_worker(
//!     requests: std::sync::mpsc::Receiver<ratatui_image::thread::ResizeRequest>,
//!     events: UnboundedSender<BgEvent>,
//! );
//! ```

use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use ratatui_image::picker::Picker;
use ratatui_image::thread::ResizeRequest;
use tokio::sync::mpsc::UnboundedSender;

use super::{Ctx, Tasks};
use crate::connections::{Connections, FsChannel};
use crate::error::Error;
use crate::preview::{self, PreviewData};
use crate::ui::action::{BgEvent, RequestId};
use crate::vfs::Location;

/// Loads the preview of `location` under `request` (cancelable with
/// `Effect::CancelRequest`) and answers with `PreviewLoaded`. Must be called
/// inside a Tokio runtime.
pub fn load(ctx: &Ctx, request: RequestId, location: Location) {
    spawn_load(
        &ctx.requests,
        Arc::clone(&ctx.connections),
        ctx.events.clone(),
        Arc::clone(&ctx.image_picker),
        request,
        location,
    );
}

/// Starts the image resize worker thread; it runs until `requests` has no
/// senders left or `events` is closed.
pub fn spawn_resize_worker(requests: Receiver<ResizeRequest>, events: UnboundedSender<BgEvent>) {
    // The worker is never joined: it ends on its own with the app.
    drop(start_resize_worker(requests, events));
}

/// `load` without the context.
fn spawn_load(
    requests: &Arc<Tasks<RequestId>>,
    connections: Arc<dyn Connections>,
    events: UnboundedSender<BgEvent>,
    picker: Arc<Picker>,
    request: RequestId,
    location: Location,
) {
    requests.spawn(request, async move {
        let result = load_preview(&*connections, &location, &picker).await;
        let event = BgEvent::PreviewLoaded {
            request,
            location,
            result,
        };
        if let Err(unsent) = events.send(event) {
            // The app has exited; nobody is left to show the preview.
            drop(unsent);
        }
    });
}

/// The preview of `location`, read over its browsing channel.
async fn load_preview(
    connections: &dyn Connections,
    location: &Location,
    picker: &Picker,
) -> Result<PreviewData, Error> {
    let fs = connections
        .fs(&location.endpoint, FsChannel::Browse)
        .await?;
    preview::load(&*fs, &location.path, picker).await
}

/// `spawn_resize_worker`, returning the thread's handle.
fn start_resize_worker(
    requests: Receiver<ResizeRequest>,
    events: UnboundedSender<BgEvent>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("scpto-image-resize".into())
        .spawn(move || {
            while let Ok(request) = requests.recv() {
                let resized = BgEvent::ImageResized(preview::resize(request));
                if events.send(resized).is_err() {
                    // The app has exited; stop resizing for it.
                    break;
                }
            }
        })
        // `thread::spawn` fails the same way: without a thread at startup
        // no image could ever be shown.
        .expect("the operating system starts the image resize thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::mpsc;
    use std::time::Duration;

    use image::{DynamicImage, Rgba, RgbaImage};
    use ratatui::layout::Size;
    use ratatui_image::thread::ThreadProtocol;
    use ratatui_image::{Resize, ResizeEncodeRender};
    use tokio::sync::mpsc::UnboundedReceiver;

    use crate::error::FsError;
    use crate::ui::effects::edit::tests::{remote, FakeConnections, Serve};

    /// The next background event, failing the test after five seconds.
    async fn next_event(events: &mut UnboundedReceiver<BgEvent>) -> BgEvent {
        tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("an event within five seconds")
            .expect("the channel is open")
    }

    fn start(
        connections: Arc<FakeConnections>,
        request: RequestId,
        location: Location,
    ) -> (Arc<Tasks<RequestId>>, UnboundedReceiver<BgEvent>) {
        let requests = Arc::new(Tasks::new());
        let (tx, events) = tokio::sync::mpsc::unbounded_channel();
        spawn_load(
            &requests,
            connections,
            tx,
            Arc::new(Picker::halfblocks()),
            request,
            location,
        );
        (requests, events)
    }

    #[tokio::test]
    async fn a_text_file_is_loaded_over_the_browsing_channel() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.rs");
        std::fs::write(&file, b"fn main() {}\n").unwrap();
        let connections = FakeConnections::new(Serve::Local);

        let (_requests, mut events) = start(connections.clone(), RequestId(7), remote(&file));

        match next_event(&mut events).await {
            BgEvent::PreviewLoaded {
                request,
                location,
                result: Ok(PreviewData::Text { bytes, truncated }),
            } => {
                assert_eq!(request, RequestId(7));
                assert_eq!(location, remote(&file));
                assert_eq!(bytes, b"fn main() {}\n");
                assert!(!truncated);
            }
            other => panic!("expected a text preview, got {other:?}"),
        }
        assert_eq!(connections.channels(), [FsChannel::Browse]);
    }

    #[tokio::test]
    async fn an_image_is_decoded_for_the_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("dot.png");
        let image = RgbaImage::from_pixel(3, 2, Rgba([200, 10, 10, 255]));
        image.save(&file).unwrap();

        let (_requests, mut events) = start(
            FakeConnections::new(Serve::Local),
            RequestId(1),
            remote(&file),
        );

        match next_event(&mut events).await {
            BgEvent::PreviewLoaded {
                result: Ok(PreviewData::Image(image)),
                ..
            } => assert_eq!((image.width, image.height), (3, 2)),
            other => panic!("expected an image preview, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_missing_connection_is_the_preview_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");

        let (_requests, mut events) = start(
            FakeConnections::new(Serve::Disconnected),
            RequestId(2),
            remote(&file),
        );

        match next_event(&mut events).await {
            BgEvent::PreviewLoaded {
                request: RequestId(2),
                result: Err(error),
                ..
            } => assert_eq!(error, Error::from(FsError::Disconnected)),
            other => panic!("expected a failed preview, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_canceled_load_never_answers() {
        let dir = tempfile::tempdir().unwrap();
        let (requests, mut events) = start(
            FakeConnections::new(Serve::Pending),
            RequestId(3),
            remote(&dir.path().join("slow.txt")),
        );
        assert!(requests.contains(&RequestId(3)));

        assert!(requests.abort(&RequestId(3)));

        // The aborted task drops its event sender unsent, which closes the
        // channel (`start` kept no other sender).
        let closed = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the channel closes within five seconds");
        assert!(closed.is_none(), "no PreviewLoaded after cancellation");
        assert!(!requests.contains(&RequestId(3)));
    }

    /// A one-shot image protocol whose resize requests go to `tx`.
    fn thread_protocol(tx: mpsc::Sender<ResizeRequest>) -> ThreadProtocol {
        let image = RgbaImage::from_pixel(8, 8, Rgba([0, 120, 255, 255]));
        let protocol = Picker::halfblocks().new_resize_protocol(DynamicImage::ImageRgba8(image));
        ThreadProtocol::new(tx, Some(protocol))
    }

    #[tokio::test]
    async fn the_resize_worker_answers_each_request_and_ends_with_its_senders() {
        let (tx, requests) = mpsc::channel();
        let (events_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        let worker = start_resize_worker(requests, events_tx);
        let mut protocol = thread_protocol(tx);

        protocol.resize_encode(&Resize::Fit(None), Size::new(4, 2));
        let resized = match next_event(&mut events).await {
            BgEvent::ImageResized(resized) => resized,
            other => panic!("expected ImageResized, got {other:?}"),
        };
        let response = resized.result.expect("the resize succeeded");
        assert!(
            protocol.update_resized_protocol(response),
            "the response belongs to the protocol that asked"
        );

        drop(protocol);
        tokio::task::spawn_blocking(move || worker.join())
            .await
            .expect("joined")
            .expect("the worker ended without panicking");
    }

    #[tokio::test]
    async fn the_resize_worker_stops_when_the_app_is_gone() {
        let (tx, requests) = mpsc::channel();
        let (events_tx, events) = tokio::sync::mpsc::unbounded_channel();
        drop(events);
        let worker = start_resize_worker(requests, events_tx);
        let mut protocol = thread_protocol(tx.clone());

        // The sender `tx` stays alive: only the closed event channel can end
        // the worker.
        protocol.resize_encode(&Resize::Fit(None), Size::new(4, 2));
        tokio::task::spawn_blocking(move || worker.join())
            .await
            .expect("joined")
            .expect("the worker ended without panicking");
        drop(tx);
    }
}
