//! Thompson NFA built from the syntax tree.
//!
//! The automaton is a vector of instructions; each names the instruction
//! (by index) that comes after it.  It is built *back to front*: to build a
//! node we first know where to go after it (`next`), which makes every
//! construction a single pass with no patching except the `*` loop.

use super::parse::{ByteSet, Node};

/// The most NFA states a regex may need (pyawk's NFA_LIMIT).  The size is
/// worked out from the tree *before* building, so `(a{1000}){1000}` is
/// refused cheaply instead of eating memory.
pub const NFA_LIMIT: u64 = 1_000_000;

/// One NFA instruction.  `u32` indexes keep it small.
#[derive(Clone, Debug)]
pub enum Inst {
    /// Consume one byte from the set, then go to `next`.
    Byte(ByteSet, u32),
    /// Go to both (an epsilon fork).
    Split(u32, u32),
    /// `^`: continue only at the start of the text.
    Bol(u32),
    /// `$`: continue only at the end of the text.
    Eol(u32),
    /// The whole pattern has matched.
    Match,
}

pub struct Nfa {
    pub insts: Vec<Inst>,
    pub start: u32,
}

/// How many instructions `node` needs (not counting the final Match).
/// Saturating arithmetic keeps huge nested counts from overflowing.
pub fn size(node: &Node) -> u64 {
    match node {
        Node::Set(_) | Node::Bol | Node::Eol => 1,
        Node::Empty => 0,
        Node::Cat(items) => items.iter().map(size).fold(0, u64::saturating_add),
        Node::Alt(bs) => bs
            .iter()
            .map(size)
            .fold(bs.len() as u64 - 1, u64::saturating_add),
        Node::Repeat(x, lo, hi) => {
            let x = size(x);
            let opt = match hi {
                None => 1u64.saturating_add(x),
                Some(h) => u64::from(h - lo).saturating_mul(1u64.saturating_add(x)),
            };
            opt.saturating_add(u64::from(*lo).saturating_mul(x))
        }
    }
}

impl Nfa {
    /// Build the NFA of `node`; the caller has checked [`size`].
    /// With `reverse`, build the automaton of the *reversed* language (used
    /// to find where matches start by scanning backwards).
    pub fn new(node: &Node, reverse: bool) -> Nfa {
        let mut b = Builder {
            insts: vec![Inst::Match],
            reverse,
        };
        let start = b.build(node, 0);
        Nfa {
            insts: b.insts,
            start,
        }
    }
}

struct Builder {
    insts: Vec<Inst>,
    reverse: bool,
}

impl Builder {
    fn push(&mut self, i: Inst) -> u32 {
        self.insts.push(i);
        (self.insts.len() - 1) as u32
    }

    /// Instructions for `node` followed by `next`; returns the entry.
    fn build(&mut self, node: &Node, next: u32) -> u32 {
        match node {
            Node::Set(s) => self.push(Inst::Byte(*s, next)),
            Node::Empty => next,
            // Reversed, the start of the text becomes its end.
            Node::Bol if self.reverse => self.push(Inst::Eol(next)),
            Node::Eol if self.reverse => self.push(Inst::Bol(next)),
            Node::Bol => self.push(Inst::Bol(next)),
            Node::Eol => self.push(Inst::Eol(next)),
            Node::Cat(items) => {
                let mut cur = next;
                if self.reverse {
                    for it in items {
                        cur = self.build(it, cur);
                    }
                } else {
                    // Back to front: the last item goes to `next`.
                    for it in items.iter().rev() {
                        cur = self.build(it, cur);
                    }
                }
                cur
            }
            Node::Alt(branches) => {
                let entries: Vec<u32> = branches.iter().map(|b| self.build(b, next)).collect();
                let mut cur = *entries.last().unwrap();
                for &e in entries[..entries.len() - 1].iter().rev() {
                    cur = self.push(Inst::Split(e, cur));
                }
                cur
            }
            Node::Repeat(x, lo, hi) => {
                // The optional part first (it comes last in the text).
                let mut cur = match hi {
                    None => {
                        // x*: a fork that either runs x and comes back, or
                        // leaves.  The fork's first arm is patched in once
                        // x is built.
                        let fork = self.push(Inst::Split(next, next));
                        let body = self.build(x, fork);
                        self.insts[fork as usize] = Inst::Split(body, next);
                        fork
                    }
                    Some(h) => {
                        // x{0,k} is (x(x(...)?)?)?: k nested optionals.
                        let mut cur = next;
                        for _ in 0..(h - lo) {
                            let body = self.build(x, cur);
                            cur = self.push(Inst::Split(body, next));
                        }
                        cur
                    }
                };
                // Then `lo` mandatory copies in front.
                for _ in 0..*lo {
                    cur = self.build(x, cur);
                }
                cur
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::parse::parse;
    use super::*;

    #[test]
    fn sizes_follow_pyawk() {
        let sz = |s: &str| size(&parse(s.as_bytes()).unwrap());
        assert_eq!(sz("abc"), 3);
        assert_eq!(sz("a|bc"), 4);
        assert_eq!(sz("a*"), 2);
        assert_eq!(sz("a{2,4}"), 6);
        assert_eq!(sz("a{3}"), 3);
        assert_eq!(sz("(a{1000}){1000}"), 1_000_000);
        // The built automaton has exactly that many (plus Match).
        for s in ["abc", "a|bc", "a*", "a{2,4}", "(ab|c)+d?"] {
            let n = parse(s.as_bytes()).unwrap();
            assert_eq!(Nfa::new(&n, false).insts.len() as u64, size(&n) + 1, "{s}");
        }
    }
}
