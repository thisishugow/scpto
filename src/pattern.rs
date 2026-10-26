//! Shell glob matching (`*`, `?`, `[...]`), ported from sshto, used for
//! sshto group rules, `vpn` rules, and ssh `Include` globs.
//!
//! The port keeps sshto's semantics and tests: `*` and `?` also match `/`
//! and a leading `.`; `\` escapes the next character; brackets support
//! ranges, negation (`!` and `^`), and POSIX classes such as
//! `[[:alpha:]]`. Matching is case-sensitive; callers that want sshto's
//! case-insensitive rules lowercase the subject before matching.
//!
//! # Public API (Wave 2)
//!
//! ```text
//! /// A compiled glob.
//! #[derive(Debug, Clone, PartialEq, Eq)]
//! pub struct Pattern { /* private */ }
//!
//! impl Pattern {
//!     /// Compiles a glob. Every string is a valid pattern; an unterminated
//!     /// `[` matches itself literally, as in sshto.
//!     pub fn new(pattern: &str) -> Pattern;
//!     /// Whether the whole `subject` matches.
//!     pub fn matches(&self, subject: &str) -> bool;
//! }
//! ```
