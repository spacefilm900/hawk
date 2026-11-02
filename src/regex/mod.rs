//! hawk's own regular-expression engine (POSIX EREs, bytes, C locale).
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
//! The DFAs live in `RefCell`s: they grow while matching, but a `Regex` is
//! shared through `&Regex` / `Rc<Regex>`.  `RefCell` moves Rust's "one
//! writer at a time" check from compile time to run time.

mod dfa;
mod nfa;
mod parse;

use std::cell::RefCell;

use crate::error::Error;
use dfa::{ACCEPT, ACCEPT_END, DEAD, Dfa};
pub use nfa::NFA_LIMIT;
pub use parse::DUP_MAX;
use parse::{Node, parse};

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
}

impl Regex {
    /// Compile an ERE.  Errors read `bad regular expression /src/: why`.
    pub fn new(src: &[u8]) -> Result<Regex, Error> {
        let tree = parse(src)?;
        let size = nfa::size(&tree);
        if size > NFA_LIMIT {
            return Err(Error::Runtime(format!(
                "bad regular expression /{}/: too big (needs more than {NFA_LIMIT} automaton states)",
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
        })
    }

    /// How many NFA states this regex uses.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Does the regex match anywhere in `text`?
    pub fn is_match(&self, text: &[u8]) -> bool {
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

    /// End of the longest match starting exactly at `start`, if any.
    pub fn longest(&self, text: &[u8], start: usize) -> Option<usize> {
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
    budget: usize,
    /// `marks[p]`: a match starts at p (computed when the budget runs out).
    marks: Option<Vec<bool>>,
}

impl<'r, 't> Finder<'r, 't> {
    pub fn new(re: &'r Regex, text: &'t [u8]) -> Finder<'r, 't> {
        Finder {
            re,
            text,
            budget: 2 * text.len() + 64,
            marks: None,
        }
    }

    /// The leftmost-longest match at or after `pos` (`pos <= len`).
    pub fn find(&mut self, pos: usize) -> Option<(usize, usize)> {
        let text = self.text;
        if let Some(lit) = &self.re.literal {
            return find_bytes(text, lit, pos).map(|s| (s, s + lit.len()));
        }
        if self.marks.is_none() {
            match self.probe(pos) {
                Probe::Found(span) => return span,
                Probe::OutOfBudget => self.mark_starts(),
            }
        }
        let marks = self.marks.as_ref().unwrap();
        let start = (pos..=text.len()).find(|&p| marks[p])?;
        let end = self.re.longest(text, start)?;
        Some((start, end))
    }

    /// Try candidate starts left to right with the anchored DFA.
    fn probe(&mut self, pos: usize) -> Probe {
        let text = self.text;
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
    fn mark_starts(&mut self) {
        let text = self.text;
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
}
