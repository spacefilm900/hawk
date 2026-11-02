//! Lazy DFA: the subset construction done on demand, one state at a time,
//! while matching.  Each DFA state stands for a set of NFA positions; its
//! transitions are filled in the first time a byte class is seen there.
//! Matching therefore reads every text byte once, with no backtracking.
//!
//! Byte classes: bytes that every set in the pattern treats alike are one
//! class (for `[a-z]+` there are three: below `a`, `a`..`z`, above `z`), so a
//! state's transition row is short.
//!
//! Like pyawk, the number of DFA states is capped ([`DFA_LIMIT`]); when the
//! cap is hit the cache is thrown away and rebuilt as needed, which keeps
//! memory bounded while each step stays linear in the NFA size.

use std::collections::HashMap;

use super::nfa::{Inst, Nfa};

/// Most DFA states kept at once.
pub const DFA_LIMIT: usize = 10_000;

/// A state id.  `UNKNOWN` marks a transition not computed yet.
pub type StateId = u32;
const UNKNOWN: StateId = u32::MAX;

/// State flag bits.
pub const ACCEPT: u8 = 1;
/// Accepts if the text ends here (a `$` may still be pending).
pub const ACCEPT_END: u8 = 2;
/// No match can come from this state any more.
pub const DEAD: u8 = 4;

/// What identifies a DFA state: the NFA byte-instructions it is at, and
/// whether it accepts now / at the end of the text.  Two kernels with the
/// same closure behave identically, so keying on the closure merges them.
type Key = (Box<[u32]>, u8);

pub struct Dfa {
    nfa: Nfa,
    /// Restart the pattern at every position (for "is there a match
    /// anywhere" scans) instead of only at the first.
    unanchored: bool,
    /// Byte -> class number, and how many classes there are.
    pub classes: [u8; 256],
    pub nclasses: usize,
    /// One representative byte per class.
    reps: Vec<u8>,
    /// Per state: its NFA byte-instructions and flags.
    chars: Vec<Box<[u32]>>,
    pub flags: Vec<u8>,
    /// Transitions: state * nclasses + class -> state (or UNKNOWN).
    pub trans: Vec<StateId>,
    map: HashMap<Key, StateId>,
    /// The two initial states (not at / at the start of the text).
    initial: [Option<StateId>; 2],
    /// Scratch for closures: `mark[i] == gen` means "seen this round".
    mark: Vec<u32>,
    generation: u32,
    stack: Vec<u32>,
    /// Bumped every time the cache is flushed (old ids become invalid).
    pub flushes: u64,
}

impl Dfa {
    pub fn new(nfa: Nfa, unanchored: bool) -> Dfa {
        // Class boundaries: a new class starts at byte b when some set
        // contains exactly one of b-1 and b.
        let sets: Vec<_> = nfa
            .insts
            .iter()
            .filter_map(|i| match i {
                Inst::Byte(s, _) => Some(*s),
                _ => None,
            })
            .collect();
        let mut classes = [0u8; 256];
        let mut reps = vec![0u8];
        let mut cls = 0usize;
        for b in 1..=255u8 {
            if sets.iter().any(|s| s.contains(b - 1) != s.contains(b)) {
                cls += 1;
                reps.push(b);
            }
            classes[b as usize] = cls as u8;
        }
        let n = nfa.insts.len();
        Dfa {
            nfa,
            unanchored,
            classes,
            nclasses: cls + 1,
            reps,
            chars: Vec::new(),
            flags: Vec::new(),
            trans: Vec::new(),
            map: HashMap::new(),
            initial: [None, None],
            mark: vec![0; n],
            generation: 0,
            stack: Vec::new(),
            flushes: 0,
        }
    }

    /// The state to begin in; `at_start` when position 0 of the text.
    pub fn initial(&mut self, at_start: bool) -> StateId {
        if let Some(id) = self.initial[at_start as usize] {
            return id;
        }
        let id = self.state_for(&[self.nfa.start], at_start);
        self.initial[at_start as usize] = Some(id);
        id
    }

    /// The next state after reading byte `b` in state `d`.
    #[inline]
    pub fn next(&mut self, d: StateId, b: u8) -> StateId {
        let cls = self.classes[b as usize] as usize;
        let t = self.trans[d as usize * self.nclasses + cls];
        if t != UNKNOWN {
            return t;
        }
        self.compute(d, cls)
    }

    /// Fill in a missing transition.
    #[cold]
    fn compute(&mut self, d: StateId, cls: usize) -> StateId {
        let b = self.reps[cls];
        let mut kernel: Vec<u32> = Vec::new();
        for &c in self.chars[d as usize].iter() {
            if let Inst::Byte(s, nx) = &self.nfa.insts[c as usize]
                && s.contains(b)
            {
                kernel.push(*nx);
            }
        }
        if self.unanchored {
            kernel.push(self.nfa.start);
        }
        let flushes = self.flushes;
        let id = self.state_for(&kernel, false);
        if self.flushes == flushes {
            self.trans[d as usize * self.nclasses + cls] = id;
        }
        id
    }

    /// Mark the closure of `kernel` (following forks and the anchors that
    /// hold) and return (byte-instructions, reached Match).
    fn closure(
        &mut self,
        kernel: &[u32],
        at_start: bool,
        at_end: bool,
        out: &mut Vec<u32>,
    ) -> bool {
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.mark.fill(0);
            self.generation = 1;
        }
        let g = self.generation;
        let mut accept = false;
        self.stack.clear();
        self.stack.extend(kernel.iter().rev());
        while let Some(s) = self.stack.pop() {
            if self.mark[s as usize] == g {
                continue;
            }
            self.mark[s as usize] = g;
            match self.nfa.insts[s as usize] {
                Inst::Byte(..) => out.push(s),
                Inst::Match => accept = true,
                Inst::Split(a, b) => {
                    self.stack.push(b);
                    self.stack.push(a);
                }
                Inst::Bol(n) => {
                    if at_start {
                        self.stack.push(n)
                    }
                }
                Inst::Eol(n) => {
                    if at_end {
                        self.stack.push(n)
                    }
                }
            }
        }
        accept
    }

    /// The state for a kernel, creating it if new.
    fn state_for(&mut self, kernel: &[u32], at_start: bool) -> StateId {
        let mut chars = Vec::new();
        let accept = self.closure(kernel, at_start, false, &mut chars);
        let mut scratch = Vec::new();
        let accept_end = accept || self.closure(kernel, at_start, true, &mut scratch);
        chars.sort_unstable();
        let mut flags = 0;
        if accept {
            flags |= ACCEPT;
        }
        if accept_end {
            flags |= ACCEPT_END;
        }
        if chars.is_empty() && !accept_end {
            flags |= DEAD;
        }
        let key: Key = (chars.into_boxed_slice(), flags);
        if let Some(&id) = self.map.get(&key) {
            return id;
        }
        if self.chars.len() >= DFA_LIMIT {
            self.flush();
        }
        let id = self.chars.len() as StateId;
        self.chars.push(key.0.clone());
        self.flags.push(flags);
        self.trans
            .extend(std::iter::repeat_n(UNKNOWN, self.nclasses));
        // A dead state goes nowhere: point all its transitions at itself.
        if flags & DEAD != 0 {
            let row = id as usize * self.nclasses;
            self.trans[row..row + self.nclasses].fill(id);
        }
        self.map.insert(key, id);
        id
    }

    fn flush(&mut self) {
        self.chars.clear();
        self.flags.clear();
        self.trans.clear();
        self.map.clear();
        self.initial = [None, None];
        self.flushes += 1;
    }

    /// The bytes that can begin a match from the state `d`, if `d` cannot
    /// accept without reading something.
    pub fn first_bytes(&self, d: StateId) -> Option<[bool; 256]> {
        if self.flags[d as usize] & (ACCEPT | ACCEPT_END) != 0 {
            return None;
        }
        let mut t = [false; 256];
        for &c in self.chars[d as usize].iter() {
            if let Inst::Byte(s, _) = &self.nfa.insts[c as usize] {
                for b in 0..=255u8 {
                    if s.contains(b) {
                        t[b as usize] = true;
                    }
                }
            }
        }
        Some(t)
    }

    #[cfg(test)]
    pub fn num_states(&self) -> usize {
        self.chars.len()
    }
}
