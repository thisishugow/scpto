//! Terminal lifecycle: entering and restoring the alternate screen, raw
//! mode, mouse capture and bracketed paste through crossterm (always via
//! the `ratatui::crossterm` re-export); the `TerminalGuard` and panic hook;
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
//! Suspend/resume (plan 7.2), all inside `run_foreground`:
//! 1. drop the caller's `EventStream` and wait until its reader has stopped,
//!    so no key typed for the child is stolen;
//! 2. disable raw mode, mouse capture, bracketed paste; leave the alternate
//!    screen; show the cursor;
//! 3. spawn the command in the terminal's foreground process group with
//!    inherited stdio and wait for it (SIGINT reaches only the child; scpto
//!    keeps running and keeps its connections);
//! 4. re-enter, put a fresh `EventStream` into the caller's slot, and clear
//!    the terminal so the next draw is a full redraw.
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
//!     /// Steps 1-4 above. Returns the child's exit status; an `Err` is a
//!     /// spawn or terminal failure (the terminal is re-entered either way
//!     /// when possible).
//!     pub async fn run_foreground(
//!         &mut self,
//!         events: &mut EventStream,
//!         command: tokio::process::Command,
//!     ) -> std::io::Result<std::process::ExitStatus>;
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
