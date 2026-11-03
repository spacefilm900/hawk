//! printf-style formatting: `printf`, `sprintf`, and CONVFMT/OFMT number
//! conversion.
//!
//! It follows pyawk v1.0's `format_printf`, which has C's semantics.  The
//! digits of `%e %f %g` come from Rust's own float formatting with an
//! explicit precision, which is *exact* (it expands the double's true
//! binary value, e.g. `%.60f`, and rounds an exact tie to even, as glibc
//! does); hawk adds C's layout around them (exponent style `e+05`, `%g`'s
//! choice of style and zero stripping, flags, width, `#`).  Integers too
//! big for 128 bits are printed exactly with a small bignum, as pyawk does
//! with Python integers.
//!
//! The unit test `matches_the_c_library` checks this module against about
//! 18,000 answers of the C library's `snprintf`, recorded in
//! `tests/data/printf_table.tsv` by `tools/printf_table.py`.
//!
//! A format is text with conversions of the form
//! `% [flags -+ #0] [width or *] [. precision or *] letter`, for example
//! `%-10s` or `%8.2f`.  Each conversion takes the next argument; missing
//! arguments count as uninitialized (0 or "").

use crate::value::{Value, special_float_str};

/// The largest width or precision used, whether written in the format
/// (`%99999999999d`) or given by `*`: bigger ones are cut to this (1 MiB),
/// so a typo cannot make hawk allocate gigabytes.
const MAX_WIDTH: usize = 1 << 20;

/// One parsed conversion.
#[derive(Default, Clone)]
struct Spec {
    minus: bool,
    plus: bool,
    space: bool,
    hash: bool,
    zero: bool,
    width: Option<usize>,
    /// `*` given for the width.
    width_star: bool,
    /// The precision; `Some(0)` for a bare `.`.
    prec: Option<usize>,
    prec_star: bool,
    conv: u8,
}

/// A piece of a parsed format: literal bytes or a conversion together with
/// its original text (unknown conversions are copied out verbatim).
enum Piece<'a> {
    Text(&'a [u8]),
    Conv(Spec, &'a [u8]),
}

/// Read decimal digits starting at `*i`; `None` if there are none.
fn digits(fmt: &[u8], i: &mut usize) -> Option<usize> {
    let start = *i;
    let mut n: usize = 0;
    while *i < fmt.len() && fmt[*i].is_ascii_digit() {
        // `saturating_*` stops at the maximum instead of overflowing.
        n = n
            .saturating_mul(10)
            .saturating_add((fmt[*i] - b'0') as usize);
        *i += 1;
    }
    (*i > start).then_some(n)
}

/// Split a format into pieces.  A `%` that does not start a well-formed
/// conversion is ordinary text, like pyawk's regular expression does it.
fn parse(fmt: &[u8]) -> Vec<Piece<'_>> {
    let mut pieces = Vec::new();
    let mut text_start = 0;
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i] != b'%' {
            i += 1;
            continue;
        }
        let start = i;
        let mut j = i + 1;
        let mut spec = Spec::default();
        while j < fmt.len() {
            match fmt[j] {
                b'-' => spec.minus = true,
                b'+' => spec.plus = true,
                b' ' => spec.space = true,
                b'#' => spec.hash = true,
                b'0' => spec.zero = true,
                _ => break,
            }
            j += 1;
        }
        if j < fmt.len() && fmt[j] == b'*' {
            spec.width_star = true;
            j += 1;
        } else {
            spec.width = digits(fmt, &mut j).map(|w| w.min(MAX_WIDTH));
        }
        if j < fmt.len() && fmt[j] == b'.' {
            j += 1;
            if j < fmt.len() && fmt[j] == b'*' {
                spec.prec_star = true;
                j += 1;
            } else {
                spec.prec = Some(digits(fmt, &mut j).unwrap_or(0).min(MAX_WIDTH));
            }
        }
        // C's "length modifiers" (`h`, `l`, `ll`, `L`, `q`, `j`, `z`, `t`, as
        // in `%ld`) mean nothing in awk, where every number is a double;
        // mawk, onetrue-awk and gawk skip them, and so does pyawk v1.1.
        let mods = j;
        while j < fmt.len() && b"hlLqjzt".contains(&fmt[j]) {
            j += 1;
        }
        // With no letter after them (`%l|`), the last modifier is itself
        // the (unknown) conversion letter, so the text is copied verbatim
        // as gawk does; pyawk's regex gets there by backtracking.
        let letter_follows = j < fmt.len() && (fmt[j].is_ascii_alphabetic() || fmt[j] == b'%');
        if !letter_follows && j > mods {
            j -= 1;
        }
        if j < fmt.len() && (fmt[j].is_ascii_alphabetic() || fmt[j] == b'%') {
            spec.conv = fmt[j];
            if start > text_start {
                pieces.push(Piece::Text(&fmt[text_start..start]));
            }
            pieces.push(Piece::Conv(spec, &fmt[start..=j]));
            i = j + 1;
            text_start = i;
        } else {
            i += 1; // not a conversion: the % stays as text
        }
    }
    if text_start < fmt.len() {
        pieces.push(Piece::Text(&fmt[text_start..]));
    }
    pieces
}

/// Number to integer for `%c` and `*`: truncate toward zero; inf and NaN
/// become 0, huge values saturate (`as` does that in Rust).
fn to_c_int(n: f64) -> i128 {
    if n.is_finite() { n as i128 } else { 0 }
}

/// Pad `body` to the width: on the right with `-`, otherwise on the left.
fn pad(out: &mut Vec<u8>, body: &[u8], spec: &Spec) {
    let w = spec.width.unwrap_or(0);
    let fill = w.saturating_sub(body.len());
    if !spec.minus {
        out.extend(std::iter::repeat_n(b' ', fill));
    }
    out.extend_from_slice(body);
    if spec.minus {
        out.extend(std::iter::repeat_n(b' ', fill));
    }
}

/// Pad a number: `prefix` is the sign and/or `0x`, `digits` the rest.
/// The `0` flag puts zeros between them (unless `zero_ok` is false: C
/// ignores `0` for integers with a precision).
fn pad_number(out: &mut Vec<u8>, prefix: &[u8], digits: &[u8], spec: &Spec, zero_ok: bool) {
    let w = spec.width.unwrap_or(0);
    let len = prefix.len() + digits.len();
    if spec.zero && !spec.minus && zero_ok && len < w {
        out.extend_from_slice(prefix);
        out.extend(std::iter::repeat_n(b'0', w - len));
        out.extend_from_slice(digits);
    } else {
        let mut body = prefix.to_vec();
        body.extend_from_slice(digits);
        pad(out, &body, spec);
    }
}

/// The digits of a whole number held as 32-bit "limbs", least significant
/// first, in base 8, 10 or 16 (`upper` for `X`).  Used for the integers too
/// big for `i128`: AWK numbers go up to about 1.8e308, and pyawk prints
/// `%d` and `%x` of them exactly, as Python's integers allow.
fn big_digits(mut limbs: Vec<u32>, radix: u32, upper: bool) -> Vec<u8> {
    let table: &[u8; 16] = if upper {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let mut out = Vec::new();
    // Schoolbook division by the radix, from the top limb down; each pass
    // yields one digit (the remainder), least significant first.
    while limbs.iter().any(|&l| l != 0) {
        let mut rem: u64 = 0;
        for l in limbs.iter_mut().rev() {
            let cur = (rem << 32) | *l as u64;
            *l = (cur / radix as u64) as u32;
            rem = cur % radix as u64;
        }
        out.push(table[rem as usize]);
        while limbs.last() == Some(&0) {
            limbs.pop();
        }
    }
    if out.is_empty() {
        out.push(b'0');
    }
    out.reverse();
    out
}

/// The magnitude of a whole, finite `x` as limbs: x = m * 2^e exactly,
/// with m the 53-bit significand, so it is m shifted left by e bits.
fn big_limbs(x: f64) -> Vec<u32> {
    let bits = x.abs().to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i64;
    let mut m = bits & ((1 << 52) - 1);
    let mut e = exp - 1075;
    if exp == 0 {
        e += 1; // subnormal: no hidden bit
    } else {
        m |= 1 << 52;
    }
    if e < 0 {
        // Only reached for |x| < 2^53 (x is whole), where the shift is exact.
        m >>= -e;
        e = 0;
    }
    let e = e as usize;
    let mut limbs = vec![0u32; e / 32 + 3];
    let (word, bit) = (e / 32, e % 32);
    let wide = (m as u128) << bit;
    for k in 0..3 {
        limbs[word + k] = (wide >> (32 * k)) as u32;
    }
    limbs
}

/// |limbs - 2^64| (for unsigned conversions of negative numbers, below).
fn abs_minus_2_64(mut limbs: Vec<u32>) -> Vec<u32> {
    limbs.resize(limbs.len().max(3), 0);
    let big = limbs[2..].iter().any(|&l| l != 0);
    let low = limbs[0] as u64 | (limbs[1] as u64) << 32;
    if !big {
        // Below 2^64: the answer is 2^64 - low, which fits in 64 bits
        // (low > 0 here, since 0 is not negative).
        let r = 0u64.wrapping_sub(low);
        return vec![r as u32, (r >> 32) as u32];
    }
    // At least 2^64: subtract 1 from the limb holding the 2^64 bit,
    // borrowing upward.
    for l in limbs[2..].iter_mut() {
        let (v, borrow) = l.overflowing_sub(1);
        *l = v;
        if !borrow {
            break;
        }
    }
    limbs
}

/// `%d %i %o %u %x %X` (C rules, see pyawk's `_format_c_int`).
fn format_int(out: &mut Vec<u8>, spec: &Spec, n: f64) {
    let signed = matches!(spec.conv, b'd' | b'i');
    let n = if n.is_finite() { n.trunc() } else { 0.0 };
    let radix = match spec.conv {
        b'o' => 8,
        b'x' | b'X' => 16,
        _ => 10,
    };
    let upper = spec.conv == b'X';
    let negative = n < 0.0;
    // Negative numbers are reinterpreted as unsigned 64-bit values by the
    // unsigned conversions: n + 2^64 (which is still negative, so printed
    // by magnitude, when n < -2^64; Python's arithmetic, as in pyawk).
    let mut digits = if n.abs() < 1e38 {
        let mut v = n as i128;
        if !signed && v < 0 {
            v += 1 << 64;
        }
        let mag = v.unsigned_abs();
        match radix {
            8 => format!("{mag:o}"),
            16 if upper => format!("{mag:X}"),
            16 => format!("{mag:x}"),
            _ => mag.to_string(),
        }
        .into_bytes()
    } else if !signed && negative {
        big_digits(abs_minus_2_64(big_limbs(n)), radix, upper)
    } else {
        big_digits(big_limbs(n), radix, upper)
    };
    // From here on `v` only matters through its sign and zero-ness.
    let v: i8 = if n == 0.0 || (!signed && n == -18446744073709551616.0) {
        0
    } else if negative {
        -1
    } else {
        1
    };
    if let Some(p) = spec.prec {
        if p == 0 && v == 0 {
            digits.clear();
        } else if digits.len() < p {
            let mut z = vec![b'0'; p - digits.len()];
            z.extend_from_slice(&digits);
            digits = z;
        }
    }
    let mut prefix: Vec<u8> = Vec::new();
    if signed {
        if v < 0 {
            prefix.push(b'-');
        } else if spec.plus {
            prefix.push(b'+');
        } else if spec.space {
            prefix.push(b' ');
        }
    }
    if spec.hash {
        if spec.conv == b'o' && digits.first() != Some(&b'0') {
            digits.insert(0, b'0');
        } else if matches!(spec.conv, b'x' | b'X') && v != 0 {
            prefix.extend_from_slice(if spec.conv == b'x' { b"0x" } else { b"0X" });
        }
    }
    pad_number(out, &prefix, &digits, spec, spec.prec.is_none());
}

/// C's exponent style: `e+05`, `e-123` (at least two digits).  Rust writes
/// `1.5e5`; this rewrites the part after the `e`.
fn c_exponent(rust: &str, upper: bool) -> String {
    let (mant, exp) = rust.split_once('e').unwrap_or((rust, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let sign = if exp < 0 { '-' } else { '+' };
    let e = if upper { 'E' } else { 'e' };
    format!("{mant}{e}{sign}{:02}", exp.abs())
}

/// Drop trailing zeros after a decimal point, then the point itself
/// (what `%g` does without `#`).
fn strip_zeros(s: &mut String) {
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
}

/// `{a:.p$}` for any precision.  Rust's formatter rejects (panics on)
/// precisions above 65535, so beyond FIXED_EXACT decimals, where every
/// double's exact expansion has already ended (the smallest has 1074),
/// the remaining digits are zeros added by hand.
fn fixed(a: f64, p: usize) -> String {
    const FIXED_EXACT: usize = 1100;
    if p <= FIXED_EXACT {
        return format!("{a:.p$}");
    }
    let mut s = format!("{a:.FIXED_EXACT$}");
    s.extend(std::iter::repeat_n('0', p - FIXED_EXACT));
    s
}

/// `{a:.p$e}` (Rust's style, `1.5e5`) for any precision; a double has at
/// most 767 significant decimal digits, so past SCI_EXACT they are zeros.
fn sci(a: f64, p: usize) -> String {
    const SCI_EXACT: usize = 800;
    if p <= SCI_EXACT {
        return format!("{a:.p$e}");
    }
    let s = format!("{a:.SCI_EXACT$e}");
    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
    format!("{mant}{}e{exp}", "0".repeat(p - SCI_EXACT))
}

/// The digits of |x| for `%e %f %g` (no sign; x is finite).
fn float_body(spec: &Spec, x: f64) -> String {
    let a = x.abs();
    let upper = spec.conv.is_ascii_uppercase();
    let p = spec.prec.unwrap_or(6);
    let mut s = match spec.conv {
        b'f' | b'F' => fixed(a, p),
        b'e' | b'E' => c_exponent(&sci(a, p), upper),
        _ => {
            // %g: P significant digits; style e if the exponent is < -4
            // or >= P, otherwise style f; then trailing zeros go.
            let p = if p == 0 { 1 } else { p };
            let e_form = sci(a, p - 1);
            let exp: i32 = e_form
                .split_once('e')
                .map_or(0, |(_, e)| e.parse().unwrap_or(0));
            let mut s = if exp < -4 || exp >= p as i32 {
                let (mant, rest) = e_form.split_once('e').unwrap_or((&e_form, "0"));
                let mut m = mant.to_string();
                if !spec.hash {
                    strip_zeros(&mut m);
                }
                c_exponent(&format!("{m}e{rest}"), upper)
            } else {
                let decimals = (p as i32 - 1 - exp).max(0) as usize;
                let mut f = fixed(a, decimals);
                if !spec.hash {
                    strip_zeros(&mut f);
                }
                f
            };
            if spec.hash && !s.contains('.') {
                // `#` keeps the point even with nothing after it.
                match s.find(['e', 'E']) {
                    Some(k) => s.insert(k, '.'),
                    None => s.push('.'),
                }
            }
            return s;
        }
    };
    if spec.hash && !s.contains('.') {
        match s.find(['e', 'E']) {
            Some(k) => s.insert(k, '.'),
            None => s.push('.'),
        }
    }
    s
}

/// `%e %E %f %F %g %G`.
fn format_float(out: &mut Vec<u8>, spec: &Spec, x: f64) {
    if !x.is_finite() {
        let text = special_float_str(x);
        let text = if spec.conv.is_ascii_uppercase() {
            text.to_ascii_uppercase()
        } else {
            text.to_string()
        };
        pad(out, text.as_bytes(), spec);
        return;
    }
    let prefix: &[u8] = if x.is_sign_negative() {
        b"-"
    } else if spec.plus {
        b"+"
    } else if spec.space {
        b" "
    } else {
        b""
    };
    let body = float_body(spec, x);
    pad_number(out, prefix, body.as_bytes(), spec, true);
}

/// `printf`/`sprintf`: format `args` with `fmt`.  `convfmt` converts
/// numbers given to `%s`/`%c`.
pub fn format_printf(fmt: &[u8], args: &[Value], convfmt: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut next = 0;
    // A closure that hands out the next argument (None when they ran
    // out).  `move` is not needed: it borrows `args` and `next`.
    let mut take = || {
        let v = args.get(next).cloned();
        next += 1;
        v
    };
    for piece in parse(fmt) {
        let (mut spec, raw) = match piece {
            Piece::Text(t) => {
                out.extend_from_slice(t);
                continue;
            }
            Piece::Conv(spec, raw) => (spec, raw),
        };
        if spec.conv == b'%' {
            out.push(b'%');
            continue;
        }
        let known = b"diouxXeEfFgGcs".contains(&spec.conv);
        if !known && !spec.width_star && !spec.prec_star {
            out.extend_from_slice(raw); // unknown conversion: verbatim
            continue;
        }
        if spec.width_star {
            let w = to_c_int(take().unwrap_or(Value::Uninit).to_num());
            if w < 0 {
                spec.minus = true;
            }
            spec.width = Some(w.unsigned_abs().min(MAX_WIDTH as u128) as usize);
        }
        if spec.prec_star {
            let p = to_c_int(take().unwrap_or(Value::Uninit).to_num());
            spec.prec = (p >= 0).then(|| p.min(MAX_WIDTH as i128) as usize);
        }
        if !known {
            out.extend_from_slice(raw);
            continue;
        }
        let arg = take();
        // `%c` is the one conversion where a missing argument differs from
        // an unset variable (see below); for the others both are 0 or "".
        if spec.conv == b'c' {
            let ch: Vec<u8> = match arg {
                None => Vec::new(),
                // An unset variable is the number 0: a NUL byte.
                Some(Value::Uninit) => vec![0],
                // A numeric string (such as a field "65") counts as a
                // number here, as POSIX says: `|` matches either variant.
                Some(Value::Num(n) | Value::StrNum(_, n)) => {
                    let c = to_c_int(n);
                    if (0..256).contains(&c) {
                        vec![c as u8]
                    } else {
                        // `u32::try_from` fails for negative codes and
                        // ones past 32 bits (`as u32` would keep the low
                        // bits); `char::from_u32` then rejects anything
                        // that is not a Unicode scalar value.
                        u32::try_from(c)
                            .ok()
                            .and_then(char::from_u32)
                            .map(|ch| ch.to_string().into_bytes())
                            .unwrap_or_default()
                    }
                }
                Some(other) => other.to_str(convfmt).iter().take(1).copied().collect(),
            };
            pad(&mut out, &ch, &spec);
            continue;
        }
        let v = arg.unwrap_or(Value::Uninit);
        match spec.conv {
            b'd' | b'i' | b'o' | b'u' | b'x' | b'X' => format_int(&mut out, &spec, v.to_num()),
            b's' => {
                let s = v.to_str(convfmt);
                let s = match spec.prec {
                    Some(p) if p < s.len() => &s[..p],
                    _ => &s[..],
                };
                pad(&mut out, s, &spec);
            }
            _ => format_float(&mut out, &spec, v.to_num()),
        }
    }
    out
}

/// CONVFMT/OFMT conversion of a non-integer number.  A format with exactly
/// one numeric conversion is used as is; anything else falls back to
/// `%.6g`, as pyawk does for formats Python rejects.
pub fn format_number(fmt: &[u8], n: f64) -> Vec<u8> {
    let pieces = parse(fmt);
    let mut convs = pieces.iter().filter_map(|p| match p {
        Piece::Conv(s, _) if s.conv != b'%' => Some(s),
        _ => None,
    });
    let ok = match (convs.next(), convs.next()) {
        (Some(s), None) => !s.width_star && !s.prec_star && b"dieEfFgG".contains(&s.conv),
        _ => false,
    };
    if ok {
        format_printf(fmt, &[Value::Num(n)], b"%.6g")
    } else {
        format_printf(b"%.6g", &[Value::Num(n)], b"%.6g")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(fmt: &str, args: &[Value]) -> String {
        String::from_utf8(format_printf(fmt.as_bytes(), args, b"%.6g")).unwrap()
    }
    fn n(x: f64) -> Value {
        Value::Num(x)
    }
    fn s(x: &str) -> Value {
        Value::str(x.as_bytes())
    }

    #[test]
    fn integers() {
        assert_eq!(
            f("%d|%5d|%-5d|%05d", &[n(42.9), n(-3.0), n(7.0), n(-12.0)]),
            "42|   -3|7    |-0012"
        );
        assert_eq!(f("%+d % d %i", &[n(5.0), n(5.0), n(-1.5)]), "+5  5 -1");
        assert_eq!(
            f(
                "%o %x %X %#o %#x %#x",
                &[n(8.0), n(255.0), n(255.0), n(8.0), n(255.0), n(0.0)]
            ),
            "10 ff FF 010 0xff 0"
        );
        assert_eq!(
            f("%.3d|%.0d|%08.3d", &[n(7.0), n(0.0), n(5.0)]),
            "007||     005"
        );
        assert_eq!(f("%u", &[n(-1.0)]), "18446744073709551615");
        assert_eq!(f("%x", &[n(-1.0)]), "ffffffffffffffff");
        assert_eq!(f("%d %d", &[n(f64::INFINITY), n(f64::NAN)]), "0 0");
    }

    #[test]
    fn integers_beyond_64_bits_are_exact() {
        // pyawk v1.0's answers (Python integers); C would overflow here.
        let big = |x: f64| [n(x)];
        assert_eq!(
            f("%o", &big(1e40)),
            "165431237070327122400000000000000000000000000"
        );
        assert_eq!(f("%x", &big(-1e40)), "1d6329f1c35ca4ffff0000000000000000");
        assert_eq!(
            f("%u", &big(-1e40)),
            "10000000000000000303767581682929957339136"
        );
        assert_eq!(
            f("%X", &big(2f64.powi(200))),
            format!("1{}", "0".repeat(50))
        );
        assert_eq!(
            f(
                "%#x|%u",
                &[n(-2f64.powi(64)), n(-2f64.powi(64) - 2f64.powi(20))]
            ),
            "0|1048576"
        );
        assert_eq!(
            f("%x", &big(2f64.powi(127))),
            "80000000000000000000000000000000"
        );
        assert_eq!(f("%d", &big(1e30)), "1000000000000000019884624838656");
        assert_eq!(f("%.3d|%+d", &[n(-1e40), n(1e40)]).len(), 42 + 43);
    }

    #[test]
    fn floats() {
        assert_eq!(
            f("%f %.2f %.0f", &[n(3.14259), n(2.675), n(2.5)]),
            "3.142590 2.67 2"
        );
        assert_eq!(
            f("%e %.2E", &[n(12345.678), n(0.000123)]),
            "1.234568e+04 1.23E-04"
        );
        assert_eq!(
            f(
                "%g %g %g %g",
                &[n(100000.0), n(1000000.0), n(0.0001), n(0.00001)]
            ),
            "100000 1e+06 0.0001 1e-05"
        );
        assert_eq!(
            f("%.3g %G %#g", &[n(3.14259), n(1e-10), n(1.5)]),
            "3.14 1E-10 1.50000"
        );
        assert_eq!(
            f(
                "%8.3f|%-8.2f|%+.1f|%08.2f",
                &[n(3.14259), n(2.5), n(1.0), n(-1.5)]
            ),
            "   3.143|2.50    |+1.0|-0001.50"
        );
        assert_eq!(f("%g %g", &[n(0.0), n(-0.0)]), "0 -0");
        assert_eq!(
            f(
                "%f %5.1E %G",
                &[n(f64::INFINITY), n(f64::NEG_INFINITY), n(f64::NAN)]
            ),
            "+inf  -INF +NAN"
        );
        assert_eq!(
            f("%.0e %#.0e %#.0f", &[n(5.0), n(5.0), n(5.0)]),
            "5e+00 5.e+00 5."
        );
    }

    #[test]
    fn strings_and_chars() {
        assert_eq!(
            f(
                "[%s][%5s][%-5s][%.2s]",
                &[s("ab"), s("ab"), s("ab"), s("abc")]
            ),
            "[ab][   ab][ab   ][ab]"
        );
        assert_eq!(f("%s %s", &[n(0.1), n(3.0)]), "0.1 3");
        assert_eq!(f("%c%c%c", &[n(65.0), s("hello"), s("")]), "Ah");
        assert_eq!(f("%3c|%-3c", &[s("x"), n(66.0)]), "  x|B  ");
    }

    #[test]
    fn char_of_numeric_strings_and_uninitialized_values() {
        // A field such as "65" is a numeric string: %c uses its number, as
        // POSIX says and gawk, mawk and onetrue-awk all do.  An unset
        // variable is the number 0, so %c writes a NUL byte; a missing
        // argument still writes nothing.
        let strnum = Value::StrNum(b"65".as_slice().into(), 65.0);
        assert_eq!(f("%c", &[strnum]), "A");
        assert_eq!(f("[%c]", &[Value::Uninit]), "[\0]");
        assert_eq!(f("[%c]", &[]), "[]");
    }

    #[test]
    fn char_of_out_of_range_codes_is_empty() {
        // Like pyawk: a code below 0 or past U+10FFFF writes nothing, even
        // when its low 32 bits would make a valid character.
        assert_eq!(f("[%c]", &[n(-1.0)]), "[]");
        assert_eq!(f("[%c]", &[n(-4294967231.0)]), "[]");
        assert_eq!(f("[%c]", &[n(4294967361.0)]), "[]");
        assert_eq!(f("[%c]", &[n(1114112.0)]), "[]");
        assert_eq!(f("[%c]", &[n(1114111.0)]), "[\u{10FFFF}]");
    }

    #[test]
    fn stars_missing_args_and_odd_formats() {
        assert_eq!(
            f(
                "%*d|%-*d|%.*f",
                &[n(4.0), n(7.0), n(3.0), n(1.0), n(2.0), n(3.14259)]
            ),
            "   7|1  |3.14"
        );
        assert_eq!(f("%*d", &[n(-4.0), n(7.0)]), "7   ");
        assert_eq!(f("%d %s|", &[]), "0 |");
        assert_eq!(f("100%% %5%|%z|%", &[]), "100% %|%z|%");
        assert_eq!(f("%*z|", &[n(3.0)]), "%*z|");
        assert_eq!(f("no conversions", &[n(1.0)]), "no conversions");
    }

    #[test]
    fn c_length_modifiers_are_ignored() {
        assert_eq!(
            f("%ld %lld %hd %hhd|", &[n(1.0), n(2.0), n(3.0), n(4.0)]),
            "1 2 3 4|"
        );
        assert_eq!(
            f(
                "%Lf %zu %jd %qd %td",
                &[n(4.5), n(5.0), n(6.0), n(7.0), n(8.0)]
            ),
            "4.500000 5 6 7 8"
        );
        assert_eq!(
            f("%5.2lf|%-4hs|%lc", &[n(1.23456), s("ab"), n(65.0)]),
            " 1.23|ab  |A"
        );
        // No letter after them: copied as text, like an unknown conversion.
        assert_eq!(f("%l|%5ll|%lk", &[n(1.0)]), "%l|%5ll|%lk");
    }

    #[test]
    fn huge_widths_and_precisions_are_capped() {
        // Literal widths and precisions are capped like `*` ones, at
        // MAX_WIDTH, instead of trying to allocate gigabytes.
        let cap = MAX_WIDTH;
        assert_eq!(f("%99999999999d", &[n(1.0)]).len(), cap);
        assert_eq!(f("%*d", &[n(1e12), n(1.0)]).len(), cap);
        assert_eq!(f("%-99999999999s|", &[s("x")]).len(), cap + 1);
        assert_eq!(f("%.99999999999d", &[n(7.0)]).len(), cap);
        // "0." then the capped number of decimals.
        assert_eq!(f("%.99999999999f", &[n(0.5)]).len(), cap + 2);
        assert_eq!(f("%.*f", &[n(1e12), n(0.5)]).len(), cap + 2);
        // Precisions past 65535 (Rust's own limit) work for every style.
        let long = f(
            "%.70000f|%.70000e|%#.70000g|%.70000g",
            &[n(0.1), n(1e300), n(2.5), n(1.0 / 3.0)],
        );
        let parts: Vec<&str> = long.split('|').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            [70002, 70007, 70001, 56]
        );
        assert!(parts[0].starts_with("0.1000000000000000055511151231257827"));
        assert!(parts[0].ends_with("000"));
        assert!(parts[1].starts_with("1.0000000000000000525") && parts[1].ends_with("0e+300"));
        // (lengths and digits as glibc prints them; %g drops the zeros)
        assert_eq!(
            parts[3],
            "0.333333333333333314829616256247390992939472198486328125"
        );
    }

    #[test]
    fn convfmt() {
        let g = |fmt: &str, x: f64| String::from_utf8(format_number(fmt.as_bytes(), x)).unwrap();
        assert_eq!(g("%.6g", 0.1), "0.1");
        assert_eq!(g("%.2f", 3.14259), "3.14");
        assert_eq!(g("%d", 2.5), "2");
        assert_eq!(g("x%.3gy", 2.5), "x2.5y");
        assert_eq!(g("%s", 2.5), "2.5");
        assert_eq!(g("%d%d", 2.5), "2.5");
        assert_eq!(g("hello", 2.5), "2.5");
    }

    /// Every line of tests/data/printf_table.tsv: the C library's answer
    /// for one format and argument (see tools/printf_table.py).
    #[test]
    fn matches_the_c_library() {
        // `include_str!` builds the file into the test binary.
        let table = include_str!("../tests/data/printf_table.tsv");
        let mut bad = Vec::new();
        let mut count = 0;
        for line in table.lines() {
            let mut cols = line.splitn(4, '\t');
            let (kind, fmt, arg) = (
                cols.next().unwrap(),
                cols.next().unwrap(),
                cols.next().unwrap(),
            );
            let want = cols.next().unwrap_or("");
            let v = match kind {
                "f" => n(f64::from_bits(u64::from_str_radix(arg, 16).unwrap())),
                "i" => n(arg.parse::<i64>().unwrap() as f64),
                _ => {
                    let bytes: Vec<u8> = (0..arg.len())
                        .step_by(2)
                        .map(|i| u8::from_str_radix(&arg[i..i + 2], 16).unwrap())
                        .collect();
                    Value::str(&bytes)
                }
            };
            let got = f(fmt, &[v]);
            // glibc's one known slip: with `#`, a %g value that rounds up
            // to the next power of ten (999999.5 at 6 digits) loses its
            // zeros: "1.e+06".  C and POSIX ask for "1.00000e+06", which
            // is what pyawk prints, and hawk follows pyawk (DESIGN.md).
            let want = match (fmt, want) {
                ("%#g", "1.e+06") => "1.00000e+06",
                ("%#G", "1.E+06") => "1.00000E+06",
                _ => want,
            };
            if got != want {
                bad.push(format!(
                    "{kind} {fmt:?} {arg}: got {got:?}, C says {want:?}"
                ));
            }
            count += 1;
        }
        assert!(count > 10_000, "table too small: {count}");
        assert!(
            bad.is_empty(),
            "{} of {count} differ:\n{}",
            bad.len(),
            bad[..bad.len().min(20)].join("\n")
        );
    }
}
