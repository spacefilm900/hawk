//! The text side of sub(), gsub() and split() with a regex: AWK's rules
//! for `&` in replacements and for empty matches.  (The matching itself is
//! in `regex/`.)

use crate::charset;
use crate::regex::{Finder, Regex};

/// One piece of a parsed replacement string.
#[derive(Debug, PartialEq)]
pub enum Part {
    Text(Vec<u8>),
    /// `&`: the matched text.
    Matched,
}

/// Parse a sub/gsub replacement.  A run of backslashes followed by `&`
/// gives one literal backslash per pair, and an odd one left over makes
/// the `&` literal; a run *not* followed by `&` is copied unchanged.  So
/// `\&` is a literal `&`, `\\&` a backslash then the match, and `\\x` stays
/// two backslashes.  This is mawk's rule, pyawk's choice (the majority
/// answer in every case tried).
pub fn expand_replacement(repl: &[u8]) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut buf = Vec::new();
    let n = repl.len();
    let mut i = 0;
    while i < n {
        match repl[i] {
            b'\\' => {
                let mut j = i;
                while j < n && repl[j] == b'\\' {
                    j += 1;
                }
                let run = j - i;
                if j < n && repl[j] == b'&' {
                    buf.extend(std::iter::repeat_n(b'\\', run / 2));
                    if run % 2 == 1 {
                        buf.push(b'&');
                        j += 1; // that & was literal
                    }
                } else {
                    buf.extend(std::iter::repeat_n(b'\\', run));
                }
                i = j;
            }
            b'&' => {
                if !buf.is_empty() {
                    // `std::mem::take` moves the vector out, leaving an
                    // empty one behind.
                    parts.push(Part::Text(std::mem::take(&mut buf)));
                }
                parts.push(Part::Matched);
                i += 1;
            }
            c => {
                buf.push(c);
                i += 1;
            }
        }
    }
    if !buf.is_empty() {
        parts.push(Part::Text(buf));
    }
    parts
}

/// sub()/gsub() core with AWK's empty-match rules; returns the new text
/// and the number of replacements.  An empty match is replaced and then
/// one character copied so the scan moves on, and an empty match right after
/// a non-empty one is skipped: `gsub(/x*/, "-", "abxd")` gives `-a-b-d-`.
pub fn substitute(re: &Regex, repl: &[u8], s: &[u8], global: bool) -> (Vec<u8>, usize) {
    let parts = expand_replacement(repl);
    let mut finder = Finder::new(re, s);
    let mut out = Vec::with_capacity(s.len());
    let n = s.len();
    let mut i = 0;
    let mut count = 0;
    let mut last_end = None;
    while i <= n {
        let Some((st, en)) = finder.find(i) else {
            break;
        };
        if st == en && Some(st) == last_end {
            // An empty match right after the previous match: not allowed.
            if st >= n {
                break;
            }
            let next = charset::next(s, st, re.utf8());
            out.extend_from_slice(&s[i..next]);
            i = next;
            continue;
        }
        out.extend_from_slice(&s[i..st]);
        for p in &parts {
            match p {
                Part::Text(t) => out.extend_from_slice(t),
                Part::Matched => out.extend_from_slice(&s[st..en]),
            }
        }
        count += 1;
        if st == en {
            let next = charset::next(s, st, re.utf8());
            out.extend_from_slice(&s[st..next.min(n)]);
            i = next;
        } else {
            i = en;
            last_end = Some(en);
        }
        if !global {
            break;
        }
    }
    if i < n {
        out.extend_from_slice(&s[i..]);
    }
    (out, count)
}

/// Where each field of `split_regex` sits in `s`: `(start, end)` byte
/// offsets, including empty fields.  An empty `s` has no fields.
pub fn split_regex_spans(re: &Regex, s: &[u8]) -> Vec<(usize, usize)> {
    // `[ ]+` and `:+` are a run of one byte. Scanning for the run is the
    // whole split; the DFA would spend most of its time deciding that.
    if let Some(spans) = re.split_span_fast(s) {
        return spans;
    }
    if s.is_empty() {
        return Vec::new();
    }
    let mut finder = Finder::new(re, s);
    let mut pieces = Vec::new();
    let mut piece_start = 0;
    let mut pos = 0;
    while pos <= s.len() {
        let Some((st, en)) = finder.find(pos) else {
            break;
        };
        if st == en {
            // An empty match is not a separator.  Step one character so
            // a pattern like `x*` cannot loop forever.
            pos = charset::next(s, st, re.utf8());
            continue;
        }
        pieces.push((piece_start, st));
        piece_start = en;
        pos = en;
    }
    pieces.push((piece_start, s.len()));
    pieces
}

/// Split `s` at each non-empty match of `re` (split() and regex FS).
/// An empty `s` has no fields.
pub fn split_regex(re: &Regex, s: &[u8]) -> Vec<Vec<u8>> {
    split_regex_spans(re, s)
        .into_iter()
        .map(|(a, b)| s[a..b].to_vec())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn re(s: &str) -> Regex {
        Regex::new(s.as_bytes()).unwrap()
    }
    fn gsub(p: &str, r: &str, s: &str) -> (String, usize) {
        let (out, n) = substitute(&re(p), r.as_bytes(), s.as_bytes(), true);
        (String::from_utf8(out).unwrap(), n)
    }
    fn sub(p: &str, r: &str, s: &str) -> (String, usize) {
        let (out, n) = substitute(&re(p), r.as_bytes(), s.as_bytes(), false);
        (String::from_utf8(out).unwrap(), n)
    }

    #[test]
    fn replacement_backslashes() {
        let t = |s: &[u8]| Part::Text(s.to_vec());
        assert_eq!(
            expand_replacement(b"<&>"),
            [t(b"<"), Part::Matched, t(b">")]
        );
        assert_eq!(expand_replacement(br"\&"), [t(b"&")]);
        assert_eq!(expand_replacement(br"\\&"), [t(br"\"), Part::Matched]);
        assert_eq!(expand_replacement(br"\\\&"), [t(br"\&")]);
        assert_eq!(expand_replacement(br"\\x"), [t(br"\\x")]);
        assert_eq!(expand_replacement(b"a\\"), [t(b"a\\")]);
    }

    #[test]
    fn empty_match_rules() {
        assert_eq!(gsub("x*", "-", "abxd"), ("-a-b-d-".into(), 4));
        assert_eq!(gsub("b*", "X", "abc"), ("XaXcX".into(), 3));
        assert_eq!(gsub("", "-", ""), ("-".into(), 1));
        assert_eq!(gsub("^", ">", "ab"), (">ab".into(), 1));
        assert_eq!(gsub("$", "<", "ab"), ("ab<".into(), 1));
        assert_eq!(gsub("a|ab", "<&>", "abab"), ("<ab><ab>".into(), 2));
        assert_eq!(gsub("o", "0", "foo boo"), ("f00 b00".into(), 4));
        assert_eq!(sub("o", "0", "foo"), ("f0o".into(), 1));
        assert_eq!(sub("z", "0", "foo"), ("foo".into(), 0));
        assert_eq!(sub("x*", "-", "abc"), ("-abc".into(), 1));
    }

    #[test]
    fn regex_split() {
        let sp = |p: &str, s: &str| -> Vec<String> {
            split_regex(&re(p), s.as_bytes())
                .into_iter()
                .map(|v| String::from_utf8(v).unwrap())
                .collect()
        };
        assert_eq!(sp(",+", "a,,b,"), ["a", "b", ""]);
        assert_eq!(sp(" +", " a b"), ["", "a", "b"]);
        assert_eq!(sp("x*", "abc"), ["abc"]);
        assert_eq!(sp("a|ab", "xabyaz"), ["x", "y", "z"]);
        assert!(sp(",", "").is_empty());
    }
}
