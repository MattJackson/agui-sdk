//! RFC 6901 helpers over pointer text: syntax check, token splitting and
//! (un)escaping. Walking a pointer slices the text and unescapes a token only
//! when it contains `~`, so resolution allocates nothing in the common case.

use std::borrow::Cow;

/// Whether `text` is RFC 6901 syntax: empty, or `/`-prefixed tokens in which
/// every `~` is followed by `0` or `1`.
pub(crate) fn is_valid(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return true;
    }
    if bytes[0] != b'/' {
        return false;
    }
    let mut i = 1;
    while i < bytes.len() {
        if bytes[i] == b'~' {
            match bytes.get(i + 1) {
                Some(b'0') | Some(b'1') => i += 2,
                _ => return false,
            }
        } else {
            i += 1;
        }
    }
    true
}

/// Decodes one token (`~1` to `/`, then `~0` to `~`). Borrowed when the token
/// holds no escape.
pub(super) fn unescape(raw: &str) -> Cow<'_, str> {
    if !raw.contains('~') {
        return Cow::Borrowed(raw);
    }
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '~' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('1') => out.push('/'),
            Some('0') => out.push('~'),
            // Unreachable for validated text; keep the characters rather than panic.
            Some(other) => {
                out.push('~');
                out.push(other);
            }
            None => out.push('~'),
        }
    }
    Cow::Owned(out)
}

/// Appends `/` and `key` encoded as a token (`~` to `~0`, `/` to `~1`).
pub(crate) fn push_token(pointer: &mut String, key: &str) {
    pointer.push('/');
    for c in key.chars() {
        match c {
            '~' => pointer.push_str("~0"),
            '/' => pointer.push_str("~1"),
            c => pointer.push(c),
        }
    }
}

/// Raw (still escaped) tokens of pointer text.
#[derive(Debug, Clone)]
pub(super) struct RawTokens<'a> {
    rest: Option<&'a str>,
}

impl<'a> RawTokens<'a> {
    pub(super) fn new(pointer: &'a str) -> Self {
        RawTokens {
            rest: pointer.strip_prefix('/'),
        }
    }
}

impl<'a> Iterator for RawTokens<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        let rest = self.rest?;
        match rest.find('/') {
            Some(i) => {
                self.rest = Some(&rest[i + 1..]);
                Some(&rest[..i])
            }
            None => {
                self.rest = None;
                Some(rest)
            }
        }
    }
}

/// Number of tokens, which is the depth the pointer addresses.
pub(super) fn depth(pointer: &str) -> usize {
    pointer.bytes().filter(|&b| b == b'/').count()
}

/// Splits non-root pointer text into the parent pointer and the raw last token.
pub(super) fn split_last(pointer: &str) -> Option<(&str, &str)> {
    let i = pointer.rfind('/')?;
    Some((&pointer[..i], &pointer[i + 1..]))
}

/// An array index token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Index {
    At(usize),
    /// `-`: the position after the last element.
    End,
}

/// Parses an array index token: `-`, `0`, or digits without a leading zero.
/// `None` for anything else, including a number too large for `usize`.
pub(super) fn parse_index(token: &str) -> Option<Index> {
    match token.as_bytes() {
        b"-" => Some(Index::End),
        b"0" => Some(Index::At(0)),
        [b'1'..=b'9', rest @ ..] if rest.iter().all(u8::is_ascii_digit) => {
            token.parse().ok().map(Index::At)
        }
        _ => None,
    }
}

/// `prefix` is a proper ancestor of `path`, token-wise (`/a` of `/a/b`, not of `/ab`).
pub(super) fn is_proper_prefix(prefix: &str, path: &str) -> bool {
    path.len() > prefix.len() && path.starts_with(prefix) && path.as_bytes()[prefix.len()] == b'/'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax() {
        for bad in ["a", "/~", "/~2", "/a~", "foo/bar"] {
            assert!(!is_valid(bad), "{bad}");
        }
        for good in ["", "/", "//", "/~0~1", "/a/b/-"] {
            assert!(is_valid(good), "{good}");
        }
    }

    #[test]
    fn unescape_applies_tilde_one_first() {
        assert_eq!(unescape("~01"), "~1");
        assert_eq!(unescape("~10"), "/0");
        assert!(matches!(unescape("plain"), Cow::Borrowed(_)));
        let mut p = String::new();
        push_token(&mut p, "a/~b");
        assert_eq!(p, "/a~1~0b");
    }

    #[test]
    fn indices() {
        assert_eq!(parse_index("0"), Some(Index::At(0)));
        assert_eq!(parse_index("10"), Some(Index::At(10)));
        assert_eq!(parse_index("-"), Some(Index::End));
        for bad in [
            "01",
            "+1",
            "-1",
            "",
            "1a",
            " 1",
            "1.0",
            "99999999999999999999999999",
        ] {
            assert_eq!(parse_index(bad), None, "{bad}");
        }
    }

    #[test]
    fn prefixes_are_token_wise() {
        assert!(is_proper_prefix("/a", "/a/b"));
        assert!(is_proper_prefix("", "/a"));
        assert!(!is_proper_prefix("/a", "/ab"));
        assert!(!is_proper_prefix("/a", "/a"));
    }
}
