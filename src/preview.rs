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
//! raw data so that highlighting and Markdown rendering run in the UI layer
//! (through the functions below), not in the effect executor.
//!
//! # Contract (plan 8.4)
//!
//! - `load` stats the file (following symlinks). Names ending in `.png`,
//!   `.jpg`, `.jpeg`, `.gif`, `.webp` (case-insensitive) are images: larger
//!   than [`IMAGE_FETCH_LIMIT`] or undecodable → `Error::Preview`; decoding
//!   (first frame for gif) and `Picker::new_resize_protocol` run in
//!   `spawn_blocking`. Everything else reads at most [`TEXT_READ_LIMIT`]
//!   bytes; a NUL in the first [`BINARY_SNIFF_LEN`] bytes makes it
//!   `PreviewData::Binary`.
//! - Highlighting uses `two_face` syntaxes with a syntect default theme
//!   (`base16-ocean.dark` on a dark background, `InspiredGitHub` on a light
//!   one). The syntax is chosen by file name, then extension, then the first
//!   line (shebang); plain text otherwise. Bytes are decoded as lossy UTF-8.
//! - Markdown (`.md`, `.markdown`) renders with `tui_markdown::from_str`
//!   into owned lines; the raw view is highlighted as Markdown.
//! - The image resize worker calls `resize` for each `ResizeRequest` from a
//!   `ThreadProtocol` inside `spawn_blocking` and sends the result as
//!   `BgEvent::ImageResized`.
//!
//! # Public API (Wave 3)
//!
//! ```text
//! pub async fn load(fs: &dyn Fs, path: &Path, picker: &Picker) -> Result<PreviewData, Error>;
//!
//! /// Pure: whether the leading bytes contain a NUL.
//! pub fn is_binary(head: &[u8]) -> bool;
//! pub fn is_markdown(name: &OsStr) -> bool;
//!
//! /// Loaded once (the syntax set is large) and kept by the app.
//! pub struct Highlighter { /* private */ }
//! impl Highlighter {
//!     pub fn new(background: Background) -> Highlighter;
//!     pub fn highlight(&self, name: &OsStr, bytes: &[u8]) -> ratatui::text::Text<'static>;
//! }
//!
//! pub fn render_markdown(bytes: &[u8]) -> ratatui::text::Text<'static>;
//!
//! /// Blocking resize and encode; errors become `Error::Preview`.
//! pub fn resize(request: ratatui_image::thread::ResizeRequest) -> ImageResized;
//! ```

use std::fmt;
use std::time::Duration;

use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::thread::ResizeResponse;

use crate::error::Error;
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
    /// The re-encoded protocol, or `Error::Preview` with the encoder's
    /// message.
    pub result: Result<ResizeResponse, Error>,
}

impl fmt::Debug for ImageResized {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.result {
            Ok(_) => f.write_str("ImageResized(Ok)"),
            Err(error) => write!(f, "ImageResized(Err({error:?}))"),
        }
    }
}

/// The terminal's background brightness, detected once at startup by
/// `term` and used to pick the syntax highlighting theme (plan 8.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Background {
    /// Dark background; also the fallback when detection fails.
    #[default]
    Dark,
    /// Light background.
    Light,
}
