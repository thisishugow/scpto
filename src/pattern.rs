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

/// A compiled glob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    chars: Vec<char>,
}

impl Pattern {
    /// Compiles a glob. Every string is a valid pattern; an unterminated
    /// `[` matches itself literally, as in sshto.
    pub fn new(pattern: &str) -> Self {
        Self {
            chars: pattern.chars().collect(),
        }
    }

    /// Whether the whole `subject` matches.
    pub fn matches(&self, subject: &str) -> bool {
        // Each entry records a reachable pattern position after this subject prefix.
        let mut previous = vec![false; self.chars.len() + 1];
        previous[0] = true;
        for ch in subject.chars() {
            let mut next = vec![false; self.chars.len() + 1];
            for index in 0..self.chars.len() {
                if !previous[index] {
                    continue;
                }
                match self.chars[index] {
                    '*' => {
                        previous[index + 1] = true;
                        next[index] = true;
                    }
                    '?' => next[index + 1] = true,
                    '\\' => {
                        if self.chars.get(index + 1) == Some(&ch) {
                            next[index + 2] = true;
                        }
                    }
                    '[' => match bracket(&self.chars, index + 1, ch) {
                        Some((true, end)) => next[end] = true,
                        None if ch == '[' => next[index + 1] = true,
                        _ => {}
                    },
                    literal if literal == ch => next[index + 1] = true,
                    _ => {}
                }
            }
            previous = next;
        }
        for index in 0..self.chars.len() {
            if previous[index] && self.chars[index] == '*' {
                previous[index + 1] = true;
            }
        }
        previous[self.chars.len()]
    }
}

fn bracket(chars: &[char], mut index: usize, ch: char) -> Option<(bool, usize)> {
    let negated = matches!(chars.get(index), Some('!' | '^'));
    index += usize::from(negated);
    let mut first = true;
    loop {
        // A leading closing bracket is a literal and can start a range.
        if chars.get(index) == Some(&']') && !first {
            return Some((negated, index + 1));
        }
        first = false;
        let (matched, end) = if let Some((name, end)) = character_class(chars, index) {
            (matches_class(&name, ch).unwrap_or(false), end)
        } else {
            let (start, mut end) = bracket_literal(chars, index)?;
            let matched = if chars.get(end) == Some(&'-') && chars.get(end + 1) != Some(&']') {
                // A range endpoint is one literal character, even before ":class:]".
                let (last, next) = bracket_literal(chars, end + 1)?;
                end = next;
                (start..=last).contains(&ch)
            } else {
                start == ch
            };
            (matched, end)
        };
        index = end;
        if matched {
            // Bash skips the rest after a match, including complete nested classes.
            // This can consume a different closing bracket than a failed range does.
            return bracket_end(chars, index).map(|end| (!negated, end));
        }
    }
}

fn bracket_end(chars: &[char], mut index: usize) -> Option<usize> {
    while let Some(&ch) = chars.get(index) {
        if ch == ']' {
            return Some(index + 1);
        }
        if let Some((_, end)) = character_class(chars, index) {
            index = end;
        } else {
            index = bracket_literal(chars, index)?.1;
        }
    }
    None
}

fn character_class(chars: &[char], index: usize) -> Option<(String, usize)> {
    if chars.get(index) != Some(&'[') || chars.get(index + 1) != Some(&':') {
        return None;
    }
    let mut end = index + 2;
    while end + 1 < chars.len() {
        if chars[end] == ':' && chars[end + 1] == ']' {
            return Some((chars[index + 2..end].iter().collect(), end + 2));
        }
        end += 1;
    }
    None
}

fn bracket_literal(chars: &[char], index: usize) -> Option<(char, usize)> {
    match chars.get(index)? {
        '\\' => Some((*chars.get(index + 1)?, index + 2)),
        &literal => Some((literal, index + 1)),
    }
}

fn matches_class(name: &str, ch: char) -> Option<bool> {
    Some(match name {
        "alpha" => ch.is_ascii_alphabetic(),
        "digit" => ch.is_ascii_digit(),
        "alnum" => ch.is_ascii_alphanumeric(),
        "upper" => ch.is_ascii_uppercase(),
        "lower" => ch.is_ascii_lowercase(),
        "space" => matches!(ch, ' ' | '\t' | '\n' | '\r' | '\u{b}' | '\u{c}'),
        "blank" => matches!(ch, ' ' | '\t'),
        "punct" => ch.is_ascii_punctuation(),
        "print" => ch.is_ascii_graphic() || ch == ' ',
        "graph" => ch.is_ascii_graphic(),
        "cntrl" => ch.is_ascii_control(),
        "xdigit" => ch.is_ascii_hexdigit(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards_match_characters_instead_of_bytes() {
        assert!(Pattern::new("?").matches("\u{e9}"));
        assert!(Pattern::new("[\u{754c}]").matches("\u{754c}"));
        assert!(Pattern::new("[\u{e9}-\u{ea}]").matches("\u{ea}"));
        assert!(!Pattern::new("??").matches("\u{e9}"));
    }

    #[cfg(unix)]
    #[test]
    fn matches_bash_pattern_oracle() {
        use std::path::Path;
        use std::process::Command;

        if !Path::new("/bin/bash").exists() {
            return;
        }
        let cases = [
            ("", ""),
            ("", "a"),
            ("abc", "abc"),
            ("abc", "ABC"),
            ("*", ""),
            ("*", ".a/b"),
            ("**", "abc"),
            ("a*b*c", "abc"),
            ("a*b*c", "axybzc"),
            ("a*b*c", "acb"),
            ("*ab*ab", "abab"),
            ("*ab*ab", "aba"),
            ("?", ""),
            ("?", "/"),
            ("?", "."),
            ("?", "ab"),
            ("a?c", "abc"),
            ("*?", ""),
            ("*?", "a"),
            ("[abc]", "b"),
            ("[abc]", "d"),
            ("[a-c]", "b"),
            ("[a-c]", "d"),
            ("[0-9]", "7"),
            ("[A-Z]", "M"),
            ("[!a-c]", "b"),
            ("[!a-c]", "z"),
            ("[^a-c]", "a"),
            ("[^a-c]", "z"),
            ("[]a]", "]"),
            ("[]a]", "a"),
            ("[]a]", "b"),
            ("[]-a]", "_"),
            ("[]-a]", "]"),
            ("[]-a]", "a"),
            ("[]-a]", "-"),
            ("[!]-a]", "_"),
            ("[^]-a]", "b"),
            ("[]-]", "]"),
            ("[]-]", "-"),
            ("[]--a]", "a"),
            ("[]--a]", "]"),
            ("[!]]", "]"),
            ("[!]]", "a"),
            ("[^]]", "a"),
            ("[-a]", "-"),
            ("[a-]", "-"),
            ("[a-b-c]", "-"),
            ("[a-b-c]", "c"),
            ("[z-a]", "z"),
            ("[!z-a]", "z"),
            ("[[]", "["),
            ("[[^]x", "^x"),
            ("[", "["),
            ("[]", "[]"),
            ("[!]", "[!]"),
            ("a[bc", "a[bc"),
            ("[*", "[anything"),
            ("[[:alpha:]", "[a"),
            ("[[:alpha:]]", "a"),
            ("[[:alpha:]]", "Z"),
            ("[[:alpha:]]", "2"),
            ("[[:digit:]]", "2"),
            ("[[:digit:]]", "a"),
            ("[[:alnum:]]", "a"),
            ("[[:alnum:]]", "2"),
            ("[[:alnum:]]", "-"),
            ("[[:upper:]]", "A"),
            ("[[:upper:]]", "a"),
            ("[[:lower:]]", "a"),
            ("[[:lower:]]", "A"),
            ("[[:space:]]", " "),
            ("[[:space:]]", "\t"),
            ("[[:space:]]", "\n"),
            ("[[:space:]]", "\r"),
            ("[[:space:]]", "\u{b}"),
            ("[[:space:]]", "\u{c}"),
            ("[[:space:]]", "a"),
            ("[[:blank:]]", " "),
            ("[[:blank:]]", "\t"),
            ("[[:blank:]]", "\n"),
            ("[[:punct:]]", "!"),
            ("[[:punct:]]", "_"),
            ("[[:punct:]]", "a"),
            ("[[:print:]]", " "),
            ("[[:print:]]", "a"),
            ("[[:print:]]", "\t"),
            ("[[:graph:]]", "a"),
            ("[[:graph:]]", "!"),
            ("[[:graph:]]", " "),
            ("[[:cntrl:]]", "\n"),
            ("[[:cntrl:]]", "\u{7f}"),
            ("[[:cntrl:]]", " "),
            ("[[:xdigit:]]", "f"),
            ("[[:xdigit:]]", "A"),
            ("[[:xdigit:]]", "9"),
            ("[[:xdigit:]]", "g"),
            ("[![:digit:]]", "a"),
            ("[^[:digit:]]", "1"),
            ("[[:alpha:][:digit:]_-]", "_"),
            ("[[:alpha:][:digit:]_-]", "7"),
            ("[[:digit:]-z]", "-"),
            ("[a[:digit:]]", "a"),
            ("[a-[:digit:]]", "a"),
            ("[a-[:alpha:]]", "a]"),
            ("[a-[:alpha:]]", ":]"),
            ("[a-[:alpha:]]", "b]"),
            ("[!a-[:digit:]]", "a]"),
            ("[!a-[:digit:]]", "g]"),
            ("[A-[:alpha:]]", "Z]"),
            ("[A-[:alpha:]]", "[]"),
            ("[ba-[:alpha:]]", "b"),
            ("[ba-[:alpha:]]", "b]"),
            ("[ba-[:alpha:]]", "a"),
            ("[ba-[:alpha:]]", "a]"),
            (r"\*", "*"),
            (r"\*", "abc"),
            (r"\?", "?"),
            (r"\[abc\]", "[abc]"),
            (r"a\b", "ab"),
            (r"\\", "\\"),
            (r"a\", "a"),
            (r"[\]]", "]"),
            (r"[\!a]", "!"),
            (r"[a\-c]", "-"),
            (r"[a\-c]", "b"),
            (r"[a-\c]", "b"),
            (r"[\\]", "\\"),
        ];
        assert!(cases.len() >= 60);
        for (pattern, subject) in cases {
            let status = Command::new("/bin/bash")
                .args(["-c", "[[ $1 == $2 ]]", "_", subject, pattern])
                .env("LC_ALL", "C")
                .env_remove("BASH_ENV")
                .env_remove("BASHOPTS")
                .env_remove("SHELLOPTS")
                .status()
                .unwrap();
            assert!(matches!(status.code(), Some(0 | 1)), "{status:?}");
            assert_eq!(
                Pattern::new(pattern).matches(subject),
                status.success(),
                "pattern={pattern:?}, subject={subject:?}"
            );
        }
    }
}
