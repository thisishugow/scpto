//! Terminal lifecycle: entering and restoring the alternate screen, raw
//! mode, mouse capture and bracketed paste through crossterm; the
//! `TerminalGuard` and panic hook; SIGTERM/SIGINT/SIGHUP via
//! `tokio::signal::unix`; and the single suspend/resume sequence used to run
//! foreground programs (editor, shell, VPN command, interactive auth).
