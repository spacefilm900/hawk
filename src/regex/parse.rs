//! ERE parser: turns the text of a POSIX extended regular expression into a
//! small syntax tree ([`Node`]).  It follows pyawk's `regex.py`
//! (enhancements branch) rule for rule, including its error messages.
//!
//! Summary of the dialect (bytes, C locale):
//!
//! * `^` and `$` are anchors wherever they appear; `$` means the very end
//!   of the text, and `.` / negated brackets also match a newline.
//! * `{n}`, `{n,}`, `{n,m}` are intervals (counts up to 32767); a `{` not
//!   followed by a digit is a literal `{`.
//! * Each postfix operator applies to everything before it: `a+?` is
//!   `(a+)?`.  An operator with nothing to repeat is an error, except
//!   right after `^`, where `^*` and `^?` vanish and `^+` is just `^`.
//! * An empty alternative (`a||b`, `|a`) is an error, but `()` is allowed.
//!   An unmatched `)` is a literal; an unmatched `(` is an error.
//! * Escapes: `\n \t \r \f \v \a \b`, octal `\ddd`, hex `\xhh`; any other
//!   escaped byte is itself, and a trailing `\` is a literal backslash.
//!
//! UTF-8 seam: everything here works on *bytes* and a bracket expression
//! becomes a [`ByteSet`].  A later UTF-8 mode would parse code points and
//! turn each code-point set into an alternation of UTF-8 byte sequences, so
//! the automata in `nfa.rs` and `dfa.rs` could stay byte-based unchanged.

use crate::error::Error;

/// Largest repeat count in an interval (POSIX's RE_DUP_MAX).
pub const DUP_MAX: u32 = 32767;

/// A set of bytes, one bit per possible byte value (4 x 64 = 256 bits).
/// `#[derive(...)]` asks the compiler to write the listed standard
/// methods (copying, comparing, hashing, debug printing) for us.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct ByteSet(pub [u64; 4]);

impl ByteSet {
    pub fn single(b: u8) -> ByteSet {
        let mut s = ByteSet::default();
        s.insert(b);
        s
    }
    pub fn full() -> ByteSet {
        ByteSet([u64::MAX; 4])
    }
    pub fn insert(&mut self, b: u8) {
        self.0[(b >> 6) as usize] |= 1 << (b & 63);
    }
    pub fn insert_range(&mut self, lo: u8, hi: u8) {
        for b in lo..=hi {
            self.insert(b);
        }
    }
    pub fn contains(&self, b: u8) -> bool {
        self.0[(b >> 6) as usize] & (1 << (b & 63)) != 0
    }
    pub fn union(&mut self, other: &ByteSet) {
        for i in 0..4 {
            self.0[i] |= other.0[i];
        }
    }
    pub fn negate(&mut self) {
        for w in &mut self.0 {
            *w = !*w;
        }
    }
    pub fn len(&self) -> u32 {
        self.0.iter().map(|w| w.count_ones()).sum()
    }
    /// The only member, if there is exactly one.
    pub fn only(&self) -> Option<u8> {
        if self.len() != 1 {
            return None;
        }
        (0..=255u8).find(|&b| self.contains(b))
    }
}

/// The syntax tree.  `Box<Node>` is a pointer to a heap-allocated node,
/// needed because a type cannot contain itself directly.
#[derive(Clone, PartialEq, Debug)]
pub enum Node {
    /// One byte from the set.
    Set(ByteSet),
    /// The items one after another.
    Cat(Vec<Node>),
    /// Any one of the branches.
    Alt(Vec<Node>),
    /// `node` repeated `min` to `max` times (`None`: no upper bound).
    Repeat(Box<Node>, u32, Option<u32>),
    /// `^`: start of the text.
    Bol,
    /// `$`: end of the text.
    Eol,
    /// Matches the empty string.
    Empty,
}

/// Parse `src`; errors read `bad regular expression /src/: why`.
pub fn parse(src: &[u8]) -> Result<Node, Error> {
    let mut p = Parser {
        s: src,
        i: 0,
        depth: 0,
    };
    let r = p.alt();
    match r {
        Ok(node) if p.i == src.len() => Ok(node),
        // Only an unmatched `)` at depth 0 could stop `alt` early, and that
        // is a literal, so this cannot happen; be safe anyway.
        Ok(_) => Err(bad(src, "unexpected )")),
        Err(why) => Err(bad(src, &why)),
    }
}

fn bad(src: &[u8], why: &str) -> Error {
    Error::Runtime(format!(
        "bad regular expression /{}/: {why}",
        String::from_utf8_lossy(src)
    ))
}

/// The POSIX character class `[:name:]`, or None if unknown.
fn class(name: &[u8]) -> Option<ByteSet> {
    let mut s = ByteSet::default();
    let mut add = |ranges: &[(u8, u8)]| {
        for &(lo, hi) in ranges {
            s.insert_range(lo, hi);
        }
    };
    match name {
        b"alpha" => add(&[(b'a', b'z'), (b'A', b'Z')]),
        b"digit" => add(&[(b'0', b'9')]),
        b"alnum" => add(&[(b'a', b'z'), (b'A', b'Z'), (b'0', b'9')]),
        b"upper" => add(&[(b'A', b'Z')]),
        b"lower" => add(&[(b'a', b'z')]),
        // space: " \t\n\r\f\v" (tab..carriage return is 9..13).
        b"space" => add(&[(b' ', b' '), (9, 13)]),
        b"blank" => add(&[(b' ', b' '), (b'\t', b'\t')]),
        b"punct" => add(&[(b'!', b'/'), (b':', b'@'), (b'[', b'`'), (b'{', b'~')]),
        b"xdigit" => add(&[(b'0', b'9'), (b'a', b'f'), (b'A', b'F')]),
        b"cntrl" => add(&[(0, 31), (127, 127)]),
        b"print" => add(&[(32, 126)]),
        b"graph" => add(&[(33, 126)]),
        _ => return None,
    }
    Some(s)
}

/// What a bracket element turned out to be.
enum Elem {
    Byte(u8),
    Class(ByteSet),
}

/// Parser state.  Errors are plain `String` reasons here; [`parse`] wraps
/// them into the full message.
struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    depth: u32,
}

/// `Result<T, String>`: either a value or an error reason.
type P<T> = Result<T, String>;

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    /// alternation: cat ('|' cat)*
    fn alt(&mut self) -> P<Node> {
        let mut branches = Vec::new();
        // A branch that consumed no text at all (`a||b`, `|a`, `(a|)`) is
        // an error, as in mawk and onetrue-awk; `^*|a` is fine.
        let mut empty_branch = false;
        loop {
            let start = self.i;
            branches.push(self.cat()?);
            empty_branch |= self.i == start;
            if self.peek() != Some(b'|') {
                break;
            }
            self.i += 1;
        }
        if branches.len() == 1 {
            return Ok(branches.pop().unwrap());
        }
        if empty_branch {
            return Err("empty alternative".into());
        }
        // An alternation of single bytes / sets is one set: a|b|[cd].
        if branches.iter().all(|b| matches!(b, Node::Set(_))) {
            let mut all = ByteSet::default();
            for b in &branches {
                if let Node::Set(s) = b {
                    all.union(s);
                }
            }
            return Ok(Node::Set(all));
        }
        Ok(Node::Alt(branches))
    }

    /// concatenation of postfix-repeated atoms.
    fn cat(&mut self) -> P<Node> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == b'|' || (c == b')' && self.depth > 0) {
                break;
            }
            if let Some(atom) = self.atom()? {
                let node = self.postfix(atom)?;
                items.push(node);
            }
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().unwrap(),
            _ => Node::Cat(items),
        })
    }

    /// One atom, or None when `^` swallowed a following `*` / `?`.
    fn atom(&mut self) -> P<Option<Node>> {
        let c = self.s[self.i];
        match c {
            b'(' => {
                self.i += 1;
                self.depth += 1;
                let node = if self.peek() == Some(b')') {
                    Node::Empty
                } else {
                    self.alt()?
                };
                if self.peek() != Some(b')') {
                    return Err("unmatched (".into());
                }
                self.i += 1;
                self.depth -= 1;
                Ok(Some(node))
            }
            b'*' | b'+' | b'?' => Err(format!("{} with nothing to repeat", c as char)),
            b'{' if self.interval_at(self.i)?.is_some() => Err("{ with nothing to repeat".into()),
            b'^' => {
                self.i += 1;
                // Operators right after ^: `^*` and `^?` vanish, `^+` is ^.
                let mut node = Some(Node::Bol);
                while let Some(op @ (b'*' | b'+' | b'?')) = self.peek() {
                    self.i += 1;
                    if op != b'+' {
                        node = None;
                    }
                }
                Ok(node)
            }
            b'$' => {
                self.i += 1;
                Ok(Some(Node::Eol))
            }
            b'.' => {
                self.i += 1;
                Ok(Some(Node::Set(ByteSet::full())))
            }
            b'[' => {
                self.i += 1;
                Ok(Some(Node::Set(self.bracket()?)))
            }
            b'\\' => Ok(Some(Node::Set(ByteSet::single(self.escape())))),
            _ => {
                self.i += 1;
                Ok(Some(Node::Set(ByteSet::single(c))))
            }
        }
    }

    /// Apply any postfix operators after an atom.
    fn postfix(&mut self, mut node: Node) -> P<Node> {
        loop {
            match self.peek() {
                Some(b'*') => {
                    self.i += 1;
                    node = repeat(node, 0, None);
                }
                Some(b'+') => {
                    self.i += 1;
                    node = repeat(node, 1, None);
                }
                Some(b'?') => {
                    self.i += 1;
                    node = repeat(node, 0, Some(1));
                }
                Some(b'{') => match self.interval_at(self.i)? {
                    Some((lo, hi, end)) => {
                        self.i = end;
                        node = repeat(node, lo, hi);
                    }
                    None => return Ok(node),
                },
                _ => return Ok(node),
            }
        }
    }

    /// Parse an interval starting at `at` (a `{`).  Returns None when it is
    /// a literal `{`, else (min, max, index after the `}`).
    fn interval_at(&self, at: usize) -> P<Option<(u32, Option<u32>, usize)>> {
        let s = self.s;
        let digits = |mut k: usize| {
            while k < s.len() && s[k].is_ascii_digit() {
                k += 1;
            }
            k
        };
        let j = at + 1;
        let k = digits(j);
        if k == j {
            return Ok(None);
        }
        let lo = count(&s[j..k])?;
        let (hi, k) = if s.get(k) == Some(&b',') {
            let m = k + 1;
            let e = digits(m);
            (if e == m { None } else { Some(count(&s[m..e])?) }, e)
        } else {
            (Some(lo), k)
        };
        if s.get(k) != Some(&b'}') {
            return Err("unterminated interval {".into());
        }
        if let Some(h) = hi
            && h < lo
        {
            return Err("interval {n,m} with m < n".into());
        }
        Ok(Some((lo, hi, k + 1)))
    }

    /// A backslash escape at self.i; returns the byte and moves past it.
    fn escape(&mut self) -> u8 {
        let s = self.s;
        let i = self.i;
        let Some(&d) = s.get(i + 1) else {
            // A trailing backslash is a literal backslash.
            self.i += 1;
            return b'\\';
        };
        let simple = match d {
            b'n' => Some(b'\n'),
            b't' => Some(b'\t'),
            b'r' => Some(b'\r'),
            b'f' => Some(0x0c),
            b'v' => Some(0x0b),
            b'a' => Some(0x07),
            b'b' => Some(0x08),
            _ => None,
        };
        if let Some(b) = simple {
            self.i += 2;
            return b;
        }
        if (b'0'..=b'7').contains(&d) {
            // Up to three octal digits; the value wraps to a byte.
            let mut j = i + 1;
            let mut v: u32 = 0;
            while j < s.len() && j < i + 4 && (b'0'..=b'7').contains(&s[j]) {
                v = v * 8 + (s[j] - b'0') as u32;
                j += 1;
            }
            self.i = j;
            return (v & 0xff) as u8;
        }
        if d == b'x' {
            let mut j = i + 2;
            let mut v: u32 = 0;
            while j < s.len() && j < i + 4 && s[j].is_ascii_hexdigit() {
                v = v * 16 + (s[j] as char).to_digit(16).unwrap();
                j += 1;
            }
            if j > i + 2 {
                self.i = j;
                return v as u8;
            }
        }
        self.i += 2;
        d
    }

    /// A bracket expression; self.i is just after the `[`.
    fn bracket(&mut self) -> P<ByteSet> {
        let mut set = ByteSet::default();
        let negated = self.peek() == Some(b'^');
        if negated {
            self.i += 1;
        }
        let mut first = true;
        loop {
            let Some(c) = self.peek() else {
                return Err("unterminated [".into());
            };
            if c == b']' && !first {
                self.i += 1;
                break;
            }
            first = false;
            let lo = match self.element()? {
                Elem::Class(cs) => {
                    set.union(&cs);
                    continue;
                }
                Elem::Byte(b) => b,
            };
            // A range, unless the '-' is the last thing before ']'.
            if self.peek() == Some(b'-')
                && let Some(&next) = self.s.get(self.i + 1)
                && next != b']'
            {
                self.i += 1;
                let hi = match self.element()? {
                    Elem::Class(_) => return Err("character class as range end".into()),
                    Elem::Byte(b) => b,
                };
                if hi < lo {
                    return Err(format!(
                        "invalid range {}-{}",
                        String::from_utf8_lossy(&[lo]),
                        String::from_utf8_lossy(&[hi])
                    ));
                }
                set.insert_range(lo, hi);
            } else {
                set.insert(lo);
            }
        }
        if negated {
            set.negate();
        }
        Ok(set)
    }

    /// One element inside brackets: a byte, an escape, `[:class:]`,
    /// `[=x=]` or `[.x.]`.
    fn element(&mut self) -> P<Elem> {
        let s = self.s;
        let c = s[self.i];
        if c == b'['
            && let Some(&kind @ (b':' | b'.' | b'=')) = s.get(self.i + 1)
        {
            let start = self.i + 2;
            let close = s[start..]
                .windows(2)
                .position(|w| w == [kind, b']'])
                .ok_or_else(|| format!("unterminated [{}", kind as char))?;
            let name = &s[start..start + close];
            self.i = start + close + 2;
            if kind == b':' {
                return class(name).map(Elem::Class).ok_or_else(|| {
                    format!(
                        "unknown character class [:{}:]",
                        String::from_utf8_lossy(name)
                    )
                });
            }
            if name.len() != 1 {
                return Err(format!(
                    "invalid collating element [{k}{}{k}]",
                    String::from_utf8_lossy(name),
                    k = kind as char
                ));
            }
            return Ok(Elem::Byte(name[0]));
        }
        if c == b'\\' {
            return Ok(Elem::Byte(self.escape()));
        }
        self.i += 1;
        Ok(Elem::Byte(c))
    }
}

/// A repeat count: leading zeros are ignored; above DUP_MAX is an error.
fn count(digits: &[u8]) -> P<u32> {
    let mut v: u64 = 0;
    for &d in digits {
        v = (v * 10 + (d - b'0') as u64).min(u64::from(u32::MAX));
    }
    if v > u64::from(DUP_MAX) {
        return Err(format!("repetition count above {DUP_MAX}"));
    }
    Ok(v as u32)
}

/// Build a repeat, simplifying the cases that need no automaton.
fn repeat(node: Node, lo: u32, hi: Option<u32>) -> Node {
    match node {
        Node::Empty => Node::Empty,
        // A repeated anchor is the anchor itself, or nothing if optional.
        Node::Bol | Node::Eol => {
            if lo > 0 {
                node
            } else {
                Node::Empty
            }
        }
        _ => Node::Repeat(Box::new(node), lo, hi),
    }
}

/// Bytes for [`Node::Set`] tests and debugging.
#[cfg(test)]
fn set_of(bytes: &[u8]) -> Node {
    let mut s = ByteSet::default();
    for &b in bytes {
        s.insert(b);
    }
    Node::Set(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Node {
        parse(s.as_bytes()).unwrap()
    }
    fn err(s: &str) -> String {
        parse(s.as_bytes()).unwrap_err().to_string()
    }
    fn lit(s: &str) -> Node {
        let items: Vec<Node> = s.bytes().map(|b| set_of(&[b])).collect();
        if items.len() == 1 {
            items.into_iter().next().unwrap()
        } else {
            Node::Cat(items)
        }
    }
    fn rep(n: Node, lo: u32, hi: Option<u32>) -> Node {
        Node::Repeat(Box::new(n), lo, hi)
    }

    #[test]
    fn literals_and_operators() {
        assert_eq!(p("abc"), lit("abc"));
        assert_eq!(p("a*"), rep(lit("a"), 0, None));
        assert_eq!(p("a+?"), rep(rep(lit("a"), 1, None), 0, Some(1)));
        assert_eq!(p("a{2,3}"), rep(lit("a"), 2, Some(3)));
        assert_eq!(p("a{2,}"), rep(lit("a"), 2, None));
        assert_eq!(p("a{02}"), rep(lit("a"), 2, Some(2)));
        assert_eq!(p("a{x"), lit("a{x"));
        assert_eq!(p("{"), lit("{"));
        assert_eq!(p("a|b|c"), set_of(b"abc"));
        assert_eq!(p("ab|c"), Node::Alt(vec![lit("ab"), lit("c")]));
        assert_eq!(p("()"), Node::Empty);
        assert_eq!(p("a)"), lit("a)"));
        assert_eq!(p(""), Node::Empty);
        assert_eq!(p("^*|a"), Node::Alt(vec![Node::Empty, lit("a")]));
    }

    #[test]
    fn anchors() {
        assert_eq!(p("^a$"), Node::Cat(vec![Node::Bol, lit("a"), Node::Eol]));
        assert_eq!(p("^*a"), lit("a"));
        assert_eq!(p("^+a"), Node::Cat(vec![Node::Bol, lit("a")]));
        assert_eq!(p("a$?"), Node::Cat(vec![lit("a"), Node::Empty]));
    }

    #[test]
    fn escapes() {
        assert_eq!(p(r"\n"), set_of(b"\n"));
        assert_eq!(p(r"\101"), set_of(b"A"));
        assert_eq!(p(r"\x41g"), lit("Ag"));
        assert_eq!(p(r"\."), set_of(b"."));
        assert_eq!(p(r"\/"), set_of(b"/"));
        assert_eq!(p("a\\"), lit("a\\"));
    }

    #[test]
    fn brackets() {
        assert_eq!(p("[abc]"), set_of(b"abc"));
        assert_eq!(p("[]a]"), set_of(b"]a"));
        assert_eq!(p("[a-]"), set_of(b"a-"));
        assert_eq!(p("[-a]"), set_of(b"-a"));
        assert_eq!(p("[a-c]"), set_of(b"abc"));
        assert_eq!(p("[[:digit:]x]"), set_of(b"0123456789x"));
        assert_eq!(p("[[=a=][.b.]]"), set_of(b"ab"));
        assert_eq!(p(r"[\]]"), set_of(b"]"));
        let Node::Set(s) = p("[^a]") else { panic!() };
        assert!(!s.contains(b'a') && s.contains(b'\n') && s.len() == 255);
    }

    #[test]
    fn errors() {
        let e =
            |s: &str, why: &str| assert_eq!(err(s), format!("bad regular expression /{s}/: {why}"));
        e("*a", "* with nothing to repeat");
        e("(+a)", "+ with nothing to repeat");
        e("a|?b", "? with nothing to repeat");
        e("{2}", "{ with nothing to repeat");
        e("a{2", "unterminated interval {");
        e("a{2,1}", "interval {n,m} with m < n");
        e("a{32768}", "repetition count above 32767");
        e("a||b", "empty alternative");
        e("|a", "empty alternative");
        e("(a", "unmatched (");
        e("[a", "unterminated [");
        e("[[:foo:]]", "unknown character class [:foo:]");
        e("[z-a]", "invalid range z-a");
        e("[a-[:digit:]]", "character class as range end");
        e("[[.ab.]]", "invalid collating element [.ab.]");
        e("[[:alpha]", "unterminated [:");
    }
}
