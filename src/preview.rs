//! File preview: text (syntax highlighted, Markdown rendered) and images.
//!
//! Loading reads the first [`TEXT_READ_LIMIT`] bytes of a file, detects
//! binary content (a NUL byte within the first [`BINARY_SNIFF_LEN`] bytes),
//! and decodes png, jpeg, gif (first frame), and webp images up to
//! [`IMAGE_FETCH_LIMIT`] with `ratatui-image`, off the async runtime threads.
//! Remote previews always use the browsing SFTP channel, never the transfer
//! channel.
//!
//! The types below are the payload of `BgEvent::PreviewLoaded`; they carry
//! raw data so that highlighting and Markdown rendering stay in the UI layer.

use std::fmt;
use std::time::Duration;

use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::thread::ResizeResponse;

use crate::vfs::Entry;

/// Maximum number of bytes read for a text preview.
pub const TEXT_READ_LIMIT: usize = 256 * 1024;

/// Number of leading bytes inspected for a NUL byte to detect binary files.
pub const BINARY_SNIFF_LEN: usize = 8 * 1024;

/// Maximum size of an image file fetched for preview.
pub const IMAGE_FETCH_LIMIT: u64 = 20 * 1024 * 1024;

/// Delay after the last cursor move before quick view loads a preview.
pub const QUICK_VIEW_DEBOUNCE: Duration = Duration::from_millis(150);

/// Loaded preview content.
#[derive(Debug)]
pub enum PreviewData {
    /// Text content, possibly cut at [`TEXT_READ_LIMIT`].
    Text {
        /// The raw bytes read (not necessarily valid UTF-8).
        bytes: Vec<u8>,
        /// Whether the file is longer than what was read.
        truncated: bool,
    },
    /// Binary content; only the file's attributes are shown.
    Binary {
        /// The previewed file's attributes (size, type, mode).
        entry: Entry,
    },
    /// A decoded image, already prepared for the terminal's graphics protocol.
    Image(Box<ImagePreview>),
}

/// A decoded image ready to render.
pub struct ImagePreview {
    /// The encoded image state for `ratatui-image`.
    pub protocol: StatefulProtocol,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
}

impl fmt::Debug for ImagePreview {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImagePreview")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// The result of a background image resize and re-encode requested by a
/// `ratatui-image` `ThreadProtocol`.
pub struct ImageResized {
    /// The re-encoded protocol, or the encoder's error text.
    pub result: Result<ResizeResponse, String>,
}

impl fmt::Debug for ImageResized {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.result {
            Ok(_) => f.write_str("ImageResized(Ok)"),
            Err(message) => write!(f, "ImageResized(Err({message:?}))"),
        }
    }
}
