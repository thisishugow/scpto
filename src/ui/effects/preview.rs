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
