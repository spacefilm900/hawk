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
//! Array keys come from input data, so the lookup map uses the standard
//! library's hasher, which is seeded randomly per map: input crafted so its
//! keys collide (making every lookup slow) cannot be prepared in advance.
//! The faster, unseeded `FxMap` below is kept for maps keyed by the
//! program text itself (variable names).

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::rc::Rc;

use crate::value::Value;

/// Array keys are strings (numbers are converted with CONVFMT first).
pub type Key = Rc<[u8]>;

/// A small, fast hash function (the "FxHash" used inside the Rust
/// compiler).  The standard library's default hasher resists deliberate
/// collision attacks at a cost in speed; that resistance is not needed for
/// keys the program text chooses, such as variable names (array keys do
/// need it; see the top of this file).  Writing our own keeps the
/// zero-dependency rule.
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

/// A `HashMap` using `FxHasher`.
pub type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

#[derive(Default, Debug)]
pub struct Array {
    /// `HashMap<K, V>` with no third type parameter uses the standard,
    /// randomly seeded hasher (`RandomState`).
    index: HashMap<Key, usize>,
    entries: Vec<Option<(Key, Value)>>,
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
    pub fn get_or_create(&mut self, key: &Key) -> &mut Value {
        let i = match self.index.get(key) {
            Some(&i) => i,
            None => {
                self.entries.push(Some((key.clone(), Value::Uninit)));
                let i = self.entries.len() - 1;
                self.index.insert(key.clone(), i);
                i
            }
        };
        // `as_mut().unwrap()`: live index entries always point at `Some`.
        &mut self.entries[i].as_mut().unwrap().1
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
