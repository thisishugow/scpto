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
//!   `PreviewData::Binary`. Special files (FIFO, socket, device) are never
//!   read, because reading a FIFO blocks; they are `PreviewData::Binary` as
//!   well. A directory is `FsError::IsADirectory`.
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

use std::borrow::Cow;
use std::ffi::OsStr;
use std::fmt;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::thread::{ResizeRequest, ResizeResponse};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;
use tokio::io::AsyncReadExt;

use crate::error::{Error, FsError};
use crate::vfs::{Entry, EntryKind, Fs};

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

/// File extensions (lowercase) previewed as images.
const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

/// File extensions (lowercase) rendered as Markdown.
const MARKDOWN_EXTENSIONS: [&str; 2] = ["md", "markdown"];

/// Theme used on a dark terminal background.
const DARK_THEME: &str = "base16-ocean.dark";

/// Theme used on a light terminal background.
const LIGHT_THEME: &str = "InspiredGitHub";

/// Lines longer than this many bytes are shown without highlighting. The
/// regex engine's cost grows steeply with line length, and a single
/// minified line could otherwise stall the UI thread for seconds. Parsing
/// restarts from the syntax's top level after such a line.
const MAX_HIGHLIGHT_LINE_LEN: usize = 16 * 1024;

/// Columns a tab character expands to. Terminal cells cannot hold a tab.
const TAB_WIDTH: usize = 4;

/// Loads the preview of `path` on `fs` (see the module documentation for
/// the rules). Image decoding runs in `spawn_blocking`.
pub async fn load(fs: &dyn Fs, path: &Path, picker: &Picker) -> Result<PreviewData, Error> {
    let entry = fs.stat(path).await?;
    match entry.kind {
        EntryKind::Dir => {
            return Err(FsError::IsADirectory {
                path: path.to_path_buf(),
            }
            .into())
        }
        EntryKind::Special(_) => return Ok(PreviewData::Binary { entry }),
        // `stat` follows symlinks, so a symlink here is one the endpoint
        // reported unresolved; reading it surfaces the real error.
        EntryKind::File | EntryKind::Symlink { .. } => {}
    }
    let name = path.file_name().unwrap_or(entry.name.as_os_str());
    if has_extension(name, &IMAGE_EXTENSIONS) {
        return load_image(fs, path, &entry, picker).await;
    }

    let mut bytes = read_at_most(fs, path, TEXT_READ_LIMIT as u64 + 1).await?;
    let truncated = bytes.len() > TEXT_READ_LIMIT;
    bytes.truncate(TEXT_READ_LIMIT);
    if is_binary(&bytes[..bytes.len().min(BINARY_SNIFF_LEN)]) {
        return Ok(PreviewData::Binary { entry });
    }
    Ok(PreviewData::Text { bytes, truncated })
}

/// Fetches and decodes an image no larger than [`IMAGE_FETCH_LIMIT`].
async fn load_image(
    fs: &dyn Fs,
    path: &Path,
    entry: &Entry,
    picker: &Picker,
) -> Result<PreviewData, Error> {
    if entry.size.is_some_and(|size| size > IMAGE_FETCH_LIMIT) {
        return Err(image_too_large());
    }
    // The reported size may be missing or stale, so the read is bounded too.
    let bytes = read_at_most(fs, path, IMAGE_FETCH_LIMIT + 1).await?;
    if bytes.len() as u64 > IMAGE_FETCH_LIMIT {
        return Err(image_too_large());
    }
    let picker = picker.clone();
    let preview = tokio::task::spawn_blocking(move || decode_image(&bytes, &picker))
        .await
        .map_err(|error| Error::Preview {
            message: format!("the image decoder stopped unexpectedly: {error}"),
        })??;
    Ok(PreviewData::Image(Box::new(preview)))
}

/// Decodes `bytes` (format detected from the content; the first frame of
/// an animated gif) and prepares it for the terminal's graphics protocol.
fn decode_image(bytes: &[u8], picker: &Picker) -> Result<ImagePreview, Error> {
    let image = image::load_from_memory(bytes).map_err(|error| Error::Preview {
        message: error.to_string(),
    })?;
    let (width, height) = (image.width(), image.height());
    Ok(ImagePreview {
        protocol: picker.new_resize_protocol(image),
        width,
        height,
    })
}

fn image_too_large() -> Error {
    Error::Preview {
        message: format!(
            "images larger than {} MiB are not previewed",
            IMAGE_FETCH_LIMIT / (1024 * 1024)
        ),
    }
}

/// Reads the first `limit` bytes of `path` (fewer if the file is shorter).
async fn read_at_most(fs: &dyn Fs, path: &Path, limit: u64) -> Result<Vec<u8>, Error> {
    let reader = fs.open_read(path, 0).await?;
    let mut bytes = Vec::new();
    reader
        .take(limit)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| FsError::from_io(&error, path))?;
    Ok(bytes)
}

/// Whether the leading bytes contain a NUL.
pub fn is_binary(head: &[u8]) -> bool {
    head.contains(&0)
}

/// Whether `name` has a Markdown extension (`.md`, `.markdown`, any case).
pub fn is_markdown(name: &OsStr) -> bool {
    has_extension(name, &MARKDOWN_EXTENSIONS)
}

/// Whether `name`'s extension is one of `extensions`, ignoring ASCII case.
fn has_extension(name: &OsStr, extensions: &[&str]) -> bool {
    Path::new(name)
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| extensions.iter().any(|e| e.eq_ignore_ascii_case(ext)))
}

/// bat's syntax set from `two-face`, loaded on first use.
fn syntax_set() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

/// syntect's bundled themes, loaded on first use.
fn theme_set() -> &'static ThemeSet {
    static THEMES: OnceLock<ThemeSet> = OnceLock::new();
    THEMES.get_or_init(ThemeSet::load_defaults)
}

/// Syntax highlighter for text previews. Loaded once (the syntax set is
/// large) and kept by the app.
pub struct Highlighter {
    syntaxes: &'static SyntaxSet,
    theme: &'static Theme,
}

impl fmt::Debug for Highlighter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Highlighter")
            .field("theme", &self.theme.name)
            .finish_non_exhaustive()
    }
}

impl Highlighter {
    /// Creates a highlighter with the theme for `background`.
    pub fn new(background: Background) -> Highlighter {
        let name = match background {
            Background::Dark => DARK_THEME,
            Background::Light => LIGHT_THEME,
        };
        Highlighter {
            syntaxes: syntax_set(),
            // Both names are themes bundled by syntect's `default-themes`.
            theme: &theme_set().themes[name],
        }
    }

    /// Highlights `bytes` (lossy UTF-8) with the syntax chosen for the file
    /// `name` and the content's first line. Only foreground colors and font
    /// styles are applied, so the terminal background shows through.
    pub fn highlight(&self, name: &OsStr, bytes: &[u8]) -> Text<'static> {
        let content = String::from_utf8_lossy(bytes);
        let first_line = content.lines().next().unwrap_or("");
        let syntax = self.find_syntax(name, first_line);
        let mut highlighter = HighlightLines::new(syntax, self.theme);
        let mut source = LinesWithEndings::from(&content);
        let mut lines = Vec::new();
        for line in source.by_ref() {
            if line.len() > MAX_HIGHLIGHT_LINE_LEN {
                // The skipped line may open a string or comment, so the old
                // parse state no longer describes what follows.
                highlighter = HighlightLines::new(syntax, self.theme);
                lines.push(Line::raw(display_text(line)));
                continue;
            }
            match highlighter.highlight_line(line, self.syntaxes) {
                Ok(regions) => lines.push(styled_line(&regions)),
                Err(_) => {
                    // The grammar failed on this line, which leaves the parse
                    // state unusable: this line and the rest are shown
                    // without highlighting.
                    lines.push(Line::raw(display_text(line)));
                    break;
                }
            }
        }
        lines.extend(source.map(|line| Line::raw(display_text(line))));
        Text::from(lines)
    }

    /// Chooses the syntax by full file name (`Dockerfile`, `nginx.conf`),
    /// then extension, then first line (shebang, modeline); plain text
    /// otherwise.
    fn find_syntax(&self, name: &OsStr, first_line: &str) -> &'static SyntaxReference {
        let syntaxes = self.syntaxes;
        let name = name.to_string_lossy();
        syntaxes
            .find_syntax_by_extension(&name)
            .or_else(|| {
                Path::new(name.as_ref())
                    .extension()
                    .and_then(OsStr::to_str)
                    .and_then(|ext| syntaxes.find_syntax_by_extension(ext))
            })
            .or_else(|| syntaxes.find_syntax_by_first_line(first_line))
            .unwrap_or_else(|| syntaxes.find_syntax_plain_text())
    }
}

/// Converts the highlighted regions of one line into a ratatui line.
fn styled_line(regions: &[(syntect::highlighting::Style, &str)]) -> Line<'static> {
    let spans: Vec<Span<'static>> = regions
        .iter()
        .filter_map(|(style, text)| {
            let text = display_text(text);
            (!text.is_empty()).then(|| Span::styled(text, convert_style(*style)))
        })
        .collect();
    Line::from(spans)
}

/// Maps a syntect style to a ratatui style (foreground and font style only).
fn convert_style(style: syntect::highlighting::Style) -> Style {
    let fg = style.foreground;
    let mut modifiers = Modifier::empty();
    if style.font_style.contains(FontStyle::BOLD) {
        modifiers |= Modifier::BOLD;
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        modifiers |= Modifier::ITALIC;
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        modifiers |= Modifier::UNDERLINED;
    }
    Style::default()
        .fg(Color::Rgb(fg.r, fg.g, fg.b))
        .add_modifier(modifiers)
}

/// Strips the line ending and expands tabs, which terminal cells cannot
/// hold.
fn display_text(text: &str) -> String {
    let text = text.trim_end_matches(['\n', '\r']);
    if text.contains('\t') {
        text.replace('\t', &" ".repeat(TAB_WIDTH))
    } else {
        text.to_owned()
    }
}

/// Renders Markdown (lossy UTF-8) with `tui-markdown` into owned lines.
pub fn render_markdown(bytes: &[u8]) -> Text<'static> {
    let content = String::from_utf8_lossy(bytes);
    let rendered = tui_markdown::from_str(&content);
    let lines = rendered
        .lines
        .into_iter()
        .map(|line| Line {
            style: line.style,
            alignment: line.alignment,
            spans: line
                .spans
                .into_iter()
                .map(|span| Span::styled(Cow::Owned(span.content.into_owned()), span.style))
                .collect(),
        })
        .collect();
    Text {
        lines,
        style: rendered.style,
        alignment: rendered.alignment,
    }
}

/// Blocking resize and encode; errors become `Error::Preview`.
pub fn resize(request: ResizeRequest) -> ImageResized {
    ImageResized {
        result: request.resize_encode().map_err(|error| Error::Preview {
            message: error.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Cursor;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use futures_util::future::BoxFuture;
    use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};

    use crate::vfs::{Capabilities, Endpoint, FsReader, FsResult, FsWriter, WriteMode};

    /// Read-only `Fs` over `tokio::fs`, providing exactly the operations
    /// `load` uses (`stat`, `open_read`) so the real loading code runs
    /// against real files.
    struct DiskFs {
        endpoint: Endpoint,
    }

    impl DiskFs {
        fn new() -> DiskFs {
            DiskFs {
                endpoint: Endpoint::Local,
            }
        }
    }

    fn unsupported<T>(operation: &str) -> BoxFuture<'static, FsResult<T>> {
        let error = FsError::Unsupported {
            operation: operation.into(),
            reason: "not needed by preview tests".into(),
        };
        Box::pin(async move { Err(error) })
    }

    impl Fs for DiskFs {
        fn endpoint(&self) -> &Endpoint {
            &self.endpoint
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                posix_rename: false,
                copy_within: false,
                symlink: false,
                trash: false,
            }
        }

        fn home(&self) -> BoxFuture<'_, FsResult<PathBuf>> {
            unsupported("home")
        }

        fn canonicalize<'a>(&'a self, _path: &'a Path) -> BoxFuture<'a, FsResult<PathBuf>> {
            unsupported("canonicalize")
        }

        fn list<'a>(&'a self, _dir: &'a Path) -> BoxFuture<'a, FsResult<Vec<Entry>>> {
            unsupported("list")
        }

        fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<Entry>> {
            Box::pin(async move {
                let meta = tokio::fs::metadata(path)
                    .await
                    .map_err(|error| FsError::from_io(&error, path))?;
                let kind = if meta.is_dir() {
                    EntryKind::Dir
                } else if meta.is_file() {
                    EntryKind::File
                } else {
                    EntryKind::Special(crate::vfs::SpecialKind::Fifo)
                };
                Ok(Entry {
                    name: path.file_name().unwrap_or_default().to_os_string(),
                    kind,
                    size: Some(meta.len()),
                    mtime: None,
                    mode: Some(meta.permissions().mode() & 0o7777),
                })
            })
        }

        fn lstat<'a>(&'a self, _path: &'a Path) -> BoxFuture<'a, FsResult<Entry>> {
            unsupported("lstat")
        }

        fn mkdir<'a>(&'a self, _path: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            unsupported("mkdir")
        }

        fn rename<'a>(&'a self, _from: &'a Path, _to: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            unsupported("rename")
        }

        fn remove_file<'a>(&'a self, _path: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            unsupported("remove_file")
        }

        fn remove_dir<'a>(&'a self, _path: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            unsupported("remove_dir")
        }

        fn trash<'a>(&'a self, _path: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            unsupported("trash")
        }

        fn chmod<'a>(&'a self, _path: &'a Path, _mode: u32) -> BoxFuture<'a, FsResult<()>> {
            unsupported("chmod")
        }

        fn set_times<'a>(
            &'a self,
            _path: &'a Path,
            _atime: i64,
            _mtime: i64,
        ) -> BoxFuture<'a, FsResult<()>> {
            unsupported("set_times")
        }

        fn read_link<'a>(&'a self, _path: &'a Path) -> BoxFuture<'a, FsResult<PathBuf>> {
            unsupported("read_link")
        }

        fn symlink<'a>(
            &'a self,
            _target: &'a Path,
            _link: &'a Path,
        ) -> BoxFuture<'a, FsResult<()>> {
            unsupported("symlink")
        }

        fn open_read<'a>(
            &'a self,
            path: &'a Path,
            offset: u64,
        ) -> BoxFuture<'a, FsResult<FsReader>> {
            assert_eq!(offset, 0, "previews always read from the start");
            Box::pin(async move {
                let file = tokio::fs::File::open(path)
                    .await
                    .map_err(|error| FsError::from_io(&error, path))?;
                Ok(Box::pin(file) as FsReader)
            })
        }

        fn open_write<'a>(
            &'a self,
            _path: &'a Path,
            _mode: WriteMode,
        ) -> BoxFuture<'a, FsResult<FsWriter>> {
            unsupported("open_write")
        }

        fn copy_within<'a>(
            &'a self,
            _from: &'a Path,
            _to: &'a Path,
        ) -> BoxFuture<'a, FsResult<()>> {
            unsupported("copy_within")
        }
    }

    fn write_file(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).expect("write test file");
        path
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = RgbaImage::from_fn(width, height, |x, y| {
            Rgba([(x * 40) as u8, (y * 40) as u8, 200, 255])
        });
        let mut bytes = Vec::new();
        DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .expect("encode png");
        bytes
    }

    fn plain(text: &Text<'_>) -> Vec<String> {
        text.lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    fn syntax_name(name: &str, first_line: &str) -> String {
        Highlighter::new(Background::Dark)
            .find_syntax(OsStr::new(name), first_line)
            .name
            .clone()
    }

    #[test]
    fn nul_bytes_mark_content_as_binary() {
        assert!(is_binary(b"\x7fELF\x02\x01\x01\x00"));
        assert!(!is_binary(b"fn main() {}\n"));
        assert!(!is_binary(b""));
        // Non-UTF-8 text without NUL is still text (decoded lossily).
        assert!(!is_binary(b"caf\xe9\n"));
    }

    #[test]
    fn markdown_is_detected_by_extension_ignoring_case() {
        assert!(is_markdown(OsStr::new("README.md")));
        assert!(is_markdown(OsStr::new("notes.MarkDown")));
        assert!(!is_markdown(OsStr::new("md")));
        assert!(!is_markdown(OsStr::new("page.mdx")));
    }

    #[test]
    fn syntax_is_chosen_by_name_extension_then_first_line() {
        assert_eq!(syntax_name("Dockerfile", ""), "Dockerfile");
        assert_eq!(syntax_name("nginx.conf", ""), "nginx");
        assert_eq!(syntax_name("Cargo.toml", ""), "TOML");
        assert_eq!(syntax_name("CONFIG.TOML", ""), "TOML");
        assert_eq!(
            syntax_name("deploy", "#!/bin/bash"),
            "Bourne Again Shell (bash)"
        );
        assert_eq!(syntax_name("tool", "#!/usr/bin/env python3"), "Python");
        assert_eq!(syntax_name("notes", "just words"), "Plain Text");
        // The raw view of a Markdown file is highlighted as Markdown.
        assert_eq!(syntax_name("README.md", ""), "Markdown");
    }

    #[test]
    fn highlighting_keeps_text_and_colors_tokens() {
        let highlighter = Highlighter::new(Background::Dark);
        let source = b"[package]\r\nname = \"scpto\"\n\tkey = 1\n";
        let text = highlighter.highlight(OsStr::new("Cargo.toml"), source);
        assert_eq!(
            plain(&text),
            ["[package]", "name = \"scpto\"", "    key = 1"]
        );
        let colors: std::collections::HashSet<_> = text.lines[1]
            .spans
            .iter()
            .filter_map(|span| span.style.fg)
            .collect();
        assert!(colors.len() > 1, "key and string get different colors");
        assert!(text
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .all(|span| span.style.bg.is_none()));
    }

    #[test]
    fn light_and_dark_backgrounds_use_different_themes() {
        let source = b"fn main() {}\n";
        let dark = Highlighter::new(Background::Dark).highlight(OsStr::new("a.rs"), source);
        let light = Highlighter::new(Background::Light).highlight(OsStr::new("a.rs"), source);
        assert_ne!(dark, light);
    }

    #[test]
    fn overlong_lines_are_shown_unstyled() {
        let mut source = vec![b'x'; MAX_HIGHLIGHT_LINE_LEN + 1];
        source.extend_from_slice(b"\nlet a = 1;\n");
        let text = Highlighter::new(Background::Dark).highlight(OsStr::new("a.js"), &source);
        assert_eq!(text.lines.len(), 2);
        assert_eq!(text.lines[0].spans.len(), 1);
        assert_eq!(text.lines[0].spans[0].style, Style::default());
        assert!(text.lines[1]
            .spans
            .iter()
            .any(|span| span.style.fg.is_some()));
    }

    #[test]
    fn markdown_renders_headings_and_lists_into_owned_text() {
        let text = render_markdown(b"# Title\n\nSome *emphasis* here.\n\n- one\n- two\n");
        let lines = plain(&text);
        assert!(lines.iter().any(|line| line.contains("Title")));
        assert!(!lines.iter().any(|line| line.contains("*emphasis*")));
        assert!(lines.iter().any(|line| line.contains("one")));
        let heading = text
            .lines
            .iter()
            .find(|line| plain_line(line).contains("Title"))
            .expect("heading line");
        let styled =
            |style: Style| style.add_modifier.contains(Modifier::BOLD) || style.fg.is_some();
        assert!(styled(heading.style) || heading.spans.iter().any(|span| styled(span.style)));
    }

    fn plain_line(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[tokio::test]
    async fn text_at_the_limit_is_complete_and_longer_text_is_truncated() {
        let dir = tempfile::tempdir().expect("temp dir");
        let picker = Picker::halfblocks();
        let fs = DiskFs::new();

        let exact = write_file(&dir, "exact.txt", &vec![b'a'; TEXT_READ_LIMIT]);
        match load(&fs, &exact, &picker).await.expect("load exact") {
            PreviewData::Text { bytes, truncated } => {
                assert_eq!(bytes.len(), TEXT_READ_LIMIT);
                assert!(!truncated);
            }
            other => panic!("expected text, got {other:?}"),
        }

        let long = write_file(&dir, "long.log", &vec![b'a'; TEXT_READ_LIMIT + 10]);
        match load(&fs, &long, &picker).await.expect("load long") {
            PreviewData::Text { bytes, truncated } => {
                assert_eq!(bytes.len(), TEXT_READ_LIMIT);
                assert!(truncated);
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn nul_within_the_sniff_window_makes_a_file_binary() {
        let dir = tempfile::tempdir().expect("temp dir");
        let picker = Picker::halfblocks();
        let fs = DiskFs::new();

        let mut head_nul = vec![b'a'; 100];
        head_nul[50] = 0;
        let binary = write_file(&dir, "app.bin", &head_nul);
        match load(&fs, &binary, &picker).await.expect("load binary") {
            PreviewData::Binary { entry } => {
                assert_eq!(entry.size, Some(100));
                assert_eq!(entry.kind, EntryKind::File);
            }
            other => panic!("expected binary, got {other:?}"),
        }

        let mut late_nul = vec![b'a'; BINARY_SNIFF_LEN + 10];
        late_nul[BINARY_SNIFF_LEN + 5] = 0;
        let text = write_file(&dir, "late.txt", &late_nul);
        assert!(matches!(
            load(&fs, &text, &picker).await.expect("load late"),
            PreviewData::Text { .. }
        ));
    }

    #[tokio::test]
    async fn a_tiny_png_decodes_into_an_image_preview() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_file(&dir, "Tiny.PNG", &png_bytes(3, 2));
        match load(&DiskFs::new(), &path, &Picker::halfblocks())
            .await
            .expect("load png")
        {
            PreviewData::Image(image) => {
                assert_eq!((image.width, image.height), (3, 2));
            }
            other => panic!("expected image, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn undecodable_and_oversized_images_are_preview_errors() {
        let dir = tempfile::tempdir().expect("temp dir");
        let picker = Picker::halfblocks();
        let fs = DiskFs::new();

        let broken = write_file(&dir, "broken.jpg", b"not an image");
        assert!(matches!(
            load(&fs, &broken, &picker).await,
            Err(Error::Preview { .. })
        ));

        let huge = dir.path().join("huge.webp");
        std::fs::File::create(&huge)
            .and_then(|file| file.set_len(IMAGE_FETCH_LIMIT + 1))
            .expect("create sparse file");
        assert!(matches!(
            load(&fs, &huge, &picker).await,
            Err(Error::Preview { .. })
        ));
    }

    #[tokio::test]
    async fn directories_and_missing_files_are_fs_errors() {
        let dir = tempfile::tempdir().expect("temp dir");
        let picker = Picker::halfblocks();
        let fs = DiskFs::new();
        assert!(matches!(
            load(&fs, dir.path(), &picker).await,
            Err(Error::Fs(FsError::IsADirectory { .. }))
        ));
        assert!(matches!(
            load(&fs, &dir.path().join("missing.txt"), &picker).await,
            Err(Error::Fs(FsError::NotFound { .. }))
        ));
    }
}
