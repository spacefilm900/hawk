//! The lexer (tokenizer): program text in, a flat list of tokens out.
//!
//! Pipeline: cli -> **lexer** -> parser -> AST -> runtime.  This is a port
//! of pyawk's `lexer.py` (v1.0).  The program text is handled as bytes
//! (`&[u8]`), never as a Rust `String`, so string and regex literals can
//! hold any bytes at all (see DESIGN.md, "Bytes everywhere").
//!
//! Entry points: [`tokenize`] and [`process_escapes`].

/// Interpret AWK string escapes: `\n`, `\t`, `\"`, `\\`, `\/`, `\a \b \f
/// \r \v`, and up to three octal digits.
///
/// Used for string literals and also for `-v` assignments, operand
/// assignments and `-F`.  An undefined escape such as `\q` or `\&` loses its
/// backslash (`"\q"` is `q`), which is what onetrue-awk and gawk do (mawk
/// keeps it).  pyawk follows the majority, and so does hawk; see
/// docs/pyawk-v1.0-DESIGN.md, "Undefined escapes in strings".  A lone
/// backslash at the very end is kept.
///
/// Octal values above `\377` wrap to 8 bits (`"\501"` is `A`), as all three
/// reference awks do.
pub fn process_escapes(s: &[u8]) -> Vec<u8> {
    // Fast path: nothing to do.  `to_vec()` copies the borrowed bytes into
    // a new, owned `Vec<u8>`.
    if !s.contains(&b'\\') {
        return s.to_vec();
    }
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        // A plain byte, or a backslash that is the very last byte: keep it.
        if c != b'\\' || i + 1 == s.len() {
            out.push(c);
            i += 1;
            continue;
        }
        let d = s[i + 1];
        // `if let Some(x) = ...` runs the block only when the Option holds
        // a value, and names that value `x`.
        if let Some(byte) = simple_escape(d) {
            out.push(byte);
            i += 2;
        } else if is_octal(d) {
            // Up to three octal digits after the backslash.
            let mut j = i + 1;
            let mut value: u32 = 0;
            while j < s.len() && j < i + 4 && is_octal(s[j]) {
                value = value * 8 + u32::from(s[j] - b'0');
                j += 1;
            }
            // `as u8` keeps the low 8 bits: the wrap described above.
            out.push(value as u8);
            i = j;
        } else {
            // Undefined escape: drop the backslash, keep the byte.
            out.push(d);
            i += 2;
        }
    }
    out
}

/// The byte a one-letter escape stands for, or `None` if it is not one.
fn simple_escape(d: u8) -> Option<u8> {
    // `match` compares against each pattern in turn; `_` matches anything.
    Some(match d {
        b'"' => b'"',
        b'\\' => b'\\',
        b'/' => b'/',
        b'a' => 0x07,
        b'b' => 0x08,
        b'f' => 0x0c,
        b'n' => b'\n',
        b'r' => b'\r',
        b't' => b'\t',
        b'v' => 0x0b,
        _ => return None,
    })
}

fn is_octal(c: u8) -> bool {
    // `b'0'..=b'7'` is an inclusive range pattern.
    matches!(c, b'0'..=b'7')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `b"..."` is a byte-string literal: a `&[u8]`, not a `&str`.
    fn esc(s: &[u8]) -> Vec<u8> {
        process_escapes(s)
    }

    #[test]
    fn defined_escapes() {
        assert_eq!(esc(br"a\nb"), b"a\nb");
        assert_eq!(esc(br"\t"), b"\t");
        assert_eq!(esc(br#"\"\\\/"#), br#""\/"#);
        assert_eq!(esc(br"\a\b\f\r\v"), b"\x07\x08\x0c\r\x0b");
    }

    #[test]
    fn undefined_escapes_drop_the_backslash() {
        // onetrue-awk and gawk; mawk keeps it.  See the pyawk design notes.
        assert_eq!(esc(br"\q"), b"q");
        assert_eq!(esc(br"\&"), b"&");
        assert_eq!(esc(br"\."), b".");
        assert_eq!(esc(br"\8\9"), b"89"); // 8 and 9 are not octal digits
        // "\\" is one backslash, so "\\&" keeps a backslash before the &.
        assert_eq!(esc(br"\\&"), br"\&");
    }

    #[test]
    fn trailing_backslash_is_kept() {
        assert_eq!(esc(b"\\"), b"\\");
        assert_eq!(esc(b"ab\\"), b"ab\\");
    }

    #[test]
    fn octal_takes_at_most_three_digits() {
        assert_eq!(esc(br"\0101"), b"\x081");
        assert_eq!(esc(br"\101\60"), b"A0");
        assert_eq!(esc(br"\0"), b"\0");
        // Above \377 the value wraps to a byte, as in the C awks.
        assert_eq!(esc(br"\501"), b"A");
    }

    #[test]
    fn other_bytes_pass_through() {
        assert_eq!(esc(b"plain"), b"plain");
        assert_eq!(esc(b"\xff\xfe\\n"), b"\xff\xfe\n");
    }
}
