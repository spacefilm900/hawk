//! `rand()` and `srand()`: the same random numbers as pyawk.
//!
//! POSIX does not say which generator awk uses, and the reference awks all
//! differ, so hawk copies pyawk exactly: Python's `random.Random`, which is
//! the Mersenne Twister (MT19937) seeded the way CPython seeds it.  With the
//! default seed 0, `rand()` gives the same sequence as `python3 -c 'import
//! random; r = random.Random(0); print(r.random())'`.

const N: usize = 624;
const M: usize = 397;

pub struct Random {
    state: [u32; N],
    index: usize,
}

impl Random {
    /// A generator seeded like `random.seed(seed)` for a Python float.
    pub fn new(seed: f64) -> Random {
        // CPython seeds with `hash(seed)` (as an unsigned 64-bit number),
        // cut into 32-bit words, lowest first.
        let mut n = py_hash_float(seed) as u64;
        let mut key = Vec::new();
        while n != 0 {
            key.push(n as u32);
            n >>= 32;
        }
        if key.is_empty() {
            key.push(0);
        }
        let mut r = Random {
            state: [0; N],
            index: N,
        };
        r.init_by_array(&key);
        r
    }

    fn init_genrand(&mut self, s: u32) {
        self.state[0] = s;
        for i in 1..N {
            let prev = self.state[i - 1];
            self.state[i] = 1812433253u32
                .wrapping_mul(prev ^ (prev >> 30))
                .wrapping_add(i as u32);
        }
        self.index = N;
    }

    fn init_by_array(&mut self, key: &[u32]) {
        self.init_genrand(19650218);
        let (mut i, mut j) = (1usize, 0usize);
        let mt = &mut self.state;
        for _ in 0..N.max(key.len()) {
            let prev = mt[i - 1];
            mt[i] = (mt[i] ^ (prev ^ (prev >> 30)).wrapping_mul(1664525))
                .wrapping_add(key[j])
                .wrapping_add(j as u32);
            i += 1;
            j += 1;
            if i >= N {
                mt[0] = mt[N - 1];
                i = 1;
            }
            if j >= key.len() {
                j = 0;
            }
        }
        for _ in 0..N - 1 {
            let prev = mt[i - 1];
            mt[i] = (mt[i] ^ (prev ^ (prev >> 30)).wrapping_mul(1566083941)).wrapping_sub(i as u32);
            i += 1;
            if i >= N {
                mt[0] = mt[N - 1];
                i = 1;
            }
        }
        mt[0] = 0x8000_0000;
    }

    fn next_u32(&mut self) -> u32 {
        if self.index >= N {
            let mt = &mut self.state;
            for k in 0..N {
                let y = (mt[k] & 0x8000_0000) | (mt[(k + 1) % N] & 0x7fff_ffff);
                let mut v = mt[(k + M) % N] ^ (y >> 1);
                if y & 1 != 0 {
                    v ^= 0x9908_b0df;
                }
                mt[k] = v;
            }
            self.index = 0;
        }
        let mut y = self.state[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^= y >> 18;
        y
    }

    /// A number in [0, 1) with 53 random bits, as Python's `random()`.
    pub fn random(&mut self) -> f64 {
        let a = (self.next_u32() >> 5) as f64;
        let b = (self.next_u32() >> 6) as f64;
        (a * 67108864.0 + b) / 9007199254740992.0
    }
}

/// CPython's `hash()` of a float (`_Py_HashDouble`): for whole numbers it
/// is the number itself modulo 2^61 - 1, keeping the sign.
fn py_hash_float(v: f64) -> i64 {
    const BITS: u32 = 61;
    const MODULUS: u64 = (1 << BITS) - 1;
    if v.is_infinite() {
        return if v > 0.0 { 314159 } else { -314159 };
    }
    if v.is_nan() {
        return 0; // CPython hashes NaN by object identity; any value will do
    }
    let (mut m, mut e) = frexp(v);
    let mut sign: i64 = 1;
    if m < 0.0 {
        sign = -1;
        m = -m;
    }
    let mut x: u64 = 0;
    while m != 0.0 {
        x = ((x << 28) & MODULUS) | (x >> (BITS - 28));
        m *= 268435456.0;
        e -= 28;
        let y = m as u64;
        m -= y as f64;
        x += y;
        if x >= MODULUS {
            x -= MODULUS;
        }
    }
    let e = if e >= 0 {
        (e as u32) % BITS
    } else {
        BITS - 1 - ((-1 - e) as u32 % BITS)
    };
    x = ((x << e) & MODULUS) | (x >> (BITS - e));
    let mut h = (x as i64).wrapping_mul(sign);
    if h == -1 {
        h = -2;
    }
    h
}

/// Split `v` into a mantissa in [0.5, 1) and a power of two (C's frexp).
fn frexp(v: f64) -> (f64, i32) {
    if v == 0.0 || !v.is_finite() {
        return (v, 0);
    }
    let bits = v.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32;
    if exp == 0 {
        // Subnormal: scale up into the normal range first.
        let (m, e) = frexp(v * f64::powi(2.0, 64));
        return (m, e - 64);
    }
    let m = f64::from_bits((bits & !(0x7ff << 52)) | (1022 << 52));
    (m, exp - 1022)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_python_random() {
        // python3 -c 'import random; r=random.Random(0); print([r.random() for _ in range(3)])'
        let mut r = Random::new(0.0);
        assert_eq!(r.random(), 0.8444218515250481);
        assert_eq!(r.random(), 0.7579544029403025);
        assert_eq!(r.random(), 0.420571580830845);
        // r = random.Random(); r.seed(s); r.random()
        for (seed, want) in [
            (1.0, 0.13436424411240122),
            (1.5, 0.551763726942059),
            (-2.0, 0.13119521545503066),
            (12345678901.0, 0.9460118159397316),
        ] {
            assert_eq!(Random::new(seed).random(), want, "seed {seed}");
        }
    }

    #[test]
    fn float_hash_like_python() {
        assert_eq!(py_hash_float(0.0), 0);
        assert_eq!(py_hash_float(42.0), 42);
        assert_eq!(py_hash_float(-1.0), -2);
        assert_eq!(py_hash_float(0.5), 1152921504606846976);
        assert_eq!(py_hash_float(1.5), 1152921504606846977);
        assert_eq!(py_hash_float(-3.25), -576460752303423491);
        assert_eq!(py_hash_float(1e300), 1224995262755759164);
    }
}
