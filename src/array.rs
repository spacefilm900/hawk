//! AWK arrays: maps from string keys to values that remember the order in
//! which keys were added.
//!
//! `for (k in a)` visits keys in an order POSIX leaves unspecified.  pyawk
//! uses insertion order (Python dictionaries keep it), and so does hawk, so
//! that the two print the same thing.  Rust's `HashMap` has no stable order,
//! so the array keeps its entries in a `Vec` (the order) plus a `HashMap`
//! from key to position in that `Vec` (the fast lookup).  Deleting leaves a
//! hole (`None`) that is cleaned up when holes pile up.
//!
//! Array keys come from input data, so each array's lookup map is seeded
//! randomly.  Input crafted so its keys collide (making every lookup slow)
//! cannot be prepared in advance: the seed is not known until the array
//! exists.  The hash itself is FxHash, which is much faster than the
//! standard library's SipHash and, with a secret seed, still stops that
//! precomputation.  It is not a cryptographic hash: someone who can try
//! keys against a running program and time the answers could still hunt
//! for a collision.  Variable names (chosen by the program text) and the
//! small regex cache use FxHash with a fixed seed; a collision there is
//! not interesting.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::rc::Rc;

use crate::value::Value;

/// Array keys are strings (numbers are converted with CONVFMT first).
pub type Key = Rc<[u8]>;

/// A small, fast hash function (the "FxHash" used inside the Rust
/// compiler).  The standard library's default hasher resists deliberate
/// collision attacks at a cost in speed; that resistance is not needed for
/// keys the program text chooses, such as variable names.  Array keys use
/// the same function with a random seed (see the top of this file).
/// Writing our own keeps the zero-dependency rule.
#[derive(Default, Clone, Copy)]
pub struct FxHasher(u64);

impl Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        const K: u64 = 0x51_7c_c1_b7_27_22_0a_95;
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.0 = (self.0.rotate_left(5) ^ u64::from_le_bytes(word)).wrapping_mul(K);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

/// Mix the high bits of an FxHash down into the low bits.
///
/// `HashMap` picks its bucket from the low bits.  FxHash leaves those
/// bits unchanged for keys that differ only in the middle: the crafted
/// keys in `colliding_keys_stay_fast` all ended in the same 24 bits, so
/// every key landed in one bucket and inserting became quadratic.  Array
/// keys go through this; variable names do not, because there are few of
/// them and the extra multiplies would be paid on every lookup.
fn avalanche(mut h: u64) -> u64 {
    // The finalizer from MurmurHash3.
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^= h >> 33;
    h
}

/// FxHash with the avalanche, for array keys.
struct SeededFx(FxHasher);

impl Hasher for SeededFx {
    fn write(&mut self, bytes: &[u8]) {
        self.0.write(bytes);
    }
    fn finish(&self) -> u64 {
        avalanche(self.0.finish())
    }
}

/// A `HashMap` using `FxHasher` with a fixed seed (see the top of this file).
pub type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

/// FxHash plus a per-array random seed.  `BuildHasher` is what `HashMap`
/// asks for when it wants a fresh hasher; the seed is copied into it.
#[derive(Clone)]
struct FxBuildHasher {
    seed: u64,
}

impl std::hash::BuildHasher for FxBuildHasher {
    type Hasher = SeededFx;
    fn build_hasher(&self) -> SeededFx {
        SeededFx(FxHasher(self.seed))
    }
}

impl std::fmt::Debug for FxBuildHasher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FxBuildHasher")
    }
}

/// A seed the program text cannot predict.  The standard library's
/// `RandomState` draws one from the operating system; hashing nothing
/// still folds that secret in, so `finish` is a random `u64`.
fn random_seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

#[derive(Debug)]
pub struct Array {
    index: HashMap<Key, usize, FxBuildHasher>,
    entries: Vec<Option<(Key, Value)>>,
}

impl Default for Array {
    fn default() -> Array {
        Array {
            index: HashMap::with_hasher(FxBuildHasher {
                seed: random_seed(),
            }),
            entries: Vec::new(),
        }
    }
}

impl std::fmt::Debug for FxHasher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FxHasher")
    }
}

impl Array {
    pub fn new() -> Array {
        Array::default()
    }

    /// Number of elements.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    pub fn contains(&self, key: &[u8]) -> bool {
        self.index.contains_key(key)
    }

    /// The element, if it exists (reading `a[k]` in AWK creates it; use
    /// `get_or_create` for that).
    pub fn get(&self, key: &[u8]) -> Option<&Value> {
        let &i = self.index.get(key)?;
        self.entries[i].as_ref().map(|(_, v)| v)
    }

    /// The element, created as uninitialized if missing.
    ///
    /// `key` is borrowed.  A key that is already present costs no
    /// allocation; only a new key is copied into an `Rc`.
    pub fn get_or_create(&mut self, key: &[u8]) -> &mut Value {
        let i = match self.index.get(key) {
            Some(&i) => i,
            None => {
                let key: Key = Rc::from(key);
                self.entries.push(Some((key.clone(), Value::Uninit)));
                let i = self.entries.len() - 1;
                self.index.insert(key, i);
                i
            }
        };
        // `as_mut().unwrap()`: live index entries always point at `Some`.
        &mut self.entries[i].as_mut().unwrap().1
    }

    /// `a[k] += delta`, creating a missing element as 0 first.  Returns
    /// the number that was there before the addition, which is what
    /// `a[k]++` yields.  The stored result is a plain number.
    pub fn add_number(&mut self, key: &[u8], delta: f64) -> f64 {
        if let Some(&i) = self.index.get(key) {
            let v = &mut self.entries[i].as_mut().unwrap().1;
            let old = v.to_num();
            *v = Value::Num(old + delta);
            return old;
        }
        let key: Key = Rc::from(key);
        self.entries.push(Some((key.clone(), Value::Num(delta))));
        let i = self.entries.len() - 1;
        self.index.insert(key, i);
        0.0
    }

    pub fn set(&mut self, key: &Key, v: Value) {
        *self.get_or_create(key) = v;
    }

    pub fn remove(&mut self, key: &[u8]) {
        if let Some(i) = self.index.remove(key) {
            self.entries[i] = None;
            // Compact when more than half the slots are holes.
            if self.entries.len() > 32 && self.index.len() * 2 < self.entries.len() {
                self.entries.retain(|e| e.is_some());
                for (i, e) in self.entries.iter().enumerate() {
                    if let Some((k, _)) = e {
                        self.index.insert(k.clone(), i);
                    }
                }
            }
        }
    }

    pub fn clear(&mut self) {
        self.index.clear();
        self.entries.clear();
    }

    /// The keys in insertion order (a copy: the loop body may change the
    /// array while `for (k in a)` walks this list).
    pub fn keys(&self) -> Vec<Key> {
        self.entries
            .iter()
            .filter_map(|e| e.as_ref().map(|(k, _)| k.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &str) -> Key {
        Rc::from(s.as_bytes())
    }

    #[test]
    fn keeps_insertion_order_through_deletes() {
        let mut a = Array::new();
        for i in 0..100 {
            a.set(&k(&i.to_string()), Value::Num(i as f64));
        }
        for i in (0..100).filter(|i| i % 3 != 0) {
            a.remove(i.to_string().as_bytes());
        }
        let keys: Vec<String> = a
            .keys()
            .iter()
            .map(|k| String::from_utf8(k.to_vec()).unwrap())
            .collect();
        let want: Vec<String> = (0..100)
            .filter(|i| i % 3 == 0)
            .map(|i| i.to_string())
            .collect();
        assert_eq!(keys, want);
        assert_eq!(a.len(), 34);
        assert!(a.contains(b"99"));
        assert!(!a.contains(b"98"));
        assert_eq!(a.get(b"99").unwrap().to_num(), 99.0);
        let mut b = Array::new();
        assert_eq!(b.add_number(b"b", 1.0), 0.0);
        assert_eq!(b.add_number(b"a", 1.0), 0.0);
        assert_eq!(b.add_number(b"b", 1.0), 1.0);
        let got: Vec<Vec<u8>> = b.keys().iter().map(|k| k.to_vec()).collect();
        assert_eq!(got, [b"b".to_vec(), b"a".to_vec()]);
        assert_eq!(b.get(b"b").unwrap().to_num(), 2.0);
        a.set(&k("1"), Value::Num(1.0));
        assert_eq!(a.keys().last().unwrap().as_ref(), b"1");
    }

    #[test]
    fn colliding_keys_stay_fast() {
        // Input data picks the keys, so a weak hash lets crafted input make
        // every insert walk one long collision chain (quadratic time).
        // These keys share their first three bytes, which made FxHash put
        // them all in the same buckets.
        let mut a = Array::new();
        let start = std::time::Instant::now();
        for i in 0..200_000u32 {
            let [b0, b1, b2, _] = i.to_le_bytes();
            let key: Key = Rc::from(&[b'a', b'b', b'c', b0, b1, b2, b'z', b'z'][..]);
            a.set(&key, Value::Num(1.0));
        }
        assert_eq!(a.len(), 200_000);
        let took = start.elapsed();
        assert!(took.as_secs() < 10, "{took:?}");
    }
}
