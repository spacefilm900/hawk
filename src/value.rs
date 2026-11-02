//! AWK values: numbers, strings, "strnums" and the uninitialized value.
//!
//! AWK has no declared types.  A value is one of four kinds (see DESIGN.md,
//! "Values"):
//!
//! * `Num`: the result of arithmetic, e.g. `1 + 2`;
//! * `Str`: a string constant or the result of string operations;
//! * `StrNum`: text that came from outside the program (a field, `getline`,
//!   `-v`, `ARGV`, `ENVIRON`) and *looks like a number*.  It compares as a
//!   number but prints as the original text, so `$1 == 1` is true for the
//!   field `1.0` while `print $1` still prints `1.0`;
//! * `Uninit`: a variable nobody assigned yet; it is both 0 and "".
//!
//! Strings are bytes (`[u8]`), not Rust `String`s, because AWK input need
//! not be valid UTF-8 (DESIGN.md, "Bytes").
//!
//! `Rc<[u8]>` is a reference-counted, read-only byte string: cloning it
//! copies a pointer and bumps a counter instead of copying the bytes.
//! Values are copied around a lot (assignment, function arguments, array
//! elements), so this keeps copies cheap.

use std::cmp::Ordering;
use std::rc::Rc;

use crate::format;

/// One AWK value.  `#[derive(Clone, Debug)]` gives `.clone()` and
/// developer printing (`{:?}`) for free.
#[derive(Clone, Debug)]
pub enum Value {
    /// Never assigned: 0 as a number, "" as a string.
    Uninit,
    /// A number (AWK numbers are C doubles; Rust calls them `f64`).
    Num(f64),
    /// A string.
    Str(Rc<[u8]>),
    /// Input text that looks like a number: the text and its value.
    StrNum(Rc<[u8]>, f64),
}

/// The empty string, shared.
pub fn empty() -> Rc<[u8]> {
    Rc::from(&b""[..])
}

impl Value {
    /// A string value from a byte slice (`&[u8]`); the bytes are copied.
    pub fn str(s: &[u8]) -> Value {
        Value::Str(Rc::from(s))
    }

    /// Text from outside the program: a `StrNum` when it looks like a
    /// number (see `looks_numeric`), otherwise a plain `Str`.  pyawk's
    /// `make_strnum`.
    pub fn strnum(s: &[u8]) -> Value {
        match looks_numeric(s) {
            Some(n) => Value::StrNum(Rc::from(s), n),
            None => Value::Str(Rc::from(s)),
        }
    }

    /// The numeric value.  Strings use their longest numeric prefix, as
    /// C's `strtod` does ("3abc" is 3, "abc" is 0).
    pub fn to_num(&self) -> f64 {
        match self {
            Value::Uninit => 0.0,
            Value::Num(n) | Value::StrNum(_, n) => *n,
            Value::Str(s) => str_to_num(s),
        }
    }

    /// The string value; numbers are converted with `convfmt` (CONVFMT,
    /// or OFMT when printing) unless they are integers.
    ///
    /// `Rc<[u8]>` is returned so that strings come back without copying.
    pub fn to_str(&self, convfmt: &[u8]) -> Rc<[u8]> {
        match self {
            Value::Uninit => empty(),
            Value::Str(s) | Value::StrNum(s, _) => s.clone(),
            Value::Num(n) => Rc::from(num_to_str(*n, convfmt)),
        }
    }

    /// AWK truth: numbers are true when non-zero, strings when non-empty.
    /// A strnum counts as a number ("0.0" read from input is false).
    pub fn to_bool(&self) -> bool {
        match self {
            Value::Uninit => false,
            Value::Num(n) | Value::StrNum(_, n) => *n != 0.0,
            Value::Str(s) => !s.is_empty(),
        }
    }

    /// True for a plain string (the one kind that forces string comparison).
    pub fn is_str(&self) -> bool {
        matches!(self, Value::Str(_))
    }
}

/// The whitespace C's `isspace` accepts around numbers: space \t \n \v \f \r.
pub fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

/// Length of the number at the start of `s` (no leading whitespace
/// allowed here): `[+-]? (digits [. digits*] | . digits) ([eE] [+-]? digits)?`.
/// Returns 0 when there is no number.  Hexadecimal, "inf" and "nan" are not
/// numbers, as in pyawk v1.0.
fn number_prefix_len(s: &[u8]) -> usize {
    let mut i = 0;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        i += 1;
    }
    let int_start = i;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    let mut digits = i - int_start;
    if i < s.len() && s[i] == b'.' {
        let frac_start = i + 1;
        let mut j = frac_start;
        while j < s.len() && s[j].is_ascii_digit() {
            j += 1;
        }
        digits += j - frac_start;
        if digits > 0 {
            i = j;
        }
    }
    if digits == 0 {
        return 0;
    }
    // An exponent counts only if digits follow it: "1e" is 1 then "e".
    if i < s.len() && (s[i] == b'e' || s[i] == b'E') {
        let mut j = i + 1;
        if j < s.len() && (s[j] == b'+' || s[j] == b'-') {
            j += 1;
        }
        let exp_start = j;
        while j < s.len() && s[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp_start {
            i = j;
        }
    }
    i
}

/// Parse an already-validated number.  Rust's `parse::<f64>` rounds
/// correctly, like glibc's `strtod`.  It needs a `&str`; the bytes are
/// ASCII, so `from_utf8` cannot fail (`unwrap_or` is belt and braces).
fn parse_number(s: &[u8]) -> f64 {
    std::str::from_utf8(s)
        .ok()
        .and_then(|t| t.parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// String to number the way `strtod` reads it: skip leading whitespace,
/// take the longest numeric prefix, 0 if there is none.
pub fn str_to_num(s: &[u8]) -> f64 {
    let start = s.iter().position(|&b| !is_space(b)).unwrap_or(s.len());
    let rest = &s[start..];
    let len = number_prefix_len(rest);
    if len == 0 {
        0.0
    } else {
        parse_number(&rest[..len])
    }
}

/// `Some(value)` when the *whole* of `s` is a number, allowing whitespace
/// on both sides (POSIX's "numeric string"); `None` otherwise.
pub fn looks_numeric(s: &[u8]) -> Option<f64> {
    let start = s.iter().position(|&b| !is_space(b))?;
    let end = s.iter().rposition(|&b| !is_space(b))? + 1;
    let text = &s[start..end];
    let len = number_prefix_len(text);
    if len > 0 && len == text.len() {
        Some(parse_number(text))
    } else {
        None
    }
}

/// How infinities and NaNs print: "+inf", "-inf", "+nan", "-nan" (the
/// spelling gawk uses; the sign of a NaN is its sign bit).
pub fn special_float_str(n: f64) -> &'static str {
    match (n.is_nan(), n.is_sign_negative()) {
        (true, false) => "+nan",
        (true, true) => "-nan",
        (false, false) => "+inf",
        (false, true) => "-inf",
    }
}

/// Number to string (CONVFMT/OFMT conversion).  Integers below 2^64 in
/// magnitude print as integers ("3", not "3.00000"); everything else goes
/// through the format, e.g. "%.6g".
pub fn num_to_str(n: f64, fmt: &[u8]) -> Vec<u8> {
    if !n.is_finite() {
        return special_float_str(n).as_bytes().to_vec();
    }
    if n == n.trunc() && n.abs() < 18446744073709551616.0 {
        // `as i128` converts exactly here: |n| < 2^64 fits easily.
        return (n as i128).to_string().into_bytes();
    }
    format::format_number(fmt, n)
}

/// AWK comparison of two values.
///
/// Numeric when neither side is a plain string (numbers, strnums and
/// uninitialized values); otherwise both sides are compared as strings,
/// byte by byte.  `None` means "unordered": a NaN was involved, so every
/// comparison except `!=` is false.
pub fn compare(a: &Value, b: &Value, convfmt: &[u8]) -> Option<Ordering> {
    if a.is_str() || b.is_str() {
        Some(a.to_str(convfmt).cmp(&b.to_str(convfmt)))
    } else {
        a.to_num().partial_cmp(&b.to_num())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G6: &[u8] = b"%.6g";

    fn s(v: Vec<u8>) -> String {
        String::from_utf8(v).unwrap()
    }

    #[test]
    fn string_to_number_takes_the_longest_prefix() {
        assert_eq!(str_to_num(b"3abc"), 3.0);
        assert_eq!(str_to_num(b"  -2.5e3x"), -2500.0);
        assert_eq!(str_to_num(b"\t\n+.5"), 0.5);
        assert_eq!(str_to_num(b"1e"), 1.0);
        assert_eq!(str_to_num(b"1e+"), 1.0);
        assert_eq!(str_to_num(b"1."), 1.0);
        assert_eq!(str_to_num(b"."), 0.0);
        assert_eq!(str_to_num(b"abc"), 0.0);
        assert_eq!(str_to_num(b""), 0.0);
        // Not numbers in pyawk v1.0: hex, inf, nan.
        assert_eq!(str_to_num(b"0x1A"), 0.0);
        assert_eq!(str_to_num(b"inf"), 0.0);
        assert_eq!(str_to_num(b"nan"), 0.0);
        // Correct rounding, like strtod.
        assert_eq!(str_to_num(b"0.1"), 0.1);
        assert_eq!(str_to_num(b"1e400"), f64::INFINITY);
    }

    #[test]
    fn numeric_strings() {
        assert_eq!(looks_numeric(b" 1.0 "), Some(1.0));
        assert_eq!(looks_numeric(b"+3"), Some(3.0));
        assert_eq!(looks_numeric(b"1e5\n"), Some(100000.0));
        assert_eq!(looks_numeric(b"1x"), None);
        assert_eq!(looks_numeric(b""), None);
        assert_eq!(looks_numeric(b"  "), None);
        assert_eq!(looks_numeric(b"."), None);
        assert_eq!(looks_numeric(b"0x10"), None);
        assert!(matches!(Value::strnum(b"10"), Value::StrNum(_, n) if n == 10.0));
        assert!(matches!(Value::strnum(b"ten"), Value::Str(_)));
    }

    #[test]
    fn number_to_string() {
        assert_eq!(s(num_to_str(3.0, G6)), "3");
        assert_eq!(s(num_to_str(-0.0, G6)), "0");
        assert_eq!(s(num_to_str(0.1, G6)), "0.1");
        assert_eq!(s(num_to_str(1.0 / 3.0, G6)), "0.333333");
        assert_eq!(s(num_to_str(1e6, G6)), "1000000");
        assert_eq!(s(num_to_str(1e18, G6)), "1000000000000000000");
        assert_eq!(s(num_to_str(2f64.powi(63), G6)), "9223372036854775808");
        assert_eq!(s(num_to_str(1e30, G6)), "1e+30");
        assert_eq!(s(num_to_str(123456.7, G6)), "123457");
        assert_eq!(s(num_to_str(3.14259, b"%.2f")), "3.14");
        assert_eq!(s(num_to_str(f64::INFINITY, G6)), "+inf");
        assert_eq!(s(num_to_str(f64::NEG_INFINITY, G6)), "-inf");
        assert_eq!(s(num_to_str(f64::NAN, G6)), "+nan");
        assert_eq!(s(num_to_str(-f64::NAN, G6)), "-nan");
    }

    #[test]
    fn truth() {
        assert!(!Value::Uninit.to_bool());
        assert!(!Value::Num(0.0).to_bool());
        assert!(Value::Num(0.5).to_bool());
        assert!(!Value::strnum(b"0.0").to_bool());
        assert!(Value::str(b"0").to_bool());
        assert!(!Value::str(b"").to_bool());
    }

    #[test]
    fn comparisons() {
        use Ordering::*;
        let cmp = |a: &Value, b: &Value| compare(a, b, G6);
        assert_eq!(cmp(&Value::Num(2.0), &Value::Num(10.0)), Some(Less));
        // A strnum compares numerically ...
        assert_eq!(cmp(&Value::strnum(b"10"), &Value::Num(9.0)), Some(Greater));
        assert_eq!(
            cmp(&Value::strnum(b"1.0"), &Value::strnum(b"1")),
            Some(Equal)
        );
        // ... but a string constant forces string comparison.
        assert_eq!(cmp(&Value::str(b"10"), &Value::Num(9.0)), Some(Less));
        assert_eq!(cmp(&Value::Uninit, &Value::Num(0.0)), Some(Equal));
        assert_eq!(cmp(&Value::Uninit, &Value::str(b"")), Some(Equal));
        assert_eq!(cmp(&Value::Num(f64::NAN), &Value::Num(1.0)), None);
    }
}
