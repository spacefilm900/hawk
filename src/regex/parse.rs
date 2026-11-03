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
//! In the C locale every byte is a character and a bracket expression
//! becomes a [`ByteSet`].
//!
//! UTF-8 mode (pyawk v1.1's rules): the pattern is read as characters, and
//! every set of characters (`.`, a bracket, a class, a literal) is a list of
//! code-point ranges, which [`utf8_node`] turns into an alternation of
//! UTF-8 byte sequences.  So the automata in `nfa.rs` and `dfa.rs` stay
//! byte-based, unchanged, and C mode does not pay for UTF-8 at all.
//!
//! * An invalid byte b in the pattern, or an octal/hex escape for a byte
//!   of 0x80 or more that does not join the escapes after it into a valid
//!   character, stands for the *escaped byte* U+DC00+b (see `charset`),
//!   which matches only that invalid byte in the text.  `\303\251` is `é`.
//! * `.` and `[^...]` match any character but never an invalid byte.
//! * `[:class:]` uses gawk's Unicode tables; `[=x=]` and `[.x.]` hold one
//!   character, which is only equivalent to itself.
//!
//! The escaped bytes are encoded as if they were ordinary code points
//! (U+DC80..U+DCFF, `ED B2 80`..`ED B3 BF`); a pattern that can match one
//! is *raw* ([`Parsed::raw`]) and is run on a copy of the text with each
//! invalid byte written that way (see `regex/mod.rs`).

use crate::charset;
use crate::error::Error;

/// Largest repeat count in an interval (POSIX's RE_DUP_MAX).
pub const DUP_MAX: u32 = 32767;

/// The deepest syntax tree allowed: nested groups plus repeat operators
/// stacked on one another (`a**`), counted along any one path.  The parser
/// and the code that walks the tree recurse once per level, so without a
/// limit `((((...))))` a million deep would overflow the stack.  mawk and
/// pyawk give up at 500 nested groups; gawk crashes at 10,000.
pub const MAX_DEPTH: u32 = 2000;

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

/// A parsed pattern.
pub struct Parsed {
    pub node: Node,
    /// UTF-8 mode: the pattern can match an invalid byte (an escaped byte
    /// U+DC80..U+DCFF), so the text must be transcoded before matching.
    pub raw: bool,
}

/// Parse `src` in the C locale; errors read `bad regular expression
/// /src/: why`.
#[cfg(test)]
pub fn parse(src: &[u8]) -> Result<Node, Error> {
    parse_mode(src, false).map(|p| p.node)
}

/// Parse `src`, as characters if `utf8`.
pub fn parse_mode(src: &[u8], utf8: bool) -> Result<Parsed, Error> {
    let mut p = Parser {
        s: src,
        i: 0,
        depth: 0,
        utf8,
        raw: false,
    };
    let r = p.alt();
    match r {
        Ok((node, _)) if p.i == src.len() => Ok(Parsed { node, raw: p.raw }),
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

/// The POSIX character class `[:name:]` in the C locale as byte ranges,
/// or None if unknown.
fn class(name: &[u8]) -> Option<&'static [(u32, u32)]> {
    Some(match name {
        b"alpha" => &[(0x61, 0x7A), (0x41, 0x5A)], // a-z A-Z
        b"digit" => &[(0x30, 0x39)],
        b"alnum" => &[(0x61, 0x7A), (0x41, 0x5A), (0x30, 0x39)],
        b"upper" => &[(0x41, 0x5A)],
        b"lower" => &[(0x61, 0x7A)],
        // space: " \t\n\r\f\v" (tab..carriage return is 9..13).
        b"space" => &[(0x20, 0x20), (9, 13)],
        b"blank" => &[(0x20, 0x20), (9, 9)],
        b"punct" => &[(0x21, 0x2F), (0x3A, 0x40), (0x5B, 0x60), (0x7B, 0x7E)],
        b"xdigit" => &[(0x30, 0x39), (0x61, 0x66), (0x41, 0x46)],
        b"cntrl" => &[(0, 31), (127, 127)],
        b"print" => &[(32, 126)],
        b"graph" => &[(33, 126)],
        _ => return None,
    })
}

/// The escaped bytes, U+DC80..U+DCFF: what invalid bytes are in a pattern.
const ESCAPED: (u32, u32) = (charset::ESCAPED_BASE + 0x80, charset::ESCAPED_BASE + 0xFF);

/// Sort and merge overlapping or touching ranges.
fn merge(mut r: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    r.sort_unstable();
    let mut out: Vec<(u32, u32)> = Vec::with_capacity(r.len());
    for (lo, hi) in r {
        match out.last_mut() {
            Some(last) if lo <= last.1.saturating_add(1) => last.1 = last.1.max(hi),
            _ => out.push((lo, hi)),
        }
    }
    out
}

/// The ranges of `r` (merged) minus the range `(lo, hi)`.
fn subtract(r: &[(u32, u32)], (lo, hi): (u32, u32)) -> Vec<(u32, u32)> {
    let mut out = Vec::with_capacity(r.len() + 1);
    for &(a, b) in r {
        if b < lo || a > hi {
            out.push((a, b));
            continue;
        }
        if a < lo {
            out.push((a, lo - 1));
        }
        if b > hi {
            out.push((hi + 1, b));
        }
    }
    out
}

/// The complement of merged ranges within 0..=0x10FFFF.
fn complement(r: &[(u32, u32)]) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let mut next = 0;
    for &(a, b) in r {
        if a > next {
            out.push((next, a - 1));
        }
        next = b + 1;
    }
    if next <= 0x10FFFF {
        out.push((next, 0x10FFFF));
    }
    out
}

/// The UTF-8 byte-range sequences for the code points lo..=hi (both
/// 0x80 or more): each sequence is a list of byte ranges, one per byte,
/// and the sequences together match exactly the encodings of lo..=hi.
/// This is the standard split (as in RE2 and Rust's regex crate): cut the
/// range where the encoded length changes, then where the bytes after
/// the first stop covering the full 80..BF, until each piece is a simple
/// product of byte ranges.  Surrogates get the plain UTF-8 bit layout;
/// the callers only let the escaped bytes through.
fn utf8_sequences(lo: u32, hi: u32, out: &mut Vec<Vec<(u8, u8)>>) {
    let mut todo = vec![(lo, hi)];
    // `'piece: loop` is a labelled loop; `continue 'piece` restarts it.
    while let Some((s, mut e)) = todo.pop() {
        'piece: loop {
            for max in [0x7F, 0x7FF, 0xFFFF] {
                if s <= max && max < e {
                    todo.push((max + 1, e));
                    e = max;
                    continue 'piece;
                }
            }
            for i in 1..4 {
                let m = (1u32 << (6 * i)) - 1;
                if s & !m != e & !m {
                    if s & m != 0 {
                        todo.push(((s | m) + 1, e));
                        e = s | m;
                        continue 'piece;
                    }
                    if e & m != m {
                        todo.push((e & !m, e));
                        e = (e & !m) - 1;
                        continue 'piece;
                    }
                }
            }
            let (mut a, mut b) = (Vec::new(), Vec::new());
            charset::utf8_bytes(s, &mut a);
            charset::utf8_bytes(e, &mut b);
            out.push(a.into_iter().zip(b).collect());
            break;
        }
    }
}

/// Byte ranges as a ByteSet.
fn byte_set(r: &[(u32, u32)]) -> ByteSet {
    let mut set = ByteSet::default();
    for &(lo, hi) in r {
        if lo <= 0xFF {
            set.insert_range(lo as u8, hi.min(0xFF) as u8);
        }
    }
    set
}

/// The node matching one character from the merged code-point ranges `r`
/// (UTF-8 mode).  ASCII is one byte set; the rest is a trie of byte-range
/// sequences, so sequences that share their first bytes share those
/// states: `[éè]` is `C3 (A8|A9)`, not `C3 A8 | C3 A9`.
pub fn utf8_node(r: &[(u32, u32)]) -> Node {
    let mut seqs: Vec<Vec<(u8, u8)>> = Vec::new();
    let ascii: Vec<(u32, u32)> = r
        .iter()
        .filter(|&&(lo, _)| lo < 0x80)
        .map(|&(lo, hi)| (lo, hi.min(0x7F)))
        .collect();
    for &(lo, hi) in r {
        if hi >= 0x80 {
            utf8_sequences(lo.max(0x80), hi, &mut seqs);
        }
    }
    let mut branches = Vec::new();
    if !ascii.is_empty() {
        branches.push(Node::Set(byte_set(&ascii)));
    }
    trie(&seqs, &mut branches);
    alt_of(branches)
}

/// Append the alternatives for `seqs` (sorted, all non-empty): one per
/// distinct first byte range, followed by the trie of what comes after.
fn trie(seqs: &[Vec<(u8, u8)>], out: &mut Vec<Node>) {
    let mut leaves = ByteSet::default();
    let mut any_leaf = false;
    let mut i = 0;
    while i < seqs.len() {
        let head = seqs[i][0];
        let mut j = i;
        while j < seqs.len() && seqs[j][0] == head {
            j += 1;
        }
        let set = Node::Set(byte_set(&[(head.0 as u32, head.1 as u32)]));
        let rest: Vec<Vec<(u8, u8)>> = seqs[i..j]
            .iter()
            .filter(|s| s.len() > 1)
            .map(|s| s[1..].to_vec())
            .collect();
        if rest.is_empty() {
            // Single-byte alternatives merge into one set.
            leaves.union(&byte_set(&[(head.0 as u32, head.1 as u32)]));
            any_leaf = true;
        } else {
            let mut tail = Vec::new();
            trie(&rest, &mut tail);
            let mut items = vec![set];
            match alt_of(tail) {
                Node::Cat(more) => items.extend(more),
                other => items.push(other),
            }
            out.push(Node::Cat(items));
        }
        i = j;
    }
    if any_leaf {
        out.push(Node::Set(leaves));
    }
}

fn alt_of(mut branches: Vec<Node>) -> Node {
    match branches.len() {
        0 => Node::Set(ByteSet::default()), // matches nothing
        1 => branches.pop().unwrap(),
        _ => Node::Alt(branches),
    }
}

/// How tall (see [`Tree`]) a node built by [`utf8_node`] is.
fn height(node: &Node) -> u32 {
    match node {
        Node::Cat(v) | Node::Alt(v) => 1 + v.iter().map(height).max().unwrap_or(0),
        Node::Repeat(x, ..) => 1 + height(x),
        _ => 1,
    }
}

/// What a bracket element turned out to be.
enum Elem {
    /// One character: a byte in C mode, a code point in UTF-8 mode.
    Char(u32),
    Class(Vec<(u32, u32)>),
}

/// Parser state.  Errors are plain `String` reasons here; [`parse`] wraps
/// them into the full message.
struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    depth: u32,
    /// Read the pattern as UTF-8 characters.
    utf8: bool,
    /// Some set can match an escaped byte (see [`Parsed::raw`]).
    raw: bool,
}

/// `Result<T, String>`: either a value or an error reason.
type P<T> = Result<T, String>;

/// A node with its height: the most nodes on a path from it down to a
/// leaf.  Each parsing function returns one, so the depth is known as the
/// tree is built, without walking it again.
type Tree = (Node, u32);

/// The height of a new node above `child_height`, or an error if that is
/// too deep (see `MAX_DEPTH`).
fn above(child_height: u32) -> P<u32> {
    if child_height >= MAX_DEPTH {
        return Err("too deeply nested".into());
    }
    Ok(child_height + 1)
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    /// alternation: cat ('|' cat)*
    fn alt(&mut self) -> P<Tree> {
        let mut branches = Vec::new();
        let mut height = 0;
        // A branch that consumed no text at all (`a||b`, `|a`, `(a|)`) is
        // an error, as in mawk and onetrue-awk; `^*|a` is fine.
        let mut empty_branch = false;
        loop {
            let start = self.i;
            let (branch, h) = self.cat()?;
            branches.push(branch);
            height = height.max(h);
            empty_branch |= self.i == start;
            if self.peek() != Some(b'|') {
                break;
            }
            self.i += 1;
        }
        if branches.len() == 1 {
            return Ok((branches.pop().unwrap(), height));
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
            return Ok((Node::Set(all), 1));
        }
        Ok((Node::Alt(branches), above(height)?))
    }

    /// concatenation of postfix-repeated atoms.
    fn cat(&mut self) -> P<Tree> {
        let mut items = Vec::new();
        let mut height = 0;
        while let Some(c) = self.peek() {
            if c == b'|' || (c == b')' && self.depth > 0) {
                break;
            }
            if let Some(atom) = self.atom()? {
                let (node, h) = self.postfix(atom)?;
                // A multibyte character is a Cat of bytes: splice it in, so
                // `é` in `aéb` is a plain literal (see `literal_of`).
                match node {
                    Node::Cat(more) => items.extend(more),
                    node => items.push(node),
                }
                height = height.max(h);
            }
        }
        Ok(match items.len() {
            0 => (Node::Empty, 1),
            1 => (items.pop().unwrap(), height),
            _ => (Node::Cat(items), above(height)?),
        })
    }

    /// One atom, or None when `^` swallowed a following `*` / `?`.
    fn atom(&mut self) -> P<Option<Tree>> {
        let c = self.s[self.i];
        let leaf = |node| Ok(Some((node, 1)));
        match c {
            b'(' => {
                self.i += 1;
                // A group adds no node of its own, but each level of
                // parsing recurses, so it counts toward the limit too.
                if self.depth >= MAX_DEPTH {
                    return Err("too deeply nested".into());
                }
                self.depth += 1;
                let node = if self.peek() == Some(b')') {
                    (Node::Empty, 1)
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
                let mut node = Some((Node::Bol, 1));
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
                leaf(Node::Eol)
            }
            b'.' if !self.utf8 => {
                self.i += 1;
                leaf(Node::Set(ByteSet::full()))
            }
            b'.' | b'[' | b'\\' => self.char_atom(c).map(Some),
            _ if self.utf8 && c >= 0x80 => self.char_atom(c).map(Some),
            _ => {
                self.i += 1;
                leaf(Node::Set(ByteSet::single(c)))
            }
        }
    }

    /// The atoms that are a set of characters: `.` (UTF-8 mode), a bracket,
    /// an escape or a multibyte character.  A separate function (never
    /// inlined) so that `atom`, which recurses for groups, keeps a small
    /// stack frame.
    #[inline(never)]
    fn char_atom(&mut self, c: u8) -> P<Tree> {
        match c {
            b'.' => {
                self.i += 1;
                // Any character, but no invalid byte.
                Ok(self.char_set(vec![ESCAPED], true))
            }
            b'[' => {
                self.i += 1;
                self.bracket()
            }
            b'\\' => {
                let c = self.escape();
                Ok(self.char_set(vec![(c, c)], false))
            }
            _ => {
                let (c, n) = charset::decode_at(self.s, self.i);
                self.i += n;
                Ok(self.char_set(vec![(c, c)], false))
            }
        }
    }

    /// Apply any postfix operators after an atom.
    fn postfix(&mut self, (mut node, mut height): Tree) -> P<Tree> {
        loop {
            let (lo, hi) = match self.peek() {
                Some(b'*') => {
                    self.i += 1;
                    (0, None)
                }
                Some(b'+') => {
                    self.i += 1;
                    (1, None)
                }
                Some(b'?') => {
                    self.i += 1;
                    (0, Some(1))
                }
                Some(b'{') => match self.interval_at(self.i)? {
                    Some((lo, hi, end)) => {
                        self.i = end;
                        (lo, hi)
                    }
                    None => return Ok((node, height)),
                },
                _ => return Ok((node, height)),
            };
            node = repeat(node, lo, hi);
            if matches!(node, Node::Repeat(..)) {
                height = above(height)?;
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

    /// A backslash escape at self.i; returns the character and moves past
    /// it.  In UTF-8 mode a byte escape of 0x80 or more is an escaped
    /// byte, unless it is a lead byte that the escapes after it complete
    /// into a valid character (`\303\251` is `é`, as in gawk).
    #[inline(never)]
    fn escape(&mut self) -> u32 {
        let s = self.s;
        let Some(&d) = s.get(self.i + 1) else {
            // A trailing backslash is a literal backslash.
            self.i += 1;
            return b'\\' as u32;
        };
        if let Some((b, end)) = byte_escape(s, self.i) {
            self.i = end;
            if !self.utf8 || b < 0x80 {
                return b as u32;
            }
            return self.utf8_sequence(b);
        }
        if self.utf8 && d >= 0x80 {
            // `\é` is `é`.
            let (c, n) = charset::decode_at(s, self.i + 1);
            self.i += 1 + n;
            return c;
        }
        self.i += 2;
        match d {
            b'n' => b'\n',
            b't' => b'\t',
            b'r' => b'\r',
            b'f' => 0x0c,
            b'v' => 0x0b,
            b'a' => 0x07,
            b'b' => 0x08,
            _ => d,
        }
        .into()
    }

    /// UTF-8 mode, after a byte escape for `lead` (0x80 or more): read
    /// the escapes for its continuation bytes and return the character
    /// they spell; if they do not spell one, the escaped byte `lead` alone
    /// (and nothing more is read).
    fn utf8_sequence(&mut self, lead: u8) -> u32 {
        let lone = charset::ESCAPED_BASE + lead as u32;
        if lead < 0xC0 {
            return lone;
        }
        let need = if lead < 0xE0 {
            1
        } else if lead < 0xF0 {
            2
        } else {
            3
        };
        let mut raw = vec![lead];
        let mut j = self.i;
        for _ in 0..need {
            match byte_escape(self.s, j) {
                Some((b, end)) if (0x80..=0xBF).contains(&b) => {
                    raw.push(b);
                    j = end;
                }
                _ => return lone,
            }
        }
        if charset::char_len(&raw, 0) != raw.len() {
            return lone; // e.g. an overlong form such as \300\200
        }
        self.i = j;
        charset::decode_at(&raw, 0).0
    }

    /// The node for one character from the set `ranges` (or, `negated`,
    /// from outside it).  C mode: a byte set.  UTF-8 mode: see
    /// [`utf8_node`]; surrogates other than the escaped bytes are never
    /// characters, and a negated set never matches an escaped byte.
    #[inline(never)] // keep `atom`'s stack frame small: it recurses
    fn char_set(&mut self, ranges: Vec<(u32, u32)>, negated: bool) -> Tree {
        if !self.utf8 {
            let mut set = byte_set(&ranges);
            if negated {
                set.negate();
            }
            return (Node::Set(set), 1);
        }
        let mut r = merge(ranges);
        if negated {
            r.push(ESCAPED);
            r = complement(&merge(r));
        }
        r = subtract(&r, (0xD800, ESCAPED.0 - 1));
        r = subtract(&r, (ESCAPED.1 + 1, 0xDFFF));
        if r.iter().any(|&(lo, hi)| lo <= ESCAPED.1 && hi >= ESCAPED.0) {
            self.raw = true;
        }
        if r.len() == 1 && r[0].1 < 0x80 {
            return (Node::Set(byte_set(&r)), 1);
        }
        let node = utf8_node(&r);
        let h = height(&node);
        (node, h)
    }
    /// A bracket expression; self.i is just after the `[`.
    #[inline(never)] // keep `atom`'s stack frame small: it recurses
    fn bracket(&mut self) -> P<Tree> {
        let mut set: Vec<(u32, u32)> = Vec::new();
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
                    set.extend(cs);
                    continue;
                }
                Elem::Char(b) => b,
            };
            // A range, unless the '-' is the last thing before ']'.
            if self.peek() == Some(b'-')
                && let Some(&next) = self.s.get(self.i + 1)
                && next != b']'
            {
                self.i += 1;
                let hi = match self.element()? {
                    Elem::Class(_) => return Err("character class as range end".into()),
                    Elem::Char(b) => b,
                };
                if hi < lo {
                    return Err(format!("invalid range {}-{}", self.show(lo), self.show(hi)));
                }
                set.push((lo, hi));
            } else {
                set.push((lo, lo));
            }
        }
        Ok(self.char_set(set, negated))
    }

    /// A character for an error message.
    fn show(&self, c: u32) -> String {
        let mut b = Vec::new();
        if self.utf8 {
            charset::encode(c, &mut b);
        } else {
            b.push(c as u8);
        }
        String::from_utf8_lossy(&b).into_owned()
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
                let ranges = if self.utf8 {
                    charset::class_ranges(name)
                } else {
                    class(name)
                };
                return ranges.map(|r| Elem::Class(r.to_vec())).ok_or_else(|| {
                    format!(
                        "unknown character class [:{}:]",
                        String::from_utf8_lossy(name)
                    )
                });
            }
            // [.x.] (collating symbol) and [=x=] (equivalence class): one
            // character, equivalent only to itself.
            let one = if self.utf8 {
                !name.is_empty() && charset::char_len(name, 0) == name.len()
            } else {
                name.len() == 1
            };
            if !one {
                return Err(format!(
                    "invalid collating element [{k}{}{k}]",
                    String::from_utf8_lossy(name),
                    k = kind as char
                ));
            }
            return Ok(Elem::Char(if self.utf8 {
                charset::decode_at(name, 0).0
            } else {
                name[0] as u32
            }));
        }
        if c == b'\\' {
            return Ok(Elem::Char(self.escape()));
        }
        if self.utf8 && c >= 0x80 {
            let (c, n) = charset::decode_at(s, self.i);
            self.i += n;
            return Ok(Elem::Char(c));
        }
        self.i += 1;
        Ok(Elem::Char(c as u32))
    }
}

/// `(value, end)` of an octal `\ddd` or hex `\xhh` escape at `i` (a
/// backslash), else None.  Octal values wrap to a byte.
fn byte_escape(s: &[u8], i: usize) -> Option<(u8, usize)> {
    if s.get(i) != Some(&b'\\') {
        return None;
    }
    let d = *s.get(i + 1)?;
    if (b'0'..=b'7').contains(&d) {
        let mut j = i + 1;
        let mut v: u32 = 0;
        while j < s.len() && j < i + 4 && (b'0'..=b'7').contains(&s[j]) {
            v = v * 8 + (s[j] - b'0') as u32;
            j += 1;
        }
        return Some(((v & 0xff) as u8, j));
    }
    if d == b'x' {
        let mut j = i + 2;
        let mut v: u32 = 0;
        while j < s.len() && j < i + 4 && s[j].is_ascii_hexdigit() {
            v = v * 16 + (s[j] as char).to_digit(16).unwrap();
            j += 1;
        }
        if j > i + 2 {
            return Some((v as u8, j));
        }
    }
    None
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
    fn deep_nesting_is_an_error_not_a_crash() {
        // Compile on an 8 MiB stack (a debug build, so frames are big): the
        // limit must be reached long before the stack runs out.
        let compile = |src: String| {
            std::thread::Builder::new()
                .stack_size(8 << 20)
                .spawn(move || {
                    let re = crate::regex::Regex::new(src.as_bytes());
                    re.map(|re| re.is_match(b"a")).map_err(|e| e.to_string())
                })
                .unwrap()
                .join()
                .unwrap()
        };
        let n = 1_000_000;
        for src in [
            format!("{}a{}", "(".repeat(n), ")".repeat(n)),
            format!("a{}", "*".repeat(n)),
            format!("a{}", "?".repeat(n)),
            "(a*".repeat(n / 2) + &")*".repeat(n / 2),
        ] {
            let err = compile(src).unwrap_err();
            assert!(
                err.ends_with("/: too deeply nested"),
                "{}",
                &err[err.len() - 40..]
            );
        }
        // Depths up to the limit work: more than mawk and pyawk accept.
        let n = MAX_DEPTH as usize - 1;
        assert_eq!(
            compile(format!("{}a{}", "(".repeat(n), ")".repeat(n))),
            Ok(true)
        );
        assert_eq!(compile(format!("a{}", "*".repeat(n))), Ok(true));
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

    fn u(s: &[u8]) -> Parsed {
        parse_mode(s, true).unwrap()
    }
    fn u_err(s: &str) -> String {
        parse_mode(s.as_bytes(), true).err().unwrap().to_string()
    }

    /// Does `seq` (byte ranges) match exactly the bytes `b`?
    fn seq_matches(seq: &[(u8, u8)], b: &[u8]) -> bool {
        seq.len() == b.len() && seq.iter().zip(b).all(|(&(lo, hi), &x)| lo <= x && x <= hi)
    }

    #[test]
    fn utf8_sequences_cover_exactly_the_range() {
        for (lo, hi) in [
            (0x80, 0x10FFFF),
            (0xE9, 0xE9),
            (0x7FF, 0x800),
            (0xD7FF, 0xE000),
            (0x123, 0x4567),
            (0xFFFE, 0x10001),
            (0x10000, 0x10FFFF),
        ] {
            let mut seqs = Vec::new();
            utf8_sequences(lo, hi, &mut seqs);
            let step = if hi - lo > 5000 { 97 } else { 1 };
            let probes =
                (0x80..=0x10FFFFu32)
                    .step_by(step)
                    .chain([lo, hi, lo.saturating_sub(1), hi + 1]);
            for c in probes {
                let mut b = Vec::new();
                charset::utf8_bytes(c, &mut b);
                let n = seqs.iter().filter(|s| seq_matches(s, &b)).count();
                let want = (lo <= c && c <= hi) as usize;
                assert_eq!(n, want, "U+{c:X} in {lo:X}-{hi:X}");
            }
        }
    }

    #[test]
    fn utf8_mode_reads_characters() {
        // A multibyte character is its bytes, spliced into the literal.
        assert_eq!(u("aé".as_bytes()).node, lit("aé"));
        // A repeat applies to the whole character.
        assert_eq!(u("é+".as_bytes()).node, rep(lit("é"), 1, None));
        // Escapes that spell a character join into it (gawk).
        assert_eq!(u(br"\303\251").node, lit("é"));
        assert_eq!(u(br"\xc3\xa9").node, lit("é"));
        // `\é` is `é`.
        assert_eq!(u("\\é".as_bytes()).node, lit("é"));
        // [=x=] and [.x.] hold one character.
        assert_eq!(u("[[=é=]]".as_bytes()).node, lit("é"));
        assert_eq!(u("[[.é.]]".as_bytes()).node, lit("é"));
        assert_eq!(
            u_err("[[=ab=]]"),
            "bad regular expression /[[=ab=]]/: invalid collating element [=ab=]"
        );
        // Ranges and their errors are by character.
        assert_eq!(
            u_err("[é-a]"),
            "bad regular expression /[é-a]/: invalid range é-a"
        );
        assert_eq!(
            u("[à-é]".as_bytes()).node,
            Node::Cat(vec![set_of(&[0xC3]), Node::Set(byte_set(&[(0xA0, 0xA9)]))])
        );
        // An ASCII-only set stays one byte set (the C-mode fast path).
        assert_eq!(u(b"[a-c]").node, p("[a-c]"));
    }

    #[test]
    fn utf8_mode_invalid_bytes() {
        // An invalid byte, or a lone high escape, is the escaped byte: it
        // makes the pattern raw (see `regex/mod.rs`).
        for raw in [
            &br"\xe9"[..],
            br"\351",
            br"\303",
            br"\303x",
            br"\300\200",
            br"[\200-\377]",
            b"\xe9",
        ] {
            let parsed = u(raw);
            assert!(parsed.raw, "{}", String::from_utf8_lossy(raw));
        }
        let raw = u(br"\xe9");
        assert_eq!(
            raw.node,
            Node::Cat(vec![set_of(&[0xED]), set_of(&[0xB3]), set_of(&[0xA9])])
        );
        // `.`, `[^a]`, classes and characters never match one.
        for src in ["a.b", "[^a]", "[[:alpha:]]", "é", "[^[:space:]]+"] {
            assert!(!u(src.as_bytes()).raw, "{src}");
        }
    }

    #[test]
    fn utf8_sets_stay_small() {
        // The trie keeps even the biggest classes near a thousand states.
        for (src, max) in [
            (".", 40),
            ("[^a]", 40),
            ("[[:alpha:]]", 1500),
            ("[[:punct:]]", 1200),
            ("[[:upper:]]", 400),
            ("[[:space:]]", 30),
        ] {
            let n = crate::regex::nfa::size(&u(src.as_bytes()).node);
            assert!(n <= max, "{src}: {n}");
        }
    }
}
