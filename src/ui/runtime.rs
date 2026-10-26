//! Event loop: merges the crossterm `EventStream`, background events, and
//! signals; feeds the app; dispatches effects to executors; and redraws only
//! when state changed, coalesced to at most 60 frames per second.
