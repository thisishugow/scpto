//! Terminal lifecycle: entering and restoring the alternate screen, raw
//! mode, mouse capture and bracketed paste through crossterm (always via
//! the `ratatui::crossterm` re-export); the [`TerminalGuard`] and panic hook;
//! SIGTERM/SIGINT/SIGHUP via `tokio::signal::unix`; the single
//! suspend/resume sequence used to run foreground programs (editor, shell,
//! VPN command, interactive auth); and the startup queries that must run
//! before the `EventStream` exists (image protocol, background color).
//!
//! Enter: raw mode, alternate screen, `EnableMouseCapture` (all motion
//! events), `EnableBracketedPaste`, hidden cursor. Restore reverses this in
//! the opposite order and is idempotent; it runs on drop of the guard and
//! from the panic hook (which then calls the previous hook). sshto's
//! `O_NONBLOCK` stdin hack and raw `?1000h ?1006h` sequences are not ported.
//!
//! Suspend/resume (plan 7.2), all inside [`TerminalGuard::run_foreground`]:
//! 1. drop the caller's `EventStream` and wait until its reader thread has
//!    released crossterm's input reader, so no key typed for the child is
//!    stolen; the replacement stream is built at that moment and stays
//!    unpolled (it reads nothing) while the child runs;
//! 2. disable raw mode, mouse capture, bracketed paste; leave the alternate
//!    screen; show the cursor;
//! 3. spawn the command with inherited stdio in scpto's own process group,
//!    which is the terminal's foreground group, and wait for it. Like
//!    `system(3)`, scpto ignores SIGINT and SIGQUIT while it waits and the
//!    child starts with both at their defaults, so Ctrl+C and Ctrl+\ reach
//!    only the child; scpto keeps running and keeps its connections
//!    (background ssh children live in their own process groups). Ctrl+Z
//!    stops scpto and the child together, and `fg` resumes both;
//! 4. re-enter, hand the fresh `EventStream` back, and clear the terminal so
//!    the next draw is a full redraw.
//!
//! The returned future must be awaited to completion: dropping it while the
//! child runs leaves the terminal suspended.
//!
//! Startup queries write escape sequences to stdout and read the replies
//! from stdin, so they run after [`TerminalGuard::enter`] (raw mode) and
//! before the first `EventStream`. Each first checks that the terminal
//! answers a device status report (`CSI 5 n`) within one second; a
//! terminal that does not answer is never handed to `ratatui-image`'s query,
//! whose reader thread would otherwise outlive its timeout and swallow the
//! first keystrokes.
//!
//! # Public API (Wave 2)
//!
//! ```text
//! pub struct TerminalGuard { /* private: the ratatui terminal */ }
//!
//! impl TerminalGuard {
//!     /// Enters TUI mode on stdout.
//!     pub fn enter() -> std::io::Result<TerminalGuard>;
//!     pub fn terminal(&mut self) -> &mut ratatui::DefaultTerminal;
//!     /// Steps 1-4 above. Takes the caller's stream by value and always
//!     /// returns a fresh one. The result is the child's exit status; an
//!     /// `Err` is a spawn or terminal failure (the terminal is re-entered
//!     /// either way when possible).
//!     pub async fn run_foreground(
//!         &mut self,
//!         events: EventStream,
//!         command: tokio::process::Command,
//!     ) -> (EventStream, std::io::Result<std::process::ExitStatus>);
//! }
//!
//! /// Installs the panic hook that restores the terminal first. Called once
//! /// before `TerminalGuard::enter`.
//! pub fn install_panic_hook();
//!
//! /// SIGTERM, SIGINT, SIGHUP as a stream for the runtime's select loop.
//! pub struct Signals { /* private */ }
//! impl Signals {
//!     pub fn new() -> std::io::Result<Signals>;
//!     pub async fn recv(&mut self) -> TermSignal;
//! }
//! pub enum TermSignal { Terminate, Interrupt, Hangup }
//!
//! /// Startup queries, run after `enter` and before the first `EventStream`.
//! /// Inside tmux (`$TMUX` set) or when the terminal does not answer, the
//! /// picker is `Picker::halfblocks()`.
//! pub fn query_image_picker() -> ratatui_image::picker::Picker;
//! /// OSC 11 background query; `Background::Dark` when unanswered.
//! pub fn query_background() -> preview::Background;
//! ```

use std::ffi::OsStr;
use std::io::{self, IsTerminal, Write};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ratatui::backend::{Backend, ClearType, CrosstermBackend};
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    EventStream,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::{DefaultTerminal, Terminal};
use ratatui_image::picker::Picker;
use tokio::process::Command;
use tokio::signal::unix::{signal, Signal, SignalKind};

use crate::preview::Background;

/// How long a startup query waits for the terminal's reply.
const QUERY_TIMEOUT: Duration = Duration::from_secs(1);

/// Device status report request; every terminal answers it, so its reply
/// marks the end of the replies to the requests sent before it.
const STATUS_REQUEST: &[u8] = b"\x1b[5n";

/// OSC 11 request for the default background color, terminated by ST.
const BACKGROUND_REQUEST: &[u8] = b"\x1b]11;?\x1b\\";

/// Upper bound on the bytes read while waiting for a status reply; a
/// terminal that streams more than this without answering is treated as
/// not answering.
const QUERY_REPLY_LIMIT: usize = 4096;

/// Whether this process has the terminal in TUI mode. Process-wide because
/// the panic hook has no access to the guard.
static ENTERED: AtomicBool = AtomicBool::new(false);

/// Owns the ratatui terminal while scpto is in TUI mode and restores the
/// terminal when dropped.
pub struct TerminalGuard {
    terminal: DefaultTerminal,
}

impl TerminalGuard {
    /// Enters TUI mode on stdout: raw mode, alternate screen, mouse capture
    /// (all motion events), bracketed paste, hidden cursor.
    pub fn enter() -> io::Result<TerminalGuard> {
        enter_modes()?;
        match Terminal::new(CrosstermBackend::new(io::stdout())) {
            Ok(terminal) => Ok(TerminalGuard { terminal }),
            Err(error) => {
                report_restore(restore_modes());
                Err(error)
            }
        }
    }

    /// The ratatui terminal to draw on.
    pub fn terminal(&mut self) -> &mut DefaultTerminal {
        &mut self.terminal
    }

    /// Suspends the TUI, runs `command` in the foreground with inherited
    /// stdio, and resumes (plan 7.2; steps 1-4 in the module doc).
    ///
    /// `events` is dropped before the terminal is handed over and a fresh
    /// stream is always returned, also on error. The result is the child's
    /// exit status; an `Err` is a spawn or terminal failure, reported after
    /// the terminal has been re-entered when possible.
    pub async fn run_foreground(
        &mut self,
        events: EventStream,
        command: Command,
    ) -> (EventStream, io::Result<ExitStatus>) {
        drop(events);
        let events = rebuild_event_stream().await;
        let left = restore_modes();
        let status = match left {
            Ok(()) => run_child(command).await,
            Err(error) => Err(error),
        };
        let resumed = self.resume();
        (events, status.and_then(|status| resumed.map(|()| status)))
    }

    /// Re-enters TUI mode, clears the screen, and resets both of ratatui's
    /// buffers so the next draw repaints every cell. `Terminal::clear` is
    /// not used because it first asks the terminal for the cursor position,
    /// a round trip a fullscreen viewport does not need and one that fails
    /// on a terminal that does not answer.
    fn resume(&mut self) -> io::Result<()> {
        enter_modes()?;
        self.terminal.backend_mut().clear_region(ClearType::All)?;
        // Each swap resets the buffer it makes current, so two swaps leave
        // both the drawing buffer and the diff base blank.
        self.terminal.swap_buffers();
        self.terminal.swap_buffers();
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        report_restore(restore_modes());
    }
}

/// Installs the panic hook that restores the terminal first and then runs
/// the previously installed hook. Called once before
/// [`TerminalGuard::enter`].
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report_restore(restore_modes());
        previous(info);
    }));
}

/// A termination-class signal delivered to scpto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermSignal {
    /// SIGTERM.
    Terminate,
    /// SIGINT (raw mode turns Ctrl+C into a key, so this comes from
    /// `kill -INT`).
    Interrupt,
    /// SIGHUP: the terminal went away.
    Hangup,
}

/// SIGTERM, SIGINT, and SIGHUP as one stream for the runtime's select loop.
pub struct Signals {
    terminate: Signal,
    interrupt: Signal,
    hangup: Signal,
}

impl Signals {
    /// Registers the handlers; must be called inside the tokio runtime.
    pub fn new() -> io::Result<Signals> {
        Ok(Signals {
            terminate: signal(SignalKind::terminate())?,
            interrupt: signal(SignalKind::interrupt())?,
            hangup: signal(SignalKind::hangup())?,
        })
    }

    /// Waits for the next signal.
    pub async fn recv(&mut self) -> TermSignal {
        tokio::select! {
            Some(()) = self.terminate.recv() => TermSignal::Terminate,
            Some(()) = self.interrupt.recv() => TermSignal::Interrupt,
            Some(()) = self.hangup.recv() => TermSignal::Hangup,
            // `recv` ends only when the runtime's signal driver is gone,
            // which happens at runtime shutdown; no signal can arrive after
            // that, and shutdown drops this future.
            else => std::future::pending().await,
        }
    }
}

/// Picks the image protocol once at startup. Inside tmux (`$TMUX` set),
/// when the terminal does not answer, or when it reports no capability at
/// all, the picker is `Picker::halfblocks()`.
pub fn query_image_picker() -> Picker {
    if inside_tmux(std::env::var_os("TMUX").as_deref()) || !terminal_answers() {
        return Picker::halfblocks();
    }
    match Picker::from_query_stdio() {
        Ok(picker) if !picker.capabilities().is_empty() => picker,
        // Image preview is optional: a terminal that reports nothing, or a
        // failed query, degrades to halfblocks, which every terminal draws.
        Ok(_) | Err(_) => Picker::halfblocks(),
    }
}

/// Detects the terminal background with an OSC 11 query for the syntax
/// theme; [`Background::Dark`] when the terminal does not answer or the
/// reply cannot be parsed.
pub fn query_background() -> Background {
    let mut request = BACKGROUND_REQUEST.to_vec();
    request.extend_from_slice(STATUS_REQUEST);
    match query_terminal(&request) {
        Ok(Some(reply)) => parse_background_reply(&reply)
            .map(classify_background)
            .unwrap_or_default(),
        // The theme is cosmetic: an unanswered or failed query keeps the
        // documented dark default instead of failing startup.
        Ok(None) | Err(_) => Background::Dark,
    }
}

/// Switches the terminal into TUI mode. Marks the terminal as entered as
/// soon as raw mode is on, so a failure halfway is undone by a restore.
fn enter_modes() -> io::Result<()> {
    enable_raw_mode()?;
    ENTERED.store(true, Ordering::SeqCst);
    let entered = execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        Hide
    );
    if entered.is_err() {
        report_restore(restore_modes());
    }
    entered
}

/// Leaves TUI mode in the reverse order of [`enter_modes`]. Does nothing
/// when the terminal is not entered. Raw mode is disabled even when the
/// escape sequences cannot be written; the first error is returned.
fn restore_modes() -> io::Result<()> {
    if !ENTERED.swap(false, Ordering::SeqCst) {
        return Ok(());
    }
    let screen = execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        Show
    );
    let raw = disable_raw_mode();
    screen.and(raw)
}

/// Prints a restore failure to stderr. Used where the error cannot be
/// returned (drop, the panic hook, cleanup after a failed enter); the
/// terminal has already left the alternate screen when it can, so the
/// message is visible.
fn report_restore(result: io::Result<()>) {
    if let Err(error) = result {
        eprintln!("scpto: cannot restore the terminal: {error}");
    }
}

/// Builds the replacement `EventStream`. Its constructor locks crossterm's
/// input reader, which the dropped stream's reader thread holds while it
/// waits for input; the lock is granted only once that thread has been
/// woken by the drop and has stopped reading. The new stream reads nothing
/// until it is polled.
async fn rebuild_event_stream() -> EventStream {
    tokio::task::spawn_blocking(EventStream::new)
        .await
        // A blocking task is cancelled only at runtime shutdown, which drops
        // this future first, so an error here is a panic in the constructor.
        .unwrap_or_else(|error| std::panic::resume_unwind(error.into_panic()))
}

/// Spawns `command` with inherited stdio in scpto's process group and waits
/// for it, ignoring SIGINT and SIGQUIT in scpto meanwhile.
async fn run_child(mut command: Command) -> io::Result<ExitStatus> {
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    // SAFETY: the closure runs in the forked child before exec and only
    // calls signal(2), which is async-signal-safe; it allocates nothing
    // (`last_os_error` builds an OS error code without allocating).
    unsafe {
        command.pre_exec(|| {
            for number in KEYBOARD_SIGNALS {
                if libc::signal(number, libc::SIG_DFL) == libc::SIG_ERR {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let _ignored = IgnoredKeyboardSignals::new()?;
    command.spawn()?.wait().await
}

/// Signals the terminal sends to its foreground process group from the
/// keyboard that would end scpto (or, for SIGINT, reach its handler).
const KEYBOARD_SIGNALS: [libc::c_int; 2] = [libc::SIGINT, libc::SIGQUIT];

/// Sets [`KEYBOARD_SIGNALS`] to `SIG_IGN` for scpto and restores the saved
/// dispositions (tokio's handler for SIGINT) when dropped.
struct IgnoredKeyboardSignals {
    saved: Vec<(libc::c_int, libc::sigaction)>,
}

impl IgnoredKeyboardSignals {
    fn new() -> io::Result<IgnoredKeyboardSignals> {
        let mut ignored = IgnoredKeyboardSignals {
            saved: Vec::with_capacity(KEYBOARD_SIGNALS.len()),
        };
        for number in KEYBOARD_SIGNALS {
            // SAFETY: an all-zero `sigaction` is a valid value (no flags,
            // empty mask, handler 0), and both structs outlive the call.
            unsafe {
                let mut ignore: libc::sigaction = std::mem::zeroed();
                ignore.sa_sigaction = libc::SIG_IGN;
                let mut previous: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(number, &ignore, &mut previous) != 0 {
                    // Dropping `ignored` restores the signals changed so far.
                    return Err(io::Error::last_os_error());
                }
                ignored.saved.push((number, previous));
            }
        }
        Ok(ignored)
    }
}

impl Drop for IgnoredKeyboardSignals {
    fn drop(&mut self) {
        for (number, previous) in &self.saved {
            // SAFETY: `previous` was returned by sigaction for this signal.
            // sigaction fails only for an invalid signal number or for
            // SIGKILL/SIGSTOP, so restoring a disposition it just reported
            // cannot fail.
            unsafe {
                libc::sigaction(*number, previous, std::ptr::null_mut());
            }
        }
    }
}

/// Whether `$TMUX` says scpto runs inside tmux.
fn inside_tmux(tmux: Option<&OsStr>) -> bool {
    tmux.is_some_and(|value| !value.is_empty())
}

/// Whether the terminal answers a device status report in time.
fn terminal_answers() -> bool {
    // A failed query means the terminal cannot be queried at all, which is
    // the same outcome for the caller as not answering.
    matches!(query_terminal(STATUS_REQUEST), Ok(Some(_)))
}

/// Writes `request` (which must end with [`STATUS_REQUEST`]) to stdout and
/// reads stdin until the status reply arrives. Returns the bytes read, the
/// status reply included, or `None` when the terminal does not answer
/// within [`QUERY_TIMEOUT`]. Reads one byte at a time so that input typed
/// after the reply stays in the terminal for the event reader.
fn query_terminal(request: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        // The reply goes to the terminal, not to a redirected stdin.
        return Ok(None);
    }
    let mut stdout = io::stdout();
    stdout.write_all(request)?;
    stdout.flush()?;

    let deadline = Instant::now() + QUERY_TIMEOUT;
    let mut reply = Vec::new();
    while reply.len() < QUERY_REPLY_LIMIT {
        if !wait_readable(libc::STDIN_FILENO, deadline)? {
            return Ok(None);
        }
        match read_byte(libc::STDIN_FILENO)? {
            Some(byte) => reply.push(byte),
            None => return Ok(None),
        }
        if ends_with_status_reply(&reply) {
            return Ok(Some(reply));
        }
    }
    Ok(None)
}

/// Waits until `fd` is readable or `deadline` passes; `false` on timeout.
/// A wait interrupted by a signal is resumed with the time left.
fn wait_readable(fd: libc::c_int, deadline: Instant) -> io::Result<bool> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        // Round up so a sub-millisecond remainder still waits.
        let millis = remaining.as_micros().div_ceil(1000);
        let millis = libc::c_int::try_from(millis).unwrap_or(libc::c_int::MAX);
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `poll_fd` is a valid pollfd array of length 1 for the call.
        match unsafe { libc::poll(&mut poll_fd, 1, millis) } {
            -1 => {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
            0 => return Ok(false),
            _ => return Ok(true),
        }
    }
}

/// Reads one byte from `fd` after [`wait_readable`] reported data; `None`
/// at end of file or when a non-blocking `fd` has no data. Bypasses `std::io::Stdin`, whose
/// buffer would take bytes away from crossterm's reader.
fn read_byte(fd: libc::c_int) -> io::Result<Option<u8>> {
    let mut byte = 0u8;
    loop {
        // SAFETY: `byte` is a valid one-byte buffer for the call.
        let read = unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) };
        match read {
            1 => return Ok(Some(byte)),
            0 => return Ok(None),
            _ => {
                let error = io::Error::last_os_error();
                match error.kind() {
                    io::ErrorKind::Interrupted => continue,
                    io::ErrorKind::WouldBlock => return Ok(None),
                    _ => return Err(error),
                }
            }
        }
    }
}

/// Whether `bytes` ends with a device status reply (`CSI 0 n` for OK,
/// `CSI 3 n` for a malfunction report).
fn ends_with_status_reply(bytes: &[u8]) -> bool {
    bytes.ends_with(b"\x1b[0n") || bytes.ends_with(b"\x1b[3n")
}

/// Extracts the color from an OSC 11 reply such as
/// `ESC ] 11 ; rgb:ffff/ffff/ffff ESC \` (or BEL-terminated, or `rgba:`)
/// found anywhere in `bytes`.
fn parse_background_reply(bytes: &[u8]) -> Option<(u8, u8, u8)> {
    const PREFIX: &[u8] = b"\x1b]11;";
    let start = bytes
        .windows(PREFIX.len())
        .position(|window| window == PREFIX)?
        + PREFIX.len();
    let rest = &bytes[start..];
    let end = rest.iter().position(|&byte| byte == 0x07 || byte == 0x1b)?;
    let spec = std::str::from_utf8(&rest[..end]).ok()?;
    let components = spec
        .strip_prefix("rgb:")
        .or_else(|| spec.strip_prefix("rgba:"))?;
    let mut channels = components.split('/').map(scale_hex_channel);
    let red = channels.next()??;
    let green = channels.next()??;
    let blue = channels.next()??;
    Some((red, green, blue))
}

/// Scales an X11 color channel of one to four hex digits to `0..=255`.
fn scale_hex_channel(digits: &str) -> Option<u8> {
    if digits.is_empty() || digits.len() > 4 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(digits, 16).ok()?;
    let max = (1u32 << (4 * digits.len())) - 1;
    u8::try_from((value * 255 + max / 2) / max).ok()
}

/// Light when the relative luminance (Rec. 709 weights) is at least half.
fn classify_background((red, green, blue): (u8, u8, u8)) -> Background {
    let luminance = 0.2126 * f64::from(red) + 0.7152 * f64::from(green) + 0.0722 * f64::from(blue);
    if luminance >= 127.5 {
        Background::Light
    } else {
        Background::Dark
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmux_is_detected_only_from_a_non_empty_variable() {
        assert!(inside_tmux(Some(OsStr::new(
            "/private/tmp/tmux-501/default,1234,0"
        ))));
        assert!(!inside_tmux(Some(OsStr::new(""))));
        assert!(!inside_tmux(None));
    }

    #[test]
    fn status_reply_is_recognized_at_the_end_only() {
        assert!(ends_with_status_reply(b"\x1b[0n"));
        assert!(ends_with_status_reply(b"\x1b]11;rgb:0/0/0\x07\x1b[0n"));
        assert!(ends_with_status_reply(b"\x1b[3n"));
        assert!(!ends_with_status_reply(b"\x1b[0n\x1b"));
        assert!(!ends_with_status_reply(b"\x1b[1n"));
        assert!(!ends_with_status_reply(b""));
    }

    #[test]
    fn background_reply_with_st_terminator_and_four_digits() {
        let reply = b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b[0n";
        assert_eq!(parse_background_reply(reply), Some((255, 255, 255)));
    }

    #[test]
    fn background_reply_with_bel_terminator() {
        let reply = b"\x1b]11;rgb:1c1c/1c1c/1c1c\x07\x1b[0n";
        assert_eq!(parse_background_reply(reply), Some((28, 28, 28)));
    }

    #[test]
    fn background_reply_with_short_channels_and_alpha() {
        assert_eq!(
            parse_background_reply(b"\x1b]11;rgb:fd/f6/e3\x07"),
            Some((0xfd, 0xf6, 0xe3))
        );
        assert_eq!(
            parse_background_reply(b"\x1b]11;rgb:f/8/0\x07"),
            Some((255, 136, 0))
        );
        assert_eq!(
            parse_background_reply(b"\x1b]11;rgba:0000/2b2b/3636/ffff\x1b\\"),
            Some((0x00, 0x2b, 0x36))
        );
    }

    #[test]
    fn background_reply_after_unrelated_bytes() {
        let reply = b"x\x1b[I\x1b]11;rgb:0000/0000/0000\x07\x1b[0n";
        assert_eq!(parse_background_reply(reply), Some((0, 0, 0)));
    }

    #[test]
    fn unanswered_or_malformed_background_reply_is_none() {
        assert_eq!(parse_background_reply(b"\x1b[0n"), None);
        assert_eq!(parse_background_reply(b"\x1b]11;rgb:ffff/ffff"), None);
        assert_eq!(parse_background_reply(b"\x1b]11;rgb:ffff/ffff\x07"), None);
        assert_eq!(parse_background_reply(b"\x1b]11;#ffffff\x07"), None);
        assert_eq!(parse_background_reply(b"\x1b]11;rgb:fffff/0/0\x07"), None);
        assert_eq!(parse_background_reply(b"\x1b]11;rgb:zz/00/00\x07"), None);
        assert_eq!(parse_background_reply(b"\x1b]11;rgb://\x07"), None);
    }

    #[test]
    fn hex_channels_scale_to_eight_bits() {
        assert_eq!(scale_hex_channel("0"), Some(0));
        assert_eq!(scale_hex_channel("f"), Some(255));
        assert_eq!(scale_hex_channel("80"), Some(128));
        assert_eq!(scale_hex_channel("8000"), Some(128));
        assert_eq!(scale_hex_channel("ffff"), Some(255));
        assert_eq!(scale_hex_channel(""), None);
        assert_eq!(scale_hex_channel("+f"), None);
    }

    #[test]
    fn backgrounds_classify_by_luminance() {
        assert_eq!(classify_background((0, 0, 0)), Background::Dark);
        assert_eq!(classify_background((255, 255, 255)), Background::Light);
        // Solarized dark and light.
        assert_eq!(classify_background((0x00, 0x2b, 0x36)), Background::Dark);
        assert_eq!(classify_background((0xfd, 0xf6, 0xe3)), Background::Light);
        // Pure blue is dark, pure green is light.
        assert_eq!(classify_background((0, 0, 255)), Background::Dark);
        assert_eq!(classify_background((0, 255, 0)), Background::Light);
    }

    #[test]
    fn restore_without_enter_does_nothing() {
        // The terminal is never entered in tests, so restoring must neither
        // write escape sequences nor touch the termios state.
        assert!(!ENTERED.load(Ordering::SeqCst));
        assert!(restore_modes().is_ok());
    }

    #[tokio::test]
    async fn hangup_is_delivered_as_a_signal() {
        let mut signals = Signals::new().expect("register signal handlers");
        // SAFETY: raising a signal whose handler tokio has just installed.
        let raised = unsafe { libc::raise(libc::SIGHUP) };
        assert_eq!(raised, 0);
        let received = tokio::time::timeout(Duration::from_secs(5), signals.recv())
            .await
            .expect("signal arrives");
        assert_eq!(received, TermSignal::Hangup);
    }
}
