//! Terminal events to `Action`s: key mapping per `InputMode`, the mouse
//! state machine (double click within 450 ms on the same row, drag after
//! moving at least one cell, drop and cancel), hit testing against the last
//! rendered layout, and bracketed paste.
