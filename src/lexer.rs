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

use crate::error::Error;

/// The builtin functions.  They are tokens of their own because the parser
/// treats them differently from user functions: `length` may appear
/// without parentheses, and a space before `(` is allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    Length,
    Substr,
    Index,
    Split,
    Sub,
    Gsub,
    Match,
    Sprintf,
    Sin,
    Cos,
    Atan2,
    Exp,
    Log,
    Sqrt,
    Int,
    Rand,
    Srand,
    Tolower,
    Toupper,
    System,
    Close,
    Fflush,
}

impl Builtin {
    /// How many arguments the builtin takes: `(fewest, most)`.  `sprintf`
    /// has no upper limit, written as `usize::MAX` (the largest number a
    /// `usize` can hold).  The parser rejects any other count (pyawk v1.1,
    /// mawk, gawk).
    pub fn arity(self) -> (usize, usize) {
        use Builtin::*;
        match self {
            Length | Srand | Fflush => (0, 1),
            Rand => (0, 0),
            Substr | Split | Sub | Gsub => (2, 3),
            Index | Match | Atan2 => (2, 2),
            Sprintf => (1, usize::MAX),
            Sin | Cos | Exp | Log | Sqrt | Int | Tolower | Toupper | System | Close => (1, 1),
        }
    }

    /// The builtin's AWK name.
    pub fn name(self) -> &'static str {
        use Builtin::*;
        match self {
            Length => "length",
            Substr => "substr",
            Index => "index",
            Split => "split",
            Sub => "sub",
            Gsub => "gsub",
            Match => "match",
            Sprintf => "sprintf",
            Sin => "sin",
            Cos => "cos",
            Atan2 => "atan2",
            Exp => "exp",
            Log => "log",
            Sqrt => "sqrt",
            Int => "int",
            Rand => "rand",
            Srand => "srand",
            Tolower => "tolower",
            Toupper => "toupper",
            System => "system",
            Close => "close",
            Fflush => "fflush",
        }
    }

    /// The builtin called `name`, if there is one.
    pub fn from_name(name: &str) -> Option<Builtin> {
        use Builtin::*;
        Some(match name {
            "length" => Length,
            "substr" => Substr,
            "index" => Index,
            "split" => Split,
            "sub" => Sub,
            "gsub" => Gsub,
            "match" => Match,
            "sprintf" => Sprintf,
            "sin" => Sin,
            "cos" => Cos,
            "atan2" => Atan2,
            "exp" => Exp,
            "log" => Log,
            "sqrt" => Sqrt,
            "int" => Int,
            "rand" => Rand,
            "srand" => Srand,
            "tolower" => Tolower,
            "toupper" => Toupper,
            "system" => System,
            "close" => Close,
            "fflush" => Fflush,
            _ => return None,
        })
    }
}

/// One kind of token.  This is an *enum with data*: most variants are
/// just a name (`Tok::Comma`), but some carry a value, e.g. `Tok::Number(2.5)`
/// or `Tok::Str(bytes)`.  The parser `match`es on these.
///
/// Keywords and operators each get their own variant.  `func` is not a
/// keyword: like `gawk --posix` and mawk, hawk treats it as an ordinary name.
#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // ---- values and names
    /// A numeric literal, already converted (`1e3` is 1000).
    Number(f64),
    /// A string literal with its escapes already processed.
    Str(Vec<u8>),
    /// A regex literal `/.../`: the text between the slashes, with `\/`
    /// turned into `/`; everything else is left for the regex module.
    Ere(Vec<u8>),
    /// A variable name (or a function name followed by a space).
    Name(String),
    /// A name immediately followed by `(`: a user function call.
    FuncName(String),
    Builtin(Builtin),
    // ---- layout
    /// A significant newline (statement terminator); runs collapse to one.
    Newline,
    /// End of the program text; always the last token.
    Eof,
    // ---- keywords
    Begin,
    End,
    Function,
    If,
    Else,
    While,
    For,
    Do,
    Break,
    Continue,
    Next,
    Nextfile,
    Exit,
    Return,
    Delete,
    In,
    Getline,
    Print,
    Printf,
    // ---- operators and punctuation
    AddAssign, // +=
    SubAssign, // -=
    MulAssign, // *=
    DivAssign, // /=
    ModAssign, // %=
    PowAssign, // ^= (also spelled **=)
    Eq,        // ==
    Le,        // <=
    Ge,        // >=
    Ne,        // !=
    Incr,      // ++
    Decr,      // --
    And,       // &&
    Or,        // ||
    Append,    // >>
    NoMatch,   // !~
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Semicolon,
    Comma,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret, // ^ (also spelled **)
    Not,
    Gt,
    Lt,
    Pipe,
    Question,
    Colon,
    Tilde,
    Dollar,
    Assign, // =
}

impl Tok {
    /// pyawk's name for this kind of token, used in "expected ..." messages:
    /// `NAME`, `NUMBER`, a keyword itself, an operator's spelling.
    pub fn type_name(&self) -> &'static str {
        match self {
            Tok::Number(_) => "NUMBER",
            Tok::Str(_) => "STRING",
            Tok::Ere(_) => "ERE",
            Tok::Name(_) => "NAME",
            Tok::FuncName(_) => "FUNC_NAME",
            Tok::Builtin(_) => "BUILTIN",
            Tok::Newline => "NEWLINE",
            Tok::Eof => "EOF",
            Tok::Begin => "BEGIN",
            Tok::End => "END",
            Tok::Function => "function",
            Tok::If => "if",
            Tok::Else => "else",
            Tok::While => "while",
            Tok::For => "for",
            Tok::Do => "do",
            Tok::Break => "break",
            Tok::Continue => "continue",
            Tok::Next => "next",
            Tok::Nextfile => "nextfile",
            Tok::Exit => "exit",
            Tok::Return => "return",
            Tok::Delete => "delete",
            Tok::In => "in",
            Tok::Getline => "getline",
            Tok::Print => "print",
            Tok::Printf => "printf",
            Tok::AddAssign => "+=",
            Tok::SubAssign => "-=",
            Tok::MulAssign => "*=",
            Tok::DivAssign => "/=",
            Tok::ModAssign => "%=",
            Tok::PowAssign => "^=",
            Tok::Eq => "==",
            Tok::Le => "<=",
            Tok::Ge => ">=",
            Tok::Ne => "!=",
            Tok::Incr => "++",
            Tok::Decr => "--",
            Tok::And => "&&",
            Tok::Or => "||",
            Tok::Append => ">>",
            Tok::NoMatch => "!~",
            Tok::LBrace => "{",
            Tok::RBrace => "}",
            Tok::LParen => "(",
            Tok::RParen => ")",
            Tok::LBracket => "[",
            Tok::RBracket => "]",
            Tok::Semicolon => ";",
            Tok::Comma => ",",
            Tok::Plus => "+",
            Tok::Minus => "-",
            Tok::Star => "*",
            Tok::Slash => "/",
            Tok::Percent => "%",
            Tok::Caret => "^",
            Tok::Not => "!",
            Tok::Gt => ">",
            Tok::Lt => "<",
            Tok::Pipe => "|",
            Tok::Question => "?",
            Tok::Colon => ":",
            Tok::Tilde => "~",
            Tok::Dollar => "$",
            Tok::Assign => "=",
        }
    }

    /// The token's text as an error message shows it ("found ..."):
    /// a name, a string's value, `newline`, an operator's spelling.
    pub fn shown(&self) -> Vec<u8> {
        match self {
            Tok::Number(v) => format!("{v}").into_bytes(),
            Tok::Str(s) | Tok::Ere(s) => s.clone(),
            Tok::Name(n) | Tok::FuncName(n) => n.clone().into_bytes(),
            Tok::Builtin(b) => b.name().as_bytes().to_vec(),
            Tok::Newline => b"newline".to_vec(),
            other => other.type_name().as_bytes().to_vec(),
        }
    }
}

/// A token plus the source line it started on (for error messages).
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub line: u32,
}

/// Split AWK program text into tokens, ending with `Tok::Eof`.
///
/// `Result<Vec<Token>, Error>`: either the token list or a syntax error.
pub fn tokenize(src: &[u8]) -> Result<Vec<Token>, Error> {
    let mut lx = Lexer {
        src,
        pos: 0,
        line: 1,
        tokens: Vec::new(),
    };
    lx.run()?; // `?` returns early with the error if `run` failed.
    Ok(lx.tokens)
}

/// Operators, longest first so that `**=` wins over `**` and `*`.
/// A `&[(&[u8], Tok)]` is a borrowed slice of (spelling, token) pairs; `b"..."`
/// is a byte-string literal.  `const` items are built at compile time.
const OPERATORS: &[(&[u8], Tok)] = &[
    (b"**=", Tok::PowAssign),
    (b"+=", Tok::AddAssign),
    (b"-=", Tok::SubAssign),
    (b"*=", Tok::MulAssign),
    (b"/=", Tok::DivAssign),
    (b"%=", Tok::ModAssign),
    (b"^=", Tok::PowAssign),
    (b"==", Tok::Eq),
    (b"<=", Tok::Le),
    (b">=", Tok::Ge),
    (b"!=", Tok::Ne),
    (b"++", Tok::Incr),
    (b"--", Tok::Decr),
    (b"&&", Tok::And),
    (b"||", Tok::Or),
    (b">>", Tok::Append),
    (b"!~", Tok::NoMatch),
    (b"**", Tok::Caret),
    (b"{", Tok::LBrace),
    (b"}", Tok::RBrace),
    (b"(", Tok::LParen),
    (b")", Tok::RParen),
    (b"[", Tok::LBracket),
    (b"]", Tok::RBracket),
    (b";", Tok::Semicolon),
    (b",", Tok::Comma),
    (b"+", Tok::Plus),
    (b"-", Tok::Minus),
    (b"*", Tok::Star),
    (b"/", Tok::Slash),
    (b"%", Tok::Percent),
    (b"^", Tok::Caret),
    (b"!", Tok::Not),
    (b">", Tok::Gt),
    (b"<", Tok::Lt),
    (b"|", Tok::Pipe),
    (b"?", Tok::Question),
    (b":", Tok::Colon),
    (b"~", Tok::Tilde),
    (b"$", Tok::Dollar),
    (b"=", Tok::Assign),
];

fn keyword(word: &str) -> Option<Tok> {
    Some(match word {
        "BEGIN" => Tok::Begin,
        "END" => Tok::End,
        "function" => Tok::Function,
        "if" => Tok::If,
        "else" => Tok::Else,
        "while" => Tok::While,
        "for" => Tok::For,
        "do" => Tok::Do,
        "break" => Tok::Break,
        "continue" => Tok::Continue,
        "next" => Tok::Next,
        "nextfile" => Tok::Nextfile,
        "exit" => Tok::Exit,
        "return" => Tok::Return,
        "delete" => Tok::Delete,
        "in" => Tok::In,
        "getline" => Tok::Getline,
        "print" => Tok::Print,
        "printf" => Tok::Printf,
        _ => return None,
    })
}

/// After one of these tokens a newline is not significant: the statement
/// obviously continues (`a,` / `{` / `a &&` / `do` / `else`).
fn eats_newline(t: &Tok) -> bool {
    matches!(
        t,
        Tok::Comma | Tok::LBrace | Tok::And | Tok::Or | Tok::Do | Tok::Else
    )
}

/// After one of these tokens an operand has just ended, so `/` means
/// division; anywhere else it starts a regex literal.
fn ends_operand(t: &Tok) -> bool {
    matches!(
        t,
        Tok::Name(_)
            | Tok::Number(_)
            | Tok::Str(_)
            | Tok::Ere(_)
            | Tok::Builtin(_)
            | Tok::RParen
            | Tok::RBracket
            | Tok::Incr
            | Tok::Decr
            | Tok::Dollar
    )
}

/// The lexer's working state.  `'a` is a *lifetime*: it says the lexer
/// borrows `src` and must not outlive it.  No copy of the source is made.
struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    line: u32,
    tokens: Vec<Token>,
}

// `impl` blocks hold the methods of a type.  `&mut self` means the method
// may modify the lexer.
impl Lexer<'_> {
    fn error(&self, msg: &str) -> Error {
        Error::syntax(msg, self.line)
    }

    /// The previous token, if any.  `Option<&Tok>` is either `None` or a
    /// borrowed reference to the token.
    fn prev(&self) -> Option<&Tok> {
        self.tokens.last().map(|t| &t.tok)
    }

    fn emit(&mut self, tok: Tok, line: u32) {
        self.tokens.push(Token { tok, line });
    }

    /// The byte at `i`, or `None` past the end (so no bounds panics).
    fn at(&self, i: usize) -> Option<u8> {
        self.src.get(i).copied()
    }

    fn run(&mut self) -> Result<(), Error> {
        while let Some(c) = self.at(self.pos) {
            match c {
                b' ' | b'\t' | b'\r' => self.pos += 1,
                // Backslash-newline (or backslash-CRLF): a line continuation,
                // which produces no token at all.
                b'\\' if self.at(self.pos + 1) == Some(b'\n') => {
                    self.pos += 2;
                    self.line += 1;
                }
                b'\\' if self.src[self.pos + 1..].starts_with(b"\r\n") => {
                    self.pos += 3;
                    self.line += 1;
                }
                b'#' => {
                    while self.at(self.pos).is_some_and(|c| c != b'\n') {
                        self.pos += 1;
                    }
                }
                b'\n' => {
                    // One Newline for a run of blank lines, and none at all
                    // after a token that cannot end a statement.
                    let skip = self
                        .prev()
                        .is_some_and(|t| eats_newline(t) || *t == Tok::Newline);
                    if !skip {
                        self.emit(Tok::Newline, self.line);
                    }
                    self.pos += 1;
                    self.line += 1;
                }
                b'"' => self.string()?,
                b'/' if !self.prev().is_some_and(ends_operand) => self.regex()?,
                b'0'..=b'9' => self.number(),
                b'.' if self.at(self.pos + 1).is_some_and(|d| d.is_ascii_digit()) => self.number(),
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.word(),
                _ => self.operator()?,
            }
        }
        self.emit(Tok::Eof, self.line);
        Ok(())
    }

    /// A `"..."` literal.  Escapes are collected raw, then decoded once.
    fn string(&mut self) -> Result<(), Error> {
        let start_line = self.line;
        let mut i = self.pos + 1;
        let mut raw = Vec::new();
        loop {
            let c = match self.at(i) {
                None | Some(b'\n') => return Err(self.error("unterminated string")),
                Some(c) => c,
            };
            if c == b'"' {
                break;
            }
            if c == b'\\' {
                match self.at(i + 1) {
                    // Continuation inside a string.
                    Some(b'\n') => {
                        i += 2;
                        self.line += 1;
                        continue;
                    }
                    Some(d) => {
                        raw.extend_from_slice(&[c, d]);
                        i += 2;
                        continue;
                    }
                    None => {}
                }
            }
            raw.push(c);
            i += 1;
        }
        self.pos = i + 1;
        self.emit(Tok::Str(process_escapes(&raw)), start_line);
        Ok(())
    }

    /// A `/.../` literal.  The text is kept as-is for the regex module,
    /// except that `\/` becomes `/`.  A `/` inside a bracket expression such
    /// as `[/]` does not end the regex, so we track whether we are in one.
    fn regex(&mut self) -> Result<(), Error> {
        // The token belongs to the line the regex starts on, even if a
        // backslash-newline continues it onto the next.
        let start_line = self.line;
        let src = self.src;
        let mut i = self.pos + 1;
        let mut buf = Vec::new();
        let mut in_bracket = false;
        // A regex cannot span lines, so a class's closing `:]` must be found
        // before the end of this line.
        // (A closure, as a backslash-newline continues the regex on the
        // next line and the end has to be found again.)
        let find_line_end = |from: usize| {
            src[from.min(src.len())..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(src.len(), |n| from + n)
        };
        let mut line_end = find_line_end(i);
        // Where the next `:]`, `.]` and `=]` are, found once and reused:
        // searching afresh for every `[:` would take time proportional to
        // the square of the line length on input such as `[[:[:[:[:...`.
        // `None` means not searched yet; `Some(None)` means there is none.
        let mut next_close: [Option<Option<usize>>; 3] = [None; 3];
        loop {
            let c = match self.at(i) {
                None | Some(b'\n') => return Err(self.error("unterminated regular expression")),
                Some(c) => c,
            };
            if c == b'\\'
                && let Some(d) = self.at(i + 1)
            {
                if d == b'/' {
                    buf.push(b'/');
                } else if d == b'\n' {
                    buf.extend_from_slice(&[c, d]);
                    self.line += 1;
                    line_end = find_line_end(i + 2);
                    next_close = [None; 3];
                } else {
                    buf.extend_from_slice(&[c, d]);
                }
                i += 2;
                continue;
            }
            if in_bracket {
                if c == b']' {
                    in_bracket = false;
                } else if c == b'['
                    && let Some(kind @ (b':' | b'.' | b'=')) = self.at(i + 1)
                {
                    // Copy a class such as [:alpha:] whole, so its ']' does
                    // not look like the end of the bracket expression.
                    // `kind @ pattern` binds the matched byte to `kind`.
                    let slot = &mut next_close[match kind {
                        b':' => 0,
                        b'.' => 1,
                        _ => 2,
                    }];
                    // Search again only if the remembered one is behind us.
                    let stale = match *slot {
                        None => true,
                        Some(Some(at)) => at < i + 2,
                        Some(None) => false,
                    };
                    if stale {
                        let from = (i + 2).min(line_end);
                        *slot = Some(find(&src[from..line_end], &[kind, b']']).map(|n| from + n));
                    }
                    if let Some(Some(at)) = *slot {
                        let end = at + 2;
                        buf.extend_from_slice(&src[i..end]);
                        i = end;
                        continue;
                    }
                }
            } else if c == b'[' {
                in_bracket = true;
                buf.push(c);
                i += 1;
                // A ']' right after '[' or '[^' is a literal member.
                if self.at(i) == Some(b'^') {
                    buf.push(b'^');
                    i += 1;
                }
                if self.at(i) == Some(b']') {
                    buf.push(b']');
                    i += 1;
                }
                continue;
            } else if c == b'/' {
                break;
            }
            buf.push(c);
            i += 1;
        }
        self.pos = i + 1;
        self.emit(Tok::Ere(buf), start_line);
        Ok(())
    }

    /// 12, 1.5, .5, 1e3, 2E-4.  `1e` is the number 1 then the name e.
    fn number(&mut self) {
        let digits = |lx: &Self, mut i: usize| {
            while lx.at(i).is_some_and(|c| c.is_ascii_digit()) {
                i += 1;
            }
            i
        };
        let mut i = digits(self, self.pos);
        if self.at(i) == Some(b'.') {
            i = digits(self, i + 1);
        }
        if matches!(self.at(i), Some(b'e' | b'E')) {
            let mut j = i + 1;
            if matches!(self.at(j), Some(b'+' | b'-')) {
                j += 1;
            }
            if self.at(j).is_some_and(|c| c.is_ascii_digit()) {
                i = digits(self, j);
            }
        }
        // The slice is all ASCII, so the UTF-8 conversion cannot fail, and
        // Rust's float parser accepts every shape matched above (`1.`, `.5`,
        // `1.e2`) and gives `inf` for huge exponents.
        let text = std::str::from_utf8(&self.src[self.pos..i]).expect("ASCII");
        let value: f64 = text.parse().expect("valid float");
        self.pos = i;
        self.emit(Tok::Number(value), self.line);
    }

    /// A name, classified as keyword, builtin, function call or variable.
    fn word(&mut self) {
        let start = self.pos;
        while self
            .at(self.pos)
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            self.pos += 1;
        }
        // ASCII only, so this conversion cannot fail.
        let word = String::from_utf8(self.src[start..self.pos].to_vec()).expect("ASCII");
        let tok = if let Some(k) = keyword(&word) {
            k
        } else if let Some(b) = Builtin::from_name(&word) {
            Tok::Builtin(b)
        } else if self.at(self.pos) == Some(b'(') {
            // AWK tells `f(x)` (a call: no space allowed) from `f (x)`
            // (variable f concatenated with x).
            Tok::FuncName(word)
        } else {
            Tok::Name(word)
        };
        self.emit(tok, self.line);
    }

    fn operator(&mut self) -> Result<(), Error> {
        let rest = &self.src[self.pos..];
        // `.iter().find(...)` returns the first matching entry, if any.
        match OPERATORS.iter().find(|(op, _)| rest.starts_with(op)) {
            Some((op, tok)) => {
                self.pos += op.len();
                self.emit(tok.clone(), self.line);
                Ok(())
            }
            None => {
                // `escape_ascii` shows a non-printable byte as `\xNN`.
                let shown = rest[0].escape_ascii();
                Err(self.error(&format!("unexpected character '{shown}'")))
            }
        }
    }
}

/// Position of `needle` within `hay`, if present.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {

    /// Just the token kinds, which is what most tests compare.
    fn toks(src: &str) -> Vec<Tok> {
        // `.unwrap()` takes the Ok value and panics (fails the test) on Err.
        tokenize(src.as_bytes())
            .unwrap()
            .into_iter()
            .map(|t| t.tok)
            .collect()
    }

    fn name(s: &str) -> Tok {
        Tok::Name(s.to_string())
    }

    fn num(n: f64) -> Tok {
        Tok::Number(n)
    }

    fn ere(s: &str) -> Tok {
        Tok::Ere(s.as_bytes().to_vec())
    }

    // `use Tok::*` would clash with `Builtin`'s names, so tests spell out `Tok::`.

    #[test]
    fn regex_vs_division() {
        assert_eq!(
            toks("a / b / c"),
            [
                name("a"),
                Tok::Slash,
                name("b"),
                Tok::Slash,
                name("c"),
                Tok::Eof
            ]
        );
        assert_eq!(toks("/b/"), [ere("b"), Tok::Eof]);
        assert_eq!(
            toks(r"x ~ /a\/b/"),
            [name("x"), Tok::Tilde, ere("a/b"), Tok::Eof]
        );
        assert_eq!(
            toks("(1) / 2"),
            [
                Tok::LParen,
                num(1.0),
                Tok::RParen,
                Tok::Slash,
                num(2.0),
                Tok::Eof
            ]
        );
        assert_eq!(toks("a[1] / 2")[4], Tok::Slash);
        assert_eq!(toks("NF / 2"), [name("NF"), Tok::Slash, num(2.0), Tok::Eof]);
        assert_eq!(
            toks("length / 2"),
            [
                Tok::Builtin(Builtin::Length),
                Tok::Slash,
                num(2.0),
                Tok::Eof
            ]
        );
        assert_eq!(
            toks("x++ / 2"),
            [name("x"), Tok::Incr, Tok::Slash, num(2.0), Tok::Eof]
        );
        assert_eq!(toks(r#""s" / 2"#)[1], Tok::Slash);
        assert_eq!(toks("/a/ / 2")[1], Tok::Slash);
        assert_eq!(toks("print /x/"), [Tok::Print, ere("x"), Tok::Eof]);
        assert_eq!(
            toks("a /= 2"),
            [name("a"), Tok::DivAssign, num(2.0), Tok::Eof]
        );
        assert_eq!(
            toks("$0 ~ /=/"),
            [Tok::Dollar, num(0.0), Tok::Tilde, ere("="), Tok::Eof]
        );
        assert_eq!(toks("a && /x/")[2], ere("x"));
        assert_eq!(toks("(/x/)")[1], ere("x"));
    }

    #[test]
    fn regex_literal_keeps_its_escapes() {
        // Only "\/" is rewritten; the regex module handles the rest.
        assert_eq!(toks(r"/a\.b\\c\n/")[0], ere(r"a\.b\\c\n"));
    }

    #[test]
    fn slash_inside_brackets_does_not_end_the_regex() {
        assert_eq!(toks("/[/]x/")[0], ere("[/]x"));
        // A ']' right after '[' or '[^' is a literal member.
        assert_eq!(toks("/[]/]x/")[0], ere("[]/]x"));
        assert_eq!(toks("/[^]/]x/")[0], ere("[^]/]x"));
        // A class's own ']' does not close the bracket expression.
        assert_eq!(toks("/[[:alpha:]/]/")[0], ere("[[:alpha:]/]"));
        assert_eq!(
            toks("/[a]/ /b/"),
            [ere("[a]"), Tok::Slash, name("b"), Tok::Slash, Tok::Eof]
        );
    }

    #[test]
    fn newline_handling() {
        assert_eq!(toks("a,\nb"), [name("a"), Tok::Comma, name("b"), Tok::Eof]);
        assert_eq!(
            toks("a &&\n\nb"),
            [name("a"), Tok::And, name("b"), Tok::Eof]
        );
        assert_eq!(toks("a ||\nb")[2], name("b"));
        assert_eq!(
            toks("{\n x }"),
            [Tok::LBrace, name("x"), Tok::RBrace, Tok::Eof]
        );
        assert_eq!(toks("do\nx")[1], name("x"));
        assert_eq!(toks("else\nx")[1], name("x"));
        assert_eq!(
            toks("a\n\n\nb"),
            [name("a"), Tok::Newline, name("b"), Tok::Eof]
        );
        assert_eq!(
            toks("a \\\n + b"),
            [name("a"), Tok::Plus, name("b"), Tok::Eof]
        );
        assert_eq!(
            toks("a \\\r\n + b"),
            [name("a"), Tok::Plus, name("b"), Tok::Eof]
        );
        assert_eq!(
            toks("a # comment\nb"),
            [name("a"), Tok::Newline, name("b"), Tok::Eof]
        );
        assert_eq!(
            toks("a;\nb"),
            [name("a"), Tok::Semicolon, Tok::Newline, name("b"), Tok::Eof]
        );
        assert_eq!(
            toks("a\r\nb"),
            [name("a"), Tok::Newline, name("b"), Tok::Eof]
        );
        // A leading newline is kept (the parser skips it).
        assert_eq!(toks("\n\nx"), [Tok::Newline, name("x"), Tok::Eof]);
    }

    #[test]
    fn line_numbers() {
        let t = tokenize(b"a\n\nb \\\n c\n\"s\\\nt\" d").unwrap();
        let lines: Vec<u32> = t.iter().map(|t| t.line).collect();
        // a NL b c NL "st" d EOF
        assert_eq!(lines, [1, 1, 3, 4, 4, 5, 6, 6]);
        // A regex continued with backslash-newline counts that line too;
        // the regex itself belongs to the line it starts on.
        let t = tokenize(b"a\n/x\\\ny/ d").unwrap();
        let lines: Vec<u32> = t.iter().map(|t| t.line).collect();
        // a NL /x\ny/ d EOF
        assert_eq!(lines, [1, 1, 2, 3, 3]);
        let e = tokenize(b"\n/x\\\ny").unwrap_err().to_string();
        assert!(e.starts_with("syntax error at source line 3"), "{e}");
    }

    #[test]
    fn numbers() {
        assert_eq!(
            toks("1 2.5 .5 1e3 1E-2 1.e2 12e+1 007"),
            [
                num(1.0),
                num(2.5),
                num(0.5),
                num(1000.0),
                num(0.01),
                num(100.0),
                num(120.0),
                num(7.0),
                Tok::Eof
            ]
        );
        // "1e" is the number 1 followed by the name e.
        assert_eq!(toks("1e"), [num(1.0), name("e"), Tok::Eof]);
        assert_eq!(toks("1e+"), [num(1.0), name("e"), Tok::Plus, Tok::Eof]);
        // "1.5.3" is 1.5 then .3 (concatenation, as in the references).
        assert_eq!(toks("1.5.3"), [num(1.5), num(0.3), Tok::Eof]);
        // No hex in POSIX awk: 0x1A is 0 then the name x1A.
        assert_eq!(toks("0x1A"), [num(0.0), name("x1A"), Tok::Eof]);
        assert_eq!(toks("1e999"), [num(f64::INFINITY), Tok::Eof]);
    }

    #[test]
    fn strings() {
        let t = toks(r#""a\tb\"c\101" """#);
        assert_eq!(
            t,
            [
                Tok::Str(b"a\tb\"cA".to_vec()),
                Tok::Str(Vec::new()),
                Tok::Eof
            ]
        );
        // Backslash-newline inside a string continues it.
        assert_eq!(toks("\"ab\\\ncd\"")[0], Tok::Str(b"abcd".to_vec()));
        // Non-UTF-8 bytes pass through.
        assert_eq!(
            tokenize(b"\"\xff\xfe\"").unwrap()[0].tok,
            Tok::Str(vec![0xff, 0xfe])
        );
        // An escaped quote does not end the string, an escaped backslash does not escape it.
        assert_eq!(
            toks(r#""\\" x"#),
            [Tok::Str(b"\\".to_vec()), name("x"), Tok::Eof]
        );
    }

    #[test]
    fn keywords_builtins_funcnames() {
        assert_eq!(
            toks("if while getline length substr foo foo( func"),
            [
                Tok::If,
                Tok::While,
                Tok::Getline,
                Tok::Builtin(Builtin::Length),
                Tok::Builtin(Builtin::Substr),
                name("foo"),
                Tok::FuncName("foo".into()),
                Tok::LParen,
                name("func"),
                Tok::Eof
            ]
        );
        assert_eq!(
            toks(
                "BEGIN END function else for do break continue next nextfile exit return delete in print printf"
            ),
            [
                Tok::Begin,
                Tok::End,
                Tok::Function,
                Tok::Else,
                Tok::For,
                Tok::Do,
                Tok::Break,
                Tok::Continue,
                Tok::Next,
                Tok::Nextfile,
                Tok::Exit,
                Tok::Return,
                Tok::Delete,
                Tok::In,
                Tok::Print,
                Tok::Printf,
                Tok::Eof
            ]
        );
        // A builtin keeps its token even when followed by "(".
        assert_eq!(toks("substr(")[0], Tok::Builtin(Builtin::Substr));
        // Names: letters, digits, underscores; case matters.
        assert_eq!(toks("_a1 Begin"), [name("_a1"), name("Begin"), Tok::Eof]);
    }

    #[test]
    fn operators_longest_match() {
        assert_eq!(
            toks("a**=2"),
            [name("a"), Tok::PowAssign, num(2.0), Tok::Eof]
        );
        assert_eq!(toks("a**2"), [name("a"), Tok::Caret, num(2.0), Tok::Eof]);
        assert_eq!(
            toks("a>>b!~c"),
            [
                name("a"),
                Tok::Append,
                name("b"),
                Tok::NoMatch,
                name("c"),
                Tok::Eof
            ]
        );
        assert_eq!(
            toks(
                "+= -= *= %= ^= == <= >= != ++ -- && || { } ( ) [ ] ; , + - * % ^ ! > < | ? : ~ $ ="
            ),
            [
                Tok::AddAssign,
                Tok::SubAssign,
                Tok::MulAssign,
                Tok::ModAssign,
                Tok::PowAssign,
                Tok::Eq,
                Tok::Le,
                Tok::Ge,
                Tok::Ne,
                Tok::Incr,
                Tok::Decr,
                Tok::And,
                Tok::Or,
                Tok::LBrace,
                Tok::RBrace,
                Tok::LParen,
                Tok::RParen,
                Tok::LBracket,
                Tok::RBracket,
                Tok::Semicolon,
                Tok::Comma,
                Tok::Plus,
                Tok::Minus,
                Tok::Star,
                Tok::Percent,
                Tok::Caret,
                Tok::Not,
                Tok::Gt,
                Tok::Lt,
                Tok::Pipe,
                Tok::Question,
                Tok::Colon,
                Tok::Tilde,
                Tok::Dollar,
                Tok::Assign,
                Tok::Eof
            ]
        );
        assert_eq!(
            toks("a+++b"),
            [name("a"), Tok::Incr, Tok::Plus, name("b"), Tok::Eof]
        );
    }

    #[test]
    fn errors() {
        let err = |src: &[u8]| tokenize(src).unwrap_err().to_string();
        assert_eq!(
            err(b"\"unterminated"),
            "syntax error at source line 1: unterminated string"
        );
        assert_eq!(
            err(b"x\n\"a\nb\""),
            "syntax error at source line 2: unterminated string"
        );
        assert_eq!(
            err(b"/unterminated"),
            "syntax error at source line 1: unterminated regular expression"
        );
        assert_eq!(
            err(b"/a\n/"),
            "syntax error at source line 1: unterminated regular expression"
        );
        // A character class such as [:alpha:] must close on its own line;
        // the regex may not run on to a `:]` further down the program.
        assert_eq!(
            err(b"/[[:a/\n:]]/"),
            "syntax error at source line 1: unterminated regular expression"
        );
        // ... but a backslash-newline continues the regex on the next line.
        assert_eq!(
            toks("/a\\\n[[:alpha:]]/"),
            [ere("a\\\n[[:alpha:]]"), Tok::Eof]
        );
        assert_eq!(
            err(b"a @ b"),
            "syntax error at source line 1: unexpected character '@'"
        );
        assert_eq!(
            err(b"a \xc3\xa9"),
            "syntax error at source line 1: unexpected character '\\xc3'"
        );
    }
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
