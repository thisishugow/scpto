//! Local `Fs` implementation on `tokio::fs`. Deletion goes to the system
//! trash through the `trash` crate (`DeleteMethod::NsFileManager` on macOS,
//! so Finder automation is never prompted), run in `spawn_blocking`.
