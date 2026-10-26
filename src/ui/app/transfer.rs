//! Transfers in the app: building job requests from copy, move, drag and
//! drop, and Finder drops; the queue mirror built from `TransferEvent`s;
//! queue panel controls; and conflict prompts.

/// State owned by this feature; part of [`super::App`].
#[derive(Debug, Default)]
pub struct TransferState {}
