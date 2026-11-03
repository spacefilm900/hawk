//! hawk's own regular-expression engine (POSIX EREs, over bytes in the C
//! locale and over characters in UTF-8 mode).
//!
//! The pipeline is the classic one, as in pyawk's `regex.py`:
//!
//! ```text
//! ERE text --parse--> syntax tree --nfa--> Thompson NFA --dfa--> lazy DFA
//! ```
//!
//! Matching is *leftmost-longest* as POSIX requires: of all matches the one
//! starting first, and of those the longest.  Every scan reads each byte at
//! most a constant number of times per search, so there is no backtracking
//! blow-up (`(a*)*b` on a long run of `a`s is linear).
//!
//! How a search works ([`Finder::find`]):
//!
//! 1. *Probe*: try the candidate start positions left to right (skipping
//!    bytes that cannot begin a match) with the anchored DFA.  This is fast
//!    in practice but could be quadratic, so it runs on a budget of
//!    `2n + 64` bytes per text, shared by all searches in that text.
//! 2. When the budget runs out, one backwards scan with the DFA of the
//!    *reversed* pattern marks every position where a match starts; the
//!    first mark at or after `pos` is the leftmost start.
//! 3. From the start, the anchored DFA finds the longest match.
//!
//! UTF-8 mode changes only the parser (see `parse.rs`): each set of
//! characters becomes an alternation of UTF-8 byte sequences, so the
//! automata still read bytes, and a match of a valid sequence always
//! starts and ends on character boundaries (a valid sequence never starts
//! with a continuation byte).  Invalid bytes need one more step: a *raw*
//! pattern (one that can match an invalid byte) is run on a copy of the
//! text in which each invalid byte b is written as the three-byte form of
//! U+DC00+b, and the positions are mapped back ([`Transcoded`]).  Text
//! that is valid UTF-8, the usual case, is used as it is.
//!
//! The DFAs live in `RefCell`s: they grow while matching, but a `Regex` is
//! shared through `&Regex` / `Rc<Regex>`.  `RefCell` moves Rust's "one
//! writer at a time" check from compile time to run time.

mod dfa;
mod nfa;
mod parse;

use std::cell::RefCell;
use std::rc::Rc;

use crate::error::Error;
use dfa::{ACCEPT, ACCEPT_END, DEAD, Dfa};
pub use nfa::{NFA_LIMIT, limit as nfa_limit};
pub use parse::DUP_MAX;
use parse::{Node, parse_mode};

pub struct Regex {
    /// Anchored forward DFA: the longest match from a given start.
    fwd: RefCell<Dfa>,
    /// Unanchored forward DFA: "is there a match anywhere?".
    scan: RefCell<Dfa>,
    /// Unanchored DFA of the reversed pattern: where matches start.
    rev: RefCell<Dfa>,
    /// Bytes that can begin a match (away from position 0), or None if
    /// the pattern can match the empty string there.
    first: Option<Box<[bool; 256]>>,
    /// A plain string with no operators, matched by direct search.
    literal: Option<Vec<u8>>,
    /// Number of NFA states (for the interpreter's cache limit).
    size: u64,
    /// Compiled in UTF-8 mode (callers step by characters).
    utf8: bool,
    /// UTF-8 mode: can match an invalid byte (see [`Transcoded`]).
    raw: bool,
}

impl Regex {
    /// Compile an ERE for the C locale.  Errors read `bad regular
    /// expression /src/: why`.
    pub fn new(src: &[u8]) -> Result<Regex, Error> {
        Regex::with_mode(src, false)
    }

    /// Compile an ERE, over characters if `utf8`.
    pub fn with_mode(src: &[u8], utf8: bool) -> Result<Regex, Error> {
        let parsed = parse_mode(src, utf8)?;
        let tree = parsed.node;
        let size = nfa::size(&tree);
        let limit = nfa::limit(utf8);
        if size > limit {
            return Err(Error::Runtime(format!(
                "bad regular expression /{}/: too big (needs more than {limit} automaton states)",
                String::from_utf8_lossy(src)
            )));
        }
        let literal = literal_of(&tree);
        let mut fwd = Dfa::new(nfa::Nfa::new(&tree, false), false);
        let d = fwd.initial(false);
        let first = fwd.first_bytes(d).map(Box::new);
        Ok(Regex {
            fwd: RefCell::new(fwd),
            scan: RefCell::new(Dfa::new(nfa::Nfa::new(&tree, false), true)),
            rev: RefCell::new(Dfa::new(nfa::Nfa::new(&tree, true), true)),
            first,
            literal,
            size,
            utf8,
            raw: parsed.raw,
        })
    }

    /// Was this regex compiled in UTF-8 mode?
    pub fn utf8(&self) -> bool {
        self.utf8
    }

    /// The text a raw regex must run on instead of `text`, if any.
    fn transcode(&self, text: &[u8]) -> Option<Rc<Transcoded>> {
        if self.raw {
            Transcoded::new(text).map(Rc::new)
        } else {
            None
        }
    }

    /// How many NFA states this regex uses.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Does the regex match anywhere in `text`?
    pub fn is_match(&self, text: &[u8]) -> bool {
        match self.transcode(text) {
            Some(t) => self.scan_for_match(&t.text),
            None => self.scan_for_match(text),
        }
    }

    /// `is_match` on the text to search (already transcoded if need be):
    /// one forward pass of the DFA, stopping at the first accepting state.
    fn scan_for_match(&self, text: &[u8]) -> bool {
        if let Some(lit) = &self.literal {
            return find_bytes(text, lit, 0).is_some();
        }
        let mut dfa = self.scan.borrow_mut();
        let n = text.len();
        let mut d = dfa.initial(true);
        let mut i = 0;
        // `idle`: the state of "nothing in progress"; while in it, bytes
        // that cannot begin a match can be skipped without stepping.
        let mut idle = dfa.initial(false);
        let mut flushes = dfa.flushes;
        let first = self.first.as_deref();
        loop {
            let f = dfa.flags[d as usize];
            if f & ACCEPT != 0 {
                return true;
            }
            if i == n {
                return f & ACCEPT_END != 0;
            }
            if d == idle
                && let Some(first) = first
            {
                match text[i..].iter().position(|&b| first[b as usize]) {
                    Some(k) => i += k,
                    None => {
                        i = n;
                        continue;
                    }
                }
            }
            d = dfa.next(d, text[i]);
            i += 1;
            if dfa.flushes != flushes {
                // The cache was rebuilt: refresh the remembered id.
                flushes = dfa.flushes;
                let cur = d;
                idle = dfa.initial(false);
                d = cur;
            }
        }
    }

    /// The leftmost-longest match at or after `pos`, as (start, end).
    pub fn find_at(&self, text: &[u8], pos: usize) -> Option<(usize, usize)> {
        Finder::new(self, text).find(pos)
    }

    /// End of the longest match starting exactly at `start`, if any
    /// (`text` is already transcoded if need be).
    fn longest(&self, text: &[u8], start: usize) -> Option<usize> {
        let mut dfa = self.fwd.borrow_mut();
        let n = text.len();
        let mut d = dfa.initial(start == 0);
        let mut last = None;
        let mut j = start;
        loop {
            let f = dfa.flags[d as usize];
            if j == n {
                return if f & ACCEPT_END != 0 { Some(j) } else { last };
            }
            if f & ACCEPT != 0 {
                last = Some(j);
            }
            if f & DEAD != 0 {
                return last;
            }
            d = dfa.next(d, text[j]);
            j += 1;
        }
    }
}

/// Repeated searches in one text (gsub, split, FS), sharing the probe
/// budget and the start marks between them.
pub struct Finder<'r, 't> {
    re: &'r Regex,
    text: &'t [u8],
    /// The text to search instead, for a raw regex on invalid UTF-8.
    /// `Rc` (a shared, reference-counted pointer) lets a [`Transcoding`]
    /// cache hand the same copy to many finders.
    trans: Option<Rc<Transcoded>>,
    budget: usize,
    /// `marks[p]`: a match starts at p (computed when the budget runs out).
    marks: Option<Vec<bool>>,
}

impl<'r, 't> Finder<'r, 't> {
    pub fn new(re: &'r Regex, text: &'t [u8]) -> Finder<'r, 't> {
        let trans = re.transcode(text);
        Finder::with_transcoded(re, text, trans)
    }

    /// Like `new`, for a text that is searched again and again (a record
    /// reader's buffer, once per record): the transcoded copy a raw regex
    /// needs is made once and kept in `cache`, which the caller must
    /// clear whenever `text` changes.
    pub fn cached(re: &'r Regex, text: &'t [u8], cache: &mut Transcoding) -> Finder<'r, 't> {
        let trans = if re.raw { cache.get(text) } else { None };
        Finder::with_transcoded(re, text, trans)
    }

    fn with_transcoded(
        re: &'r Regex,
        text: &'t [u8],
        trans: Option<Rc<Transcoded>>,
    ) -> Finder<'r, 't> {
        let n = trans.as_ref().map_or(text.len(), |t| t.text.len());
        Finder {
            re,
            text,
            trans,
            budget: 2 * n + 64,
            marks: None,
        }
    }

    /// The leftmost-longest match at or after `pos` (`pos <= len`).
    pub fn find(&mut self, mut pos: usize) -> Option<(usize, usize)> {
        if self.re.utf8 {
            // Matches start on characters (see the top of this file).
            pos = crate::charset::round_up(self.text, pos);
        }
        // Cloning the `Rc` copies a pointer, not the text; holding our own
        // pointer lets `self` be borrowed mutably meanwhile.
        let Some(t) = self.trans.clone() else {
            return self.find_in(self.text, pos);
        };
        let span = self.find_in(&t.text, t.to_new[pos]);
        span.map(|(a, b)| (t.to_old[a], t.to_old[b]))
    }

    /// `find` on the text to search (the transcoded one, if any): first a
    /// direct search for a literal, then probes from each start within
    /// the budget, and past it the precomputed start marks.
    fn find_in(&mut self, text: &[u8], pos: usize) -> Option<(usize, usize)> {
        if let Some(lit) = &self.re.literal {
            return find_bytes(text, lit, pos).map(|s| (s, s + lit.len()));
        }
        if self.marks.is_none() {
            match self.probe(text, pos) {
                Probe::Found(span) => return span,
                Probe::OutOfBudget => self.mark_starts(text),
            }
        }
        let marks = self.marks.as_ref().unwrap();
        let start = (pos..=text.len()).find(|&p| marks[p])?;
        let end = self.re.longest(text, start)?;
        Some((start, end))
    }

    /// Try candidate starts left to right with the anchored DFA.
    fn probe(&mut self, text: &[u8], pos: usize) -> Probe {
        let n = text.len();
        if self.budget == 0 {
            return Probe::OutOfBudget;
        }
        let mut dfa = self.re.fwd.borrow_mut();
        let first = self.re.first.as_deref();
        let mut i = pos;
        loop {
            // The candidate: position 0 and the end are always tried (`^`
            // and `$` change what can match there).
            let c = match first {
                Some(first) if i != 0 && i != n => text[i..]
                    .iter()
                    .position(|&b| first[b as usize])
                    .map_or(n, |k| i + k),
                _ => i,
            };
            let mut d = dfa.initial(c == 0);
            let mut last = None;
            let mut j = c;
            let stop = n.min(c + self.budget);
            loop {
                let f = dfa.flags[d as usize];
                if j >= stop {
                    if j == n {
                        if f & ACCEPT_END != 0 {
                            last = Some(j);
                        }
                        break;
                    }
                    self.budget = 0;
                    return Probe::OutOfBudget;
                }
                if f & ACCEPT != 0 {
                    last = Some(j);
                }
                if f & DEAD != 0 {
                    break;
                }
                d = dfa.next(d, text[j]);
                j += 1;
            }
            let cost = j - c + 1;
            self.budget = self.budget.saturating_sub(cost);
            if let Some(end) = last {
                return Probe::Found(Some((c, end)));
            }
            if c >= n {
                return Probe::Found(None);
            }
            if self.budget == 0 {
                return Probe::OutOfBudget;
            }
            i = c + 1;
        }
    }

    /// One backwards pass with the reversed pattern marks every match start.
    fn mark_starts(&mut self, text: &[u8]) {
        let n = text.len();
        let mut marks = vec![false; n + 1];
        let mut dfa = self.re.rev.borrow_mut();
        // Reading the text backwards, "the start" of the reversed text is
        // the end of the real one.
        let mut d = dfa.initial(true);
        for i in (0..n).rev() {
            if dfa.flags[d as usize] & ACCEPT != 0 {
                marks[i + 1] = true;
            }
            d = dfa.next(d, text[i]);
        }
        if dfa.flags[d as usize] & ACCEPT_END != 0 {
            marks[0] = true;
        }
        self.marks = Some(marks);
    }
}

/// A remembered [`Transcoded`] copy of one text (or the knowledge that
/// the text is valid UTF-8 and needs none), for [`Finder::cached`].  The
/// owner of the text calls `clear` whenever the text changes.
#[derive(Default)]
pub struct Transcoding {
    /// `None`: not worked out yet.  `Some(None)`: the text is valid.
    copy: Option<Option<Rc<Transcoded>>>,
}

impl Transcoding {
    /// Forget the copy (the text has changed).
    pub fn clear(&mut self) {
        self.copy = None;
    }

    fn get(&mut self, text: &[u8]) -> Option<Rc<Transcoded>> {
        self.copy
            .get_or_insert_with(|| Transcoded::new(text).map(Rc::new))
            .clone()
    }
}

/// UTF-8 mode: a text that is not valid UTF-8, rewritten for a raw regex
/// with each invalid byte b as the three bytes that encode U+DC00+b
/// (`ED B2 80`..`ED B3 BF`), which is what the pattern's escaped bytes
/// compile to.  Valid characters are copied as they are.
pub struct Transcoded {
    text: Vec<u8>,
    /// `to_new[i]`: where offset i of the original text is in `text` (an
    /// offset inside a character maps to the end of that character).
    to_new: Vec<usize>,
    /// `to_old[j]`: the original offset of the character at offset j of
    /// `text` (matches only start and end on characters).
    to_old: Vec<usize>,
}

impl Transcoded {
    /// None if `text` is valid UTF-8 (nothing to rewrite).
    fn new(text: &[u8]) -> Option<Transcoded> {
        if std::str::from_utf8(text).is_ok() {
            return None;
        }
        let mut t = Transcoded {
            text: Vec::with_capacity(text.len() + 16),
            to_new: vec![0; text.len() + 1],
            to_old: Vec::with_capacity(text.len() + 17),
        };
        let mut i = 0;
        while i < text.len() {
            let len = crate::charset::char_len(text, i);
            let at = t.text.len();
            if len == 1 && text[i] >= 0x80 {
                crate::charset::utf8_bytes(
                    crate::charset::ESCAPED_BASE + text[i] as u32,
                    &mut t.text,
                );
            } else {
                t.text.extend_from_slice(&text[i..i + len]);
            }
            // Offsets inside a character map to the next one (a search
            // from there must not find a match at the character's start).
            t.to_new[i] = at;
            for k in 1..len {
                t.to_new[i + k] = t.text.len();
            }
            t.to_old.resize(t.text.len(), i);
            i += len;
        }
        t.to_new[text.len()] = t.text.len();
        t.to_old.push(text.len());
        Some(t)
    }
}

enum Probe {
    /// The answer (a match, or None for no match at all).
    Found(Option<(usize, usize)>),
    /// The budget ran out before the answer was known.
    OutOfBudget,
}

/// The longest literal given to the direct search (see [`find_bytes`]).
const MAX_LITERAL: usize = 32;

/// The bytes of a pattern that is just a short fixed string, if it is one.
fn literal_of(tree: &Node) -> Option<Vec<u8>> {
    let lit: Option<Vec<u8>> = match tree {
        Node::Set(s) => s.only().map(|b| vec![b]),
        Node::Cat(items) => items
            .iter()
            .map(|it| match it {
                Node::Set(s) => s.only(),
                _ => None,
            })
            .collect(),
        _ => None,
    };
    lit.filter(|l| !l.is_empty() && l.len() <= MAX_LITERAL)
}

/// First position at or after `from` where `needle` occurs (non-empty).
/// A scan for the first byte, then a compare: at most `needle.len()` bytes
/// are compared per candidate, so with needles capped at [`MAX_LITERAL`]
/// bytes the work stays linear in the text.
fn find_bytes(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let m = needle.len();
    let first = needle[0];
    let mut i = from;
    while i + m <= hay.len() {
        let k = hay[i..=hay.len() - m].iter().position(|&b| b == first)?;
        i += k;
        if &hay[i..i + m] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn re(s: &str) -> Regex {
        Regex::new(s.as_bytes()).unwrap()
    }
    fn find(p: &str, t: &str) -> Option<(usize, usize)> {
        re(p).find_at(t.as_bytes(), 0)
    }

    #[test]
    fn leftmost_longest() {
        assert_eq!(find("a|ab", "xabc"), Some((1, 3)));
        assert_eq!(find("x*(xy)?", "xxy"), Some((0, 3)));
        assert_eq!(find("(a|ab)(c|bcd)", "abcd"), Some((0, 4)));
        assert_eq!(find("b*", "abc"), Some((0, 0)));
        assert_eq!(find("z", "abc"), None);
        assert_eq!(find("", ""), Some((0, 0)));
        assert_eq!(find("abc", "xxabcabc"), Some((2, 5)));
        assert_eq!(re("b").find_at(b"abcb", 2), Some((3, 4)));
    }

    #[test]
    fn anchors() {
        assert_eq!(find("^a", "aa"), Some((0, 1)));
        assert_eq!(re("^a").find_at(b"aa", 1), None);
        assert_eq!(find("a$", "aa"), Some((1, 2)));
        assert_eq!(find("$", "ab"), Some((2, 2)));
        assert_eq!(find("^$", ""), Some((0, 0)));
        assert_eq!(find("a$", "a\nb"), None);
        assert_eq!(find("x^", "x"), None);
        assert!(re("^$").is_match(b""));
        assert!(!re("^$").is_match(b"a"));
        assert!(re("b$").is_match(b"ab"));
        assert!(!re("^b").is_match(b"ab"));
    }

    #[test]
    fn is_match_and_classes() {
        assert!(re("[[:digit:]]+x").is_match(b"ab12x"));
        assert!(!re("[[:digit:]]+x").is_match(b"ab12"));
        assert!(re("a.c").is_match(b"a\nc"));
        assert!(re("a{3}").is_match(b"baaa"));
        assert!(!re("a{3}").is_match(b"baab"));
        assert!(re("").is_match(b"anything"));
    }

    #[test]
    fn linear_on_pathological_input() {
        // Would take forever with a backtracking engine.
        let text = vec![b'a'; 100_000];
        assert!(!re("(a*)*b").is_match(&text));
        assert_eq!(re("(a*)*b").find_at(&text, 0), None);
        assert_eq!(re("(a|aa)*c").find_at(&text, 0), None);
        // Many matches in one text (as gsub does): each search is linear.
        // (Leftmost-*longest* has to read ahead to know a match is the
        // longest, so `a*b|a` here costs n per match; keep it small.)
        let text = vec![b'a'; 2000];
        let r = re("a*b|a");
        let mut f = Finder::new(&r, &text);
        let (mut pos, mut count) = (0, 0);
        while let Some((_, e)) = f.find(pos) {
            count += 1;
            pos = e;
        }
        assert_eq!(count, 2000);
    }

    #[test]
    fn reverse_marks_agree_with_probe() {
        // Force the reverse path by exhausting the budget first.
        for (p, t) in [
            ("a|ab", "xxabab"),
            ("^ab", "abab"),
            ("b$", "abab"),
            ("x*", "axxb"),
        ] {
            let r = re(p);
            let mut f = Finder::new(&r, t.as_bytes());
            f.budget = 0;
            for pos in 0..=t.len() {
                assert_eq!(f.find(pos), r.find_at(t.as_bytes(), pos), "{p} {t} {pos}");
            }
        }
    }

    #[test]
    fn posix_corners_settled_by_pyawk_v1_1() {
        // [a-c-e] is undefined in POSIX; mawk (and pyawk) read a-c, "-", e.
        assert_eq!(find("[a-c-e]", "d"), None);
        assert_eq!(find("[a-c-e]", "-"), Some((0, 1)));
        assert_eq!(find("[a-c-e]", "e"), Some((0, 1)));
        // a+? is (a+)?, not a lazy a+.
        assert_eq!(find("^a+?$", ""), Some((0, 0)));
        assert_eq!(find("^a+?$", "aa"), Some((0, 2)));
        assert!(Regex::new(b"[z-a]").is_err());
    }

    #[test]
    fn limits() {
        let e = Regex::new(b"(a{1000}){1001}").err().unwrap().to_string();
        assert!(
            e.ends_with("too big (needs more than 1000000 automaton states)"),
            "{e}"
        );
        assert!(Regex::new(b"a{32767}").is_ok());
        assert!(Regex::new(b"a{32768}").is_err());
        // Many DFA states: the cache is flushed and matching still works.
        let r = re("(a|b)*a(a|b){12}");
        let mut text = Vec::new();
        let mut x: u32 = 12345;
        for _ in 0..200_000 {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            text.push(if x >> 16 & 1 == 0 { b'a' } else { b'b' });
        }
        text.extend_from_slice(b"a");
        text.extend(std::iter::repeat_n(b'b', 12));
        assert!(r.is_match(&text));
        assert!(r.scan.borrow().num_states() <= dfa::DFA_LIMIT);
    }

    #[test]
    fn dfa_memory_is_bounded_when_states_are_big() {
        // Each DFA state of this regex holds hundreds of NFA positions, so
        // 9,000 states (under DFA_LIMIT) would take millions of entries; the
        // cache must be flushed on its total size as well.
        let r = re("(a|b)*a(a|b){600}c");
        let mut text = Vec::new();
        let mut x: u32 = 12345;
        for _ in 0..9_000 {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            text.push(if x >> 16 & 1 == 0 { b'a' } else { b'b' });
        }
        // (There is no `c`, so the whole text is scanned.)
        assert!(!r.is_match(&text));
        assert!(r.scan.borrow().stored() <= dfa::KERNEL_LIMIT);
        assert!(r.scan.borrow().flushes > 0);
    }

    fn ure(s: &[u8]) -> Regex {
        Regex::with_mode(s, true).unwrap()
    }

    #[test]
    fn utf8_matches_whole_characters() {
        let t = "xéy".as_bytes();
        assert_eq!(ure(b"x.y").find_at(t, 0), Some((0, 4)));
        assert_eq!(re("x.y").find_at(t, 0), None); // C mode: é is 2 bytes
        assert_eq!(
            ure("é+".as_bytes()).find_at("aééb".as_bytes(), 0),
            Some((1, 5))
        );
        assert_eq!(ure(b"[^x]").find_at(t, 0), Some((1, 3)));
        // A search from inside a character finds the next one.
        assert_eq!(ure(b"[^x]").find_at(t, 2), Some((3, 4)));
        assert!(ure(b"^.{3}$").is_match(t));
    }

    #[test]
    fn utf8_invalid_bytes_match_only_their_escape() {
        let t = b"a\xe9b\xc3\xa9";
        // `.` and `[^a]` skip the invalid byte; the escape finds it.
        assert_eq!(ure(b"[^a]").find_at(t, 0), Some((2, 3)));
        assert_eq!(ure(b".b").find_at(t, 0), None);
        assert_eq!(ure(br"\351").find_at(t, 0), Some((1, 2)));
        assert_eq!(ure(br"\351b\303\251").find_at(t, 0), Some((1, 5)));
        assert!(ure(br"a\351").is_match(t));
        assert!(!ure(br"a\351").is_match(b"a\xc3\xa9"));
        // A raw byte in the pattern is the same as its escape.
        assert_eq!(ure(b"\xe9").find_at(t, 0), Some((1, 2)));
        // Positions map back across the rewritten text.
        let r = ure(br"[\200-\377]");
        let mut f = Finder::new(&r, b"\xff\xc3\xa9\xfe");
        assert_eq!(f.find(0), Some((0, 1)));
        assert_eq!(f.find(1), Some((3, 4)));
        assert_eq!(f.find(2), Some((3, 4)));
        assert_eq!(f.find(4), None);
        // A character set is many byte states, so UTF-8 mode allows more.
        assert!(Regex::with_mode(b".{32767}", true).is_ok());
        let err = Regex::with_mode(b"[[:alpha:]]{4000}", true).err().unwrap();
        assert!(
            err.to_string()
                .ends_with("too big (needs more than 4000000 automaton states)")
        );
        // An empty match inside a character is never reported.
        let r = ure(br"\351*");
        let mut f = Finder::new(&r, "é".as_bytes());
        assert_eq!(f.find(1), Some((2, 2)));
    }
}
