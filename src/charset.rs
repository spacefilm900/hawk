//! Characters versus bytes: the locale mode hawk runs in (pyawk's
//! `charset.py`).
//!
//! AWK's string functions count *characters*.  What a character is depends
//! on the locale, read from the environment as POSIX describes: the first
//! of `LC_ALL`, `LC_CTYPE` and `LANG` that is set and not empty decides.
//!
//! * **UTF-8 mode**: the locale name contains `UTF-8` or `utf8` (any letter
//!   case).  A valid UTF-8 sequence is one character; classes and
//!   toupper/tolower cover Unicode (tables from gawk, `unicode_tables.rs`).
//! * **C mode**: anything else.  Every byte is a character.
//!
//! hawk keeps strings as bytes in both modes; this module only says where
//! the characters are.  A byte that is not part of a valid UTF-8 sequence
//! (a stray continuation byte, a truncated sequence, an overlong form, an
//! encoded surrogate) is an *invalid byte*: it counts as one character on
//! its own and is never changed, so any input is written back unchanged.
//!
//! In regexes an invalid byte is given a code point of its own, U+DC00
//! plus the byte (U+DC80..U+DCFF, as Python's "surrogateescape" does in
//! pyawk): those values are not characters, so they cannot clash with a
//! real one.

use crate::unicode_tables;

/// First code point used for an invalid byte (0x80 -> U+DC80).
pub const ESCAPED_BASE: u32 = 0xDC00;

/// Is the locale named by the environment a UTF-8 one?  `get` looks a
/// variable up (so tests can pass a fake environment).
pub fn locale_is_utf8(get: impl Fn(&str) -> Option<String>) -> bool {
    for name in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Some(v) = get(name)
            && !v.is_empty()
        {
            let v = v.to_ascii_lowercase();
            return v.contains("utf-8") || v.contains("utf8");
        }
    }
    false
}

/// The mode from the real environment.  A value that is not valid
/// Unicode cannot name a UTF-8 locale, but it still counts as "set".
pub fn utf8_from_env() -> bool {
    locale_is_utf8(|name| std::env::var_os(name).map(|v| v.to_string_lossy().into_owned()))
}

/// Length of the valid UTF-8 sequence starting at `s[i]`, or 1 when the
/// byte there is ASCII or invalid.  The second byte's range rules out
/// overlong forms, surrogates and values above U+10FFFF.
#[inline]
pub fn char_len(s: &[u8], i: usize) -> usize {
    let b = s[i];
    if b < 0x80 {
        return 1;
    }
    let (n, lo, hi) = match b {
        0xC2..=0xDF => (2, 0x80, 0xBF),
        0xE0 => (3, 0xA0, 0xBF),
        0xED => (3, 0x80, 0x9F),
        0xE1..=0xEF => (3, 0x80, 0xBF),
        0xF0 => (4, 0x90, 0xBF),
        0xF4 => (4, 0x80, 0x8F),
        0xF1..=0xF3 => (4, 0x80, 0xBF),
        _ => return 1,
    };
    if i + n > s.len() || !(lo..=hi).contains(&s[i + 1]) {
        return 1;
    }
    if s[i + 2..i + n].iter().all(|&c| (0x80..=0xBF).contains(&c)) {
        n
    } else {
        1
    }
}

/// The character at `s[i]`: (code point, length in bytes).  An invalid
/// byte b is (U+DC00 + b, 1).
#[inline]
pub fn decode_at(s: &[u8], i: usize) -> (u32, usize) {
    let n = char_len(s, i);
    let b = s[i] as u32;
    let c = match n {
        1 if b < 0x80 => b,
        1 => ESCAPED_BASE + b,
        2 => (b & 0x1F) << 6 | (s[i + 1] as u32 & 0x3F),
        3 => (b & 0x0F) << 12 | (s[i + 1] as u32 & 0x3F) << 6 | (s[i + 2] as u32 & 0x3F),
        _ => {
            (b & 0x07) << 18
                | (s[i + 1] as u32 & 0x3F) << 12
                | (s[i + 2] as u32 & 0x3F) << 6
                | (s[i + 3] as u32 & 0x3F)
        }
    };
    (c, n)
}

/// Append the bytes for code point `c`.  An escaped byte (U+DC80..U+DCFF)
/// becomes that byte again; any other value is UTF-8 encoded (surrogates
/// included, which only the regex compiler asks for).
pub fn encode(c: u32, out: &mut Vec<u8>) {
    if (ESCAPED_BASE + 0x80..=ESCAPED_BASE + 0xFF).contains(&c) {
        out.push((c - ESCAPED_BASE) as u8);
    } else {
        utf8_bytes(c, out);
    }
}

/// Plain UTF-8 bit layout for any value below 0x110000 (no checks).
pub fn utf8_bytes(c: u32, out: &mut Vec<u8>) {
    if c < 0x80 {
        out.push(c as u8);
    } else if c < 0x800 {
        out.extend([0xC0 | (c >> 6) as u8, 0x80 | (c & 0x3F) as u8]);
    } else if c < 0x10000 {
        out.extend([
            0xE0 | (c >> 12) as u8,
            0x80 | (c >> 6 & 0x3F) as u8,
            0x80 | (c & 0x3F) as u8,
        ]);
    } else {
        out.extend([
            0xF0 | (c >> 18) as u8,
            0x80 | (c >> 12 & 0x3F) as u8,
            0x80 | (c >> 6 & 0x3F) as u8,
            0x80 | (c & 0x3F) as u8,
        ]);
    }
}

/// Number of characters in `s` (UTF-8 mode).
pub fn count(s: &[u8]) -> usize {
    if s.is_ascii() {
        return s.len();
    }
    let mut i = 0;
    let mut n = 0;
    while i < s.len() {
        i += char_len(s, i);
        n += 1;
    }
    n
}

/// Byte offset of character number `k` (0-based) in `s`, or `s.len()` if
/// there are not that many.
pub fn byte_offset(s: &[u8], k: usize) -> usize {
    let mut i = 0;
    for _ in 0..k {
        if i >= s.len() {
            return s.len();
        }
        i += char_len(s, i);
    }
    i.min(s.len())
}

/// How many characters the first `end` bytes of `s` hold (`end` must be a
/// character boundary).
pub fn chars_before(s: &[u8], end: usize) -> usize {
    count(&s[..end])
}

/// Is `i` the start of a character (or the end of `s`)?  This decodes from
/// the start, so it is O(i); use only on short paths.
pub fn is_boundary(s: &[u8], i: usize) -> bool {
    let mut j = 0;
    while j < i {
        j += char_len(s, j);
    }
    j == i
}

/// `i`, or if `i` is inside a multibyte character, the end of that
/// character.  Only the three bytes before `i` are looked at: a lead byte
/// is never inside another character, so a valid sequence that starts
/// there and reaches past `i` settles it.
pub fn round_up(s: &[u8], i: usize) -> usize {
    if i >= s.len() || !(0x80..0xC0).contains(&s[i]) {
        return i;
    }
    for back in 1..=3.min(i) {
        let j = i - back;
        if s[j] >= 0xC0 {
            let n = char_len(s, j);
            return if n > back { j + n } else { i };
        }
    }
    i
}

/// One past the character starting at `i` (`i < s.len()`), in either mode.
#[inline]
pub fn next(s: &[u8], i: usize, utf8: bool) -> usize {
    if utf8 && i < s.len() {
        i + char_len(s, i)
    } else {
        i + 1
    }
}

/// The code-point ranges of a POSIX class in UTF-8 mode.
pub fn class_ranges(name: &[u8]) -> Option<&'static [(u32, u32)]> {
    unicode_tables::CLASSES
        .iter()
        .find(|(n, _)| n.as_bytes() == name)
        .map(|(_, r)| *r)
}

/// Apply a (first, last, step, delta) case table to one code point.
fn map_case(table: &[(u32, u32, u32, i32)], c: u32) -> u32 {
    // The last run whose first is <= c (runs are sorted and disjoint).
    let k = table.partition_point(|r| r.0 <= c);
    if k == 0 {
        return c;
    }
    let (first, last, step, delta) = table[k - 1];
    if c <= last && (c - first).is_multiple_of(step) {
        (c as i64 + delta as i64) as u32
    } else {
        c
    }
}

/// toupper/tolower.  C mode: ASCII letters only.  UTF-8 mode: every
/// character through gawk's one-to-one tables; invalid bytes are kept
/// (gawk turns them into U+FFFD, onetrue-awk stops: pyawk's decision).
pub fn change_case(s: &[u8], upper: bool, utf8: bool) -> Vec<u8> {
    if !utf8 || s.is_ascii() {
        return if upper {
            s.to_ascii_uppercase()
        } else {
            s.to_ascii_lowercase()
        };
    }
    let table = if upper {
        unicode_tables::UPPER
    } else {
        unicode_tables::LOWER
    };
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let (c, n) = decode_at(s, i);
        if n == 1 && c >= ESCAPED_BASE {
            out.push(s[i]);
        } else {
            utf8_bytes(map_case(table, c), &mut out);
        }
        i += n;
    }
    out
}

/// What printf `%c` prints for the number `n` (already truncated).
/// C mode: the byte n mod 256.  UTF-8 mode, following gawk: a valid code
/// point is that character; 0x110000..0x7FFFFFFF give the old 5- and
/// 6-byte style UTF-8 that glibc still writes; anything else (negative,
/// a surrogate, larger) is the low byte.
pub fn char_for_code(n: f64, utf8: bool, out: &mut Vec<u8>) {
    // NaN and the infinities count as 0 (as for %d).  `as i64` saturates,
    // so huge values are "larger" than any code point; their low byte is
    // 0, since a double of 2^63 or more is a multiple of 2^11.
    let v = if n.is_finite() { n as i64 } else { 0 };
    if utf8 && (0..0x110000).contains(&v) && !(0xD800..=0xDFFF).contains(&v) {
        utf8_bytes(v as u32, out);
    } else if utf8 && (0x110000..=0x7FFF_FFFF).contains(&v) {
        // A lead byte announcing k continuation bytes, each carrying six
        // bits of the number.
        let k = if v < 0x20_0000 {
            3
        } else if v < 0x400_0000 {
            4
        } else {
            5
        };
        let lead = [0xF0u8, 0xF8, 0xFC][k - 3];
        out.push(lead | (v >> (6 * k)) as u8);
        for i in (0..k).rev() {
            out.push(0x80 | (v >> (6 * i) & 0x3F) as u8);
        }
    } else if n.abs() < 9.2e18 {
        out.push(v as u8); // `as u8` keeps the low 8 bits, two's complement
    } else {
        out.push(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_from_the_environment() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(locale_is_utf8(env(&[("LANG", "en_GB.UTF-8")])));
        assert!(locale_is_utf8(env(&[("LC_ALL", "C.utf8")])));
        assert!(!locale_is_utf8(env(&[])));
        // The first one set and not empty decides.
        assert!(!locale_is_utf8(env(&[
            ("LC_ALL", "C"),
            ("LANG", "C.UTF-8")
        ])));
        assert!(locale_is_utf8(env(&[
            ("LC_ALL", ""),
            ("LC_CTYPE", "C.UTF-8"),
            ("LANG", "C")
        ])));
        assert!(!locale_is_utf8(env(&[
            ("LC_CTYPE", "POSIX"),
            ("LANG", "C.UTF-8")
        ])));
    }

    #[test]
    fn round_up_finds_the_next_character() {
        let s = "aé€".as_bytes(); // a C3 A9 E2 82 AC
        let got: Vec<usize> = (0..=s.len()).map(|i| round_up(s, i)).collect();
        assert_eq!(got, [0, 1, 3, 3, 6, 6, 6]);
        // Invalid bytes are characters of their own.
        let s = b"\xc3x\xa9\xe2\x82";
        let got: Vec<usize> = (0..=s.len()).map(|i| round_up(s, i)).collect();
        assert_eq!(got, [0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn invalid_bytes_are_one_character_each() {
        assert_eq!(count("é".as_bytes()), 1);
        assert_eq!(count(b"a\xffb"), 3);
        assert_eq!(count(b"\xe2\x82"), 2); // truncated
        assert_eq!(count(b"\xc0\x80"), 2); // overlong
        assert_eq!(count(b"\xed\xa0\x80"), 3); // encoded surrogate
        assert_eq!(count(b"\xf4\x90\x80\x80"), 4); // above U+10FFFF
        assert_eq!(count("😀".as_bytes()), 1);
        assert_eq!(decode_at(b"\xff", 0), (0xDCFF, 1));
        assert_eq!(decode_at("€".as_bytes(), 0), (0x20AC, 3));
        assert_eq!(byte_offset("aéb".as_bytes(), 2), 3);
        assert_eq!(byte_offset("aéb".as_bytes(), 9), 4);
        assert!(!is_boundary("é".as_bytes(), 1));
    }

    #[test]
    fn case_mapping() {
        let up = |s: &str| String::from_utf8(change_case(s.as_bytes(), true, true)).unwrap();
        assert_eq!(up("straße é ж"), "STRAßE É Ж");
        assert_eq!(
            String::from_utf8(change_case("ÀB".as_bytes(), false, true)).unwrap(),
            "àb"
        );
        assert_eq!(change_case(b"a\xffb", true, true), b"A\xffB");
        assert_eq!(change_case("é".as_bytes(), true, false), "é".as_bytes());
    }

    #[test]
    fn percent_c_codes() {
        let c = |n: f64, utf8| {
            let mut v = Vec::new();
            char_for_code(n, utf8, &mut v);
            v
        };
        assert_eq!(c(233.0, true), "é".as_bytes());
        assert_eq!(c(233.0, false), b"\xe9");
        assert_eq!(c(1114112.0, true), b"\xf4\x90\x80\x80");
        assert_eq!(c(-1.0, true), b"\xff");
        assert_eq!(c(0xD800 as f64, true), b"\x00");
        assert_eq!(c(0x7FFF_FFFF as f64, true), b"\xfd\xbf\xbf\xbf\xbf\xbf");
        assert_eq!(c(2147483648.0, true), b"\x00");
        assert_eq!(c(256.0, false), b"\x00");
        assert_eq!(c(4294967361.0, false), b"A");
        assert_eq!(c(1e30, false), b"\x00");
        assert_eq!(c(-1.0, false), b"\xff");
    }
}
