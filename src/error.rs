//! The one error type every stage of hawk reports problems with.
//!
//! Rust has no exceptions.  A function that can fail returns
//! `Result<T, Error>`, and callers pass errors upwards with the `?` operator
//! (see DESIGN.md, "Errors").  `cli::main` prints the error as
//! `hawk: <message>` and exits with status 2, the same as pyawk.

use std::fmt;

/// What went wrong.  `#[derive(...)]` asks the compiler to write the
/// standard `Debug` (developer printing) and `PartialEq` (==) code for us.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// Bad program text, found by the lexer or the parser.  `line` is the
    /// 1-based source line, when the error belongs to one.
    Syntax { msg: String, line: Option<u32> },
    /// A fatal problem while the program runs.
    Runtime(String),
    /// Standard output was closed by the reader (e.g. `hawk ... | head`):
    /// the run stops quietly with status 2, as pyawk does.
    BrokenPipe,
}

impl Error {
    /// Shorthand for a syntax error on a known line.
    pub fn syntax(msg: impl Into<String>, line: u32) -> Error {
        // `impl Into<String>` accepts anything convertible to a String
        // (a `&str` literal or an owned `String`), so callers need no `.to_string()`.
        Error::Syntax {
            msg: msg.into(),
            line: Some(line),
        }
    }
}

impl Error {
    /// A syntax error that belongs to no particular line.
    pub fn syntax_no_line(msg: impl Into<String>) -> Error {
        Error::Syntax {
            msg: msg.into(),
            line: None,
        }
    }
}

/// `Display` is the trait behind `{}` in format strings: the user-facing
/// text.  The wording matches pyawk's messages.
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Syntax { msg, line: Some(n) } => {
                write!(f, "syntax error at source line {n}: {msg}")
            }
            Error::Syntax { msg, line: None } => write!(f, "syntax error: {msg}"),
            Error::Runtime(msg) => write!(f, "{msg}"),
            Error::BrokenPipe => write!(f, "broken pipe"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_match_pyawk() {
        assert_eq!(
            Error::syntax("unterminated string", 3).to_string(),
            "syntax error at source line 3: unterminated string"
        );
        assert_eq!(
            Error::Syntax {
                msg: "x".into(),
                line: None
            }
            .to_string(),
            "syntax error: x"
        );
        assert_eq!(
            Error::Runtime("division by zero".into()).to_string(),
            "division by zero"
        );
    }
}
