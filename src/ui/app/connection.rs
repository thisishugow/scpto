//! Endpoint connections in the app: per-endpoint status (connecting,
//! connected, reconnecting with countdown), endpoint switching from the host
//! picker, interactive auth hand-off, the masked sudo password prompt, and
//! the VPN hint dialog.

/// State owned by this feature; part of [`super::App`].
#[derive(Debug, Default)]
pub struct ConnectionState {}
