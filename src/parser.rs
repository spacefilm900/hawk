//! Recursive-descent parser: tokens in, [`Program`] out.
//!
//! A faithful port of pyawk v1.0's `parser.py`, one method per grammar rule.
//! For expressions there is one method per precedence level; each calls the
//! next tighter level for its operands, so `1 + 2 * 3` groups as
//! `1 + (2 * 3)`.  Precedence, lowest to highest (POSIX):
//!
//! ```text
//! assignment  = += -= *= /= %= ^=      (right)
//! ?:                                   (right)
//! ||                                   (left)
//! &&                                   (left)
//! in                                   (left)
//! ~ !~                                 (left)
//! < <= != == > >=   and  expr | getline (non-associative)
//! concatenation                        (left)
//! + -                                  (left)
//! * / %                                (left)
//! unary + - !
//! ^                                    (right)
//! ++ -- (pre/post)
//! $
//! grouping ()
//! ```
//!
//! Three quirks worth knowing, all as in pyawk and the reference awks:
//!
//! * Inside `print`/`printf` arguments an unparenthesised `>` is an output
//!   redirection, not a comparison.  The flag `no_gt` tracks this; it is
//!   switched off again inside `(...)`, `[...]` and call arguments.
//! * An assignment may follow `|| && ~ !~` and the comparisons without
//!   parentheses (`x && $0 = $1`), but not arithmetic (`1 + x = 3` is an
//!   error).  See [`Parser::maybe_assign`].
//! * A function may be called before it is defined, so some checks wait
//!   until the whole program is read ([`Parser::check_functions`]).
//!
//! Rust notes: every parse method returns `Result<T, Error>` and uses `?`
//! to pass a syntax error straight up to the caller, which plays the role
//! of Python's `raise`.  pyawk's `try/finally` that restores `no_gt`
//! becomes "save, call, restore, then `?`" (see [`Parser::with_no_gt`]).

use crate::ast::{
    AssignOp, BinOp, CmpOp, Expr, Function, GetlineKind, Program, Redirect, Rule, Stmt, UnOp,
};
use crate::error::Error;
use crate::interp::stack_position;
use crate::lexer::{Builtin, Tok, Token, tokenize};

/// The special variables.  POSIX forbids using one as a function name or a
/// function parameter name (gawk enforces this for all of them; mawk and
/// onetrue-awk for some), so hawk, like pyawk, rejects them all.
const SPECIAL_VARS: &[&str] = &[
    "ARGC", "ARGV", "CONVFMT", "ENVIRON", "FILENAME", "FNR", "FS", "NF", "NR", "OFMT", "OFS",
    "ORS", "RLENGTH", "RS", "RSTART", "SUBSEP",
];

/// Parse a whole program.  The entry point of this module.
pub fn parse(source: &[u8]) -> Result<Program, Error> {
    parse_with_stack(source, 0)
}

/// `parse`, on a thread whose stack is `stack_size` bytes (0: unknown, so
/// assume 2 MiB, the smallest stack a thread usually has: Rust gives the
/// threads it spawns 2 MiB, and the main thread usually has 8 MiB; on a
/// thread with less stack left than that, pass the real size).  The
/// parser uses at most half of it, leaving the rest for the interpreter's
/// own recursion over the deepest expressions.
pub fn parse_with_stack(source: &[u8], stack_size: usize) -> Result<Program, Error> {
    let tokens = tokenize(source)?;
    let mut parser = Parser::new(tokens);
    parser.stack_budget = match stack_size {
        0 => 2 << 20,
        n => n,
    } / 2;
    parser.parse_program()
}

/// Can this token begin an operand of implicit concatenation?  `+ - !`
/// are deliberately excluded: `a -1` is subtraction.
fn starts_concat(t: &Tok) -> bool {
    matches!(
        t,
        Tok::Number(_)
            | Tok::Str(_)
            | Tok::Ere(_)
            | Tok::Name(_)
            | Tok::FuncName(_)
            | Tok::Builtin(_)
            | Tok::Dollar
            | Tok::LParen
            | Tok::Incr
            | Tok::Decr
    )
}

/// Tokens that end a simple statement without being consumed.
fn ends_statement(t: &Tok) -> bool {
    matches!(t, Tok::Semicolon | Tok::Newline | Tok::RBrace | Tok::Eof)
}

fn assign_op(t: &Tok) -> Option<AssignOp> {
    Some(AssignOp(match t {
        Tok::Assign => None,
        Tok::AddAssign => Some(BinOp::Add),
        Tok::SubAssign => Some(BinOp::Sub),
        Tok::MulAssign => Some(BinOp::Mul),
        Tok::DivAssign => Some(BinOp::Div),
        Tok::ModAssign => Some(BinOp::Mod),
        Tok::PowAssign => Some(BinOp::Pow),
        _ => return None,
    }))
}

fn compare_op(t: &Tok) -> Option<CmpOp> {
    Some(match t {
        Tok::Lt => CmpOp::Lt,
        Tok::Le => CmpOp::Le,
        Tok::Eq => CmpOp::Eq,
        Tok::Ne => CmpOp::Ne,
        Tok::Gt => CmpOp::Gt,
        Tok::Ge => CmpOp::Ge,
        _ => return None,
    })
}

fn unary_op(t: &Tok) -> Option<UnOp> {
    Some(match t {
        Tok::Minus => UnOp::Neg,
        Tok::Plus => UnOp::Plus,
        Tok::Not => UnOp::Not,
        _ => return None,
    })
}

/// Python's `repr()` of a string, which pyawk's messages use to show a
/// token: single quotes unless the text contains `'` and no `"`.
pub fn py_repr(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Wrap a value in a `Box` (shorter to read in the tree-building code).
fn bx(e: Expr) -> Box<Expr> {
    Box::new(e)
}

/// The parser's state: the tokens and a cursor into them, plus context.
///
/// Helper vocabulary: `at(t)` asks "is the current token t?", `accept(t)`
/// consumes it only if so, `expect(t)` consumes it or fails, `advance()`
/// consumes unconditionally.
struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// True inside print/printf arguments, where `>` means redirection.
    no_gt: bool,
    /// The function whose body we are in (`return` is legal only there).
    in_function: Option<String>,
    /// How many loops enclose us (`break`/`continue` need one).
    loop_depth: u32,
    /// "BEGIN" or "END" while parsing that action (`next` and `nextfile`
    /// are syntax errors there).  `&'static str` is a string constant
    /// built into the program, so storing one costs nothing.
    in_special: Option<&'static str>,
    /// Every name used as a variable or array, with its line.
    var_uses: Vec<(String, u32)>,
    /// Every user function call: name, number of arguments, line.
    calls: Vec<(String, usize, u32)>,
    /// How deeply the recursive parsing functions are nested (see `nested`).
    depth: usize,
    /// How many of those levels are chain links ([`Parser::link`]).
    links: usize,
    /// Where the stack was when parsing began, and how much of it the
    /// parser may use (see `nested`).
    stack_base: usize,
    stack_budget: usize,
    /// The token index of the `(` that opens a print/printf argument list,
    /// as in `print (a, b) > "file"` (see `parse_group`).
    print_group_at: Option<usize>,
    /// True from when a parenthesised list followed by `in` is parsed until
    /// that `in` takes it as its left operand (see `parse_group`).
    group_awaits_in: bool,
}

/// The most `nested` levels allowed, whatever the stack size.  Each
/// parenthesis costs four (expression, conditional, unary, postfix), so
/// this is 5,000 nested parentheses and a little room for the statement
/// around them, as many as gawk accepts; that keeps the tree, and the
/// interpreter's recursion over it, within reason.
const MAX_NESTING: usize = 20_100;

/// Stack set aside per chain link ([`Parser::link`]).  A link costs the
/// parser nothing, but running or freeing the tree later recurses once per
/// link; that takes about 100 bytes a level when freeing, more in the
/// interpreter, and more again in a debug build.  So on a 2 MiB thread
/// (a 1 MiB budget) chains are limited to 2,048 links, while hawk's own
/// 1 GiB thread allows the full `MAX_NESTING`.
const LINK_COST: usize = 512;

impl Parser {
    fn new(tokens: Vec<Token>) -> Parser {
        Parser {
            tokens,
            pos: 0,
            no_gt: false,
            in_function: None,
            loop_depth: 0,
            in_special: None,
            var_uses: Vec::new(),
            calls: Vec::new(),
            depth: 0,
            links: 0,
            stack_base: stack_position(),
            stack_budget: 0,
            print_group_at: None,
            group_awaits_in: false,
        }
    }

    /// Run `parse` one level deeper.  Every recursive parsing function goes
    /// through here, so absurdly nested input (a million parentheses, say)
    /// is a syntax error instead of a stack overflow, which would kill the
    /// process.  Both the depth and the stack actually used are checked:
    /// how much stack a level takes depends on the compiler and the build.
    /// `impl FnOnce(&mut Self) -> ...` accepts any function or closure
    /// taking the parser.
    fn nested<T>(&mut self, parse: impl FnOnce(&mut Self) -> Result<T, Error>) -> Result<T, Error> {
        // `wrapping_sub` because the subtraction must not panic even if
        // the stack were laid out unexpectedly; the check then just fails.
        let used = self.stack_base.wrapping_sub(stack_position());
        if self.too_deep(used) {
            return Err(self.error("nesting too deep"));
        }
        let (depth, links) = (self.depth, self.links);
        self.depth += 1;
        let result = parse(self);
        // Give this level back, but not the chain links ([`Parser::link`])
        // made inside it: they are part of the tree `parse` returned, which
        // may yet start a longer chain, as `(1 + 2)` does in `(1 + 2) + 3`.
        // So they keep counting until the whole tree is done ([`Parser::tree`]).
        // (Links only ever grow inside `parse`, so this cannot underflow.)
        self.depth = depth + (self.links - links);
        result
    }

    /// Parse one whole tree: a statement or a pattern.  Once it is done its
    /// chain links no longer add to the depth of anything parsed after it,
    /// so the depth and link counts go back to what they were.
    fn tree<T>(&mut self, parse: impl FnOnce(&mut Self) -> Result<T, Error>) -> Result<T, Error> {
        let (depth, links) = (self.depth, self.links);
        let result = parse(self);
        (self.depth, self.links) = (depth, links);
        result
    }

    /// One more link in a left-nested chain such as `1 + 2 + 3` (that is
    /// `(1 + 2) + 3`), which the operator loops build without recursing.
    /// The tree is still as deep as the chain is long, and the interpreter,
    /// like the code that frees the tree, recurses over that depth, so each
    /// link counts towards the nesting limit until the statement or pattern
    /// it is in is finished (see [`Parser::nested`] and [`Parser::tree`]).
    /// Separate chains in one statement, as in `f(1 + 2, 3 + 4)`, are
    /// counted together: more than needed, but safe and simple.
    fn link(&mut self) -> Result<(), Error> {
        let used = self.stack_base.wrapping_sub(stack_position());
        if self.too_deep(used) {
            return Err(self.error("nesting too deep"));
        }
        self.depth += 1;
        self.links += 1;
        Ok(())
    }

    /// Is one more level too many, given `used` bytes of stack so far?
    /// The stack the chain links will need later counts as used.
    fn too_deep(&self, used: usize) -> bool {
        let reserved = self.links.saturating_mul(LINK_COST);
        self.depth >= MAX_NESTING || used.saturating_add(reserved) > self.stack_budget
    }

    // ------------------------------------------------------------ utilities

    /// The current token.  (The lexer always ends the list with `Eof`, and
    /// `advance` never moves past it, so this index is always valid.)
    fn tok(&self) -> &Tok {
        &self.tokens[self.pos].tok
    }

    fn line(&self) -> u32 {
        self.tokens[self.pos].line
    }

    /// The token `offset` places ahead (the final `Eof` if past the end).
    fn peek(&self, offset: usize) -> &Tok {
        let i = (self.pos + offset).min(self.tokens.len() - 1);
        &self.tokens[i].tok
    }

    /// Consume the current token and return a copy of it.
    fn advance(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        if t.tok != Tok::Eof {
            self.pos += 1;
        }
        t
    }

    /// Is the current token `t`?  For tokens that carry data (names,
    /// numbers) compare with [`Parser::at_kind`] instead.
    fn at(&self, t: &Tok) -> bool {
        self.tok() == t
    }

    /// Is the current token of the same *kind* as `t`, ignoring its data?
    /// `std::mem::discriminant` gives an enum value's variant tag.
    fn at_kind(&self, t: &Tok) -> bool {
        std::mem::discriminant(self.tok()) == std::mem::discriminant(t)
    }

    fn accept(&mut self, t: &Tok) -> bool {
        if self.at(t) {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Consume a token of `t`'s kind, or fail with "expected ...".
    fn expect(&mut self, t: &Tok) -> Result<Token, Error> {
        if !self.at_kind(t) {
            return Err(self.error(&format!("expected {}", py_repr(t.type_name().as_bytes()))));
        }
        Ok(self.advance())
    }

    /// A syntax error at the current token: "msg, found 'x'".
    fn error(&self, msg: &str) -> Error {
        let shown = match self.tok() {
            Tok::Eof => "end of input".to_string(),
            t => py_repr(&t.shown()),
        };
        Error::syntax(format!("{msg}, found {shown}"), self.line())
    }

    fn skip_newlines(&mut self) {
        while self.at(&Tok::Newline) {
            self.pos += 1;
        }
    }

    fn skip_terminators(&mut self) {
        while matches!(self.tok(), Tok::Newline | Tok::Semicolon) {
            self.pos += 1;
        }
    }

    /// Run `f` with `no_gt` set to `value`, then restore the old setting,
    /// whether `f` succeeded or not.  `FnOnce(&mut Self) -> T` accepts any
    /// closure that takes the parser; `T` is whatever the closure returns.
    fn with_no_gt<T>(&mut self, value: bool, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = self.no_gt;
        self.no_gt = value;
        let result = f(self);
        self.no_gt = saved;
        result
    }

    /// Note that a NAME token is used as a variable or array (for
    /// `check_functions`) and return the name.
    fn name_use(&mut self, t: Token) -> String {
        // `let ... else` destructures or runs the else branch, which must
        // leave the function; callers only pass NAME tokens.
        let Tok::Name(name) = t.tok else {
            unreachable!("name_use called on {:?}", t.tok)
        };
        self.var_uses.push((name.clone(), t.line));
        name
    }

    /// Consume a NAME token, record the use, and return the name.
    fn expect_name_use(&mut self) -> Result<String, Error> {
        let t = self.expect(&Tok::Name(String::new()))?;
        Ok(self.name_use(t))
    }

    fn check_no_group(&self, e: &Expr) -> Result<(), Error> {
        if matches!(e, Expr::Group(_)) {
            return Err(self.group_error());
        }
        Ok(())
    }

    fn group_error(&self) -> Error {
        Error::syntax(
            "parenthesised list is only valid before 'in' or after print",
            self.line(),
        )
    }

    /// A parenthesised list `(a, b)` that opened at token `start` has just
    /// been read.  It is valid only directly before `in` (`(i, j) in arr`)
    /// or as the whole argument list of print/printf (`print (a, b) >
    /// "f"`); anywhere else (`1 + (a, b)`, `print ((a, b))`) it is a syntax
    /// error now, as in gawk, mawk and onetrue-awk.
    fn parse_group(&mut self, start: usize, exprs: Vec<Expr>) -> Result<Expr, Error> {
        if self.at(&Tok::In) {
            // `parse_in` checks that the `in` really gets the list itself,
            // not `-(a, b)` or `1 + (a, b)`.
            self.group_awaits_in = true;
        } else {
            let ends_print = matches!(
                self.tok(),
                Tok::Semicolon
                    | Tok::Newline
                    | Tok::RBrace
                    | Tok::Eof
                    | Tok::Gt
                    | Tok::Append
                    | Tok::Pipe
            );
            if !(ends_print && self.print_group_at == Some(start)) {
                return Err(self.group_error());
            }
        }
        Ok(Expr::Group(exprs))
    }

    // -------------------------------------------------------------- program

    fn parse_program(mut self) -> Result<Program, Error> {
        let mut prog = Program::default();
        self.skip_terminators();
        while !self.at(&Tok::Eof) {
            self.parse_item(&mut prog)?;
            self.skip_terminators();
        }
        self.check_functions(&mut prog)?;
        Ok(prog)
    }

    /// Checks that need the whole program, because a function may be used
    /// before it is defined:
    ///
    /// * a function name may not also be used as a variable or array (this
    ///   also catches a call written with a space: `f (x)`);
    /// * a parameter may not have the name of any function;
    /// * passing more arguments than a function has parameters is allowed
    ///   with a warning (as onetrue-awk and gawk do; mawk refuses).
    ///
    /// Calling a function that is never defined is not an error here: like
    /// onetrue-awk and gawk, it fails only if the call actually runs.
    fn check_functions(&self, prog: &mut Program) -> Result<(), Error> {
        for (name, line) in &self.var_uses {
            if prog.function(name).is_some() {
                return Err(Error::syntax(
                    format!(
                        "function {name} called with a space before '(' or used as a variable or array"
                    ),
                    *line,
                ));
            }
        }
        for f in &prog.functions {
            for p in &f.params {
                if prog.function(p).is_some() {
                    return Err(Error::syntax_no_line(format!(
                        "function {}: parameter {p} has the name of a function",
                        f.name
                    )));
                }
            }
        }
        for (name, nargs, line) in &self.calls {
            if let Some(f) = prog.function(name)
                && *nargs > f.params.len()
            {
                let msg = format!(
                    "line {line}: function {name} called with {nargs} args, accepts only {}",
                    f.params.len()
                );
                prog.warnings.push(msg);
            }
        }
        Ok(())
    }

    /// One top-level item: `BEGIN {...}`, `END {...}`, a function, or
    /// `pattern`, `pattern {...}`, `start, end [{...}]`, `{...}`.
    fn parse_item(&mut self, prog: &mut Program) -> Result<(), Error> {
        match self.tok() {
            Tok::Begin | Tok::End => {
                let is_begin = self.at(&Tok::Begin);
                self.advance();
                self.skip_newlines();
                self.in_special = Some(if is_begin { "BEGIN" } else { "END" });
                // Reset the flag before `?` can return an error early.
                let block = self.parse_block();
                self.in_special = None;
                let block = block?;
                if is_begin {
                    prog.begin.push(block);
                } else {
                    prog.end.push(block);
                }
            }
            Tok::Function => self.parse_function(prog)?,
            Tok::Name(n) if n == "func" && matches!(self.peek(1), Tok::FuncName(_)) => {
                // `func name(...)` would otherwise parse as a pattern and
                // fail confusingly later.
                return Err(Error::syntax(
                    "'func' is not POSIX awk; write 'function' to define a function",
                    self.line(),
                ));
            }
            Tok::LBrace => {
                let action = self.parse_block()?;
                prog.rules.push(Rule {
                    pattern: None,
                    action: Some(action),
                    end: None,
                });
            }
            _ => {
                let pattern = self.parse_pattern()?;
                // `start, end`: a range pattern.  The comma is only
                // recognised here, after a complete expression, so it binds
                // more loosely than everything and a range can never appear
                // inside an expression.  Newlines may follow it (not precede it).
                let mut end = None;
                if self.accept(&Tok::Comma) {
                    self.skip_newlines();
                    if matches!(self.tok(), Tok::Begin | Tok::End | Tok::LBrace) {
                        return Err(
                            self.error("expected an expression after ',' in a range pattern")
                        );
                    }
                    end = Some(self.parse_pattern()?);
                }
                let action = if self.at(&Tok::LBrace) {
                    Some(self.parse_block()?)
                } else if matches!(self.tok(), Tok::Newline | Tok::Semicolon | Tok::Eof) {
                    None
                } else {
                    return Err(self.error("expected newline or '{' after pattern"));
                };
                prog.rules.push(Rule {
                    pattern: Some(pattern),
                    action,
                    end,
                });
            }
        }
        Ok(())
    }

    /// One pattern expression; a parenthesised list `(/a/, /b/)` is not one.
    fn parse_pattern(&mut self) -> Result<Expr, Error> {
        let e = self.tree(Self::parse_expr)?;
        self.check_no_group(&e)?;
        Ok(e)
    }

    /// `function name(p1, p2, ...) [newlines] { body }`.  The name is a
    /// FUNC_NAME token when written `f(` and a NAME when written `f (`; both
    /// are fine here.  A builtin or keyword lexes as something else and is
    /// rejected.
    fn parse_function(&mut self, prog: &mut Program) -> Result<(), Error> {
        self.expect(&Tok::Function)?;
        let line = self.line();
        let name = match self.tok() {
            Tok::Name(n) | Tok::FuncName(n) => n.clone(),
            _ => return Err(self.error("expected function name")),
        };
        self.advance();
        if SPECIAL_VARS.contains(&name.as_str()) {
            return Err(Error::syntax(
                format!("{name} is a special variable and cannot be a function name"),
                line,
            ));
        }
        if prog.function(&name).is_some() {
            return Err(Error::syntax(
                format!("function {name} is defined twice"),
                line,
            ));
        }
        self.expect(&Tok::LParen)?;
        let mut params: Vec<String> = Vec::new();
        if !self.at(&Tok::RParen) {
            loop {
                let pline = self.line();
                let Tok::Name(p) = self.tok().clone() else {
                    return Err(self.error("expected parameter name"));
                };
                self.advance();
                let problem = if p == name {
                    Some(format!(
                        "function {name}: parameter has the same name as the function"
                    ))
                } else if SPECIAL_VARS.contains(&p.as_str()) {
                    Some(format!(
                        "function {name}: special variable {p} cannot be a parameter"
                    ))
                } else if params.contains(&p) {
                    Some(format!("function {name}: parameter {p} appears twice"))
                } else {
                    None
                };
                if let Some(msg) = problem {
                    return Err(Error::syntax(msg, pline));
                }
                params.push(p);
                if !self.accept(&Tok::Comma) {
                    break;
                }
                self.skip_newlines();
            }
        }
        self.expect(&Tok::RParen)?;
        self.skip_newlines();
        // Record the function before parsing the body, so a second
        // definition is caught and the body may call itself (recursion).
        prog.functions.push(Function {
            name: name.clone(),
            params,
            body: Vec::new(),
        });
        let index = prog.functions.len() - 1;
        // Inside the body `return` is legal and no loop encloses us.
        // `Option::replace` stores a new value and hands back the old one;
        // `std::mem::replace` does the same for any variable.
        let saved_fn = self.in_function.replace(name);
        let saved_depth = std::mem::replace(&mut self.loop_depth, 0);
        let body = self.parse_block();
        self.in_function = saved_fn;
        self.loop_depth = saved_depth;
        prog.functions[index].body = body?;
        Ok(())
    }

    // ----------------------------------------------------------- statements

    fn parse_block(&mut self) -> Result<Vec<Stmt>, Error> {
        self.expect(&Tok::LBrace)?;
        let mut body = Vec::new();
        loop {
            self.skip_terminators();
            if self.accept(&Tok::RBrace) {
                return Ok(body);
            }
            if self.at(&Tok::Eof) {
                return Err(self.error("missing '}'"));
            }
            body.push(self.parse_statement()?);
        }
    }

    /// A simple statement ends at `;` or newline, or just before `}`.
    fn end_simple(&mut self) -> Result<(), Error> {
        if matches!(self.tok(), Tok::Semicolon | Tok::Newline) {
            self.advance();
        } else if !matches!(self.tok(), Tok::RBrace | Tok::Eof) {
            return Err(self.error("expected ';' or newline"));
        }
        Ok(())
    }

    fn parse_statement(&mut self) -> Result<Stmt, Error> {
        self.tree(|p| p.nested(Self::parse_statement_body))
    }

    fn parse_statement_body(&mut self) -> Result<Stmt, Error> {
        match self.tok() {
            Tok::LBrace => Ok(Stmt::Block(self.parse_block()?)),
            Tok::Semicolon => {
                self.advance();
                Ok(Stmt::Block(Vec::new()))
            }
            Tok::If => self.parse_if(),
            Tok::While => {
                self.advance();
                self.expect(&Tok::LParen)?;
                let test = self.parse_paren_expr()?;
                self.expect(&Tok::RParen)?;
                let body = self.parse_loop_body()?;
                Ok(Stmt::While {
                    test,
                    body: Box::new(body),
                })
            }
            Tok::Do => {
                self.advance();
                self.skip_newlines();
                self.loop_depth += 1;
                let body = self.parse_statement();
                self.loop_depth -= 1;
                let body = body?;
                self.skip_terminators();
                self.expect(&Tok::While)?;
                self.expect(&Tok::LParen)?;
                let test = self.parse_paren_expr()?;
                self.expect(&Tok::RParen)?;
                self.end_simple()?;
                Ok(Stmt::DoWhile {
                    body: Box::new(body),
                    test,
                })
            }
            Tok::For => self.parse_for(),
            _ => {
                let stmt = self.parse_simple_statement()?;
                self.end_simple()?;
                Ok(stmt)
            }
        }
    }

    /// Body of if/while/for: optional newlines, then one statement.
    fn parse_body(&mut self) -> Result<Stmt, Error> {
        self.skip_newlines();
        self.parse_statement()
    }

    /// Body of a loop, where `break` and `continue` are legal.
    fn parse_loop_body(&mut self) -> Result<Stmt, Error> {
        self.loop_depth += 1;
        let body = self.parse_body();
        self.loop_depth -= 1;
        body
    }

    /// An expression inside `(...)` of if/while/for, where `>` compares.
    fn parse_paren_expr(&mut self) -> Result<Expr, Error> {
        self.with_no_gt(false, |p| {
            p.skip_newlines();
            let e = p.parse_expr()?;
            p.skip_newlines();
            Ok(e)
        })
    }

    fn parse_if(&mut self) -> Result<Stmt, Error> {
        self.expect(&Tok::If)?;
        self.expect(&Tok::LParen)?;
        let test = self.parse_paren_expr()?;
        self.expect(&Tok::RParen)?;
        let then = Box::new(self.parse_body()?);
        // Look past ';' and newlines for an `else`; if there is none, rewind
        // so the terminators still end the enclosing statement list.
        let save = self.pos;
        self.skip_terminators();
        if self.accept(&Tok::Else) {
            self.skip_newlines();
            let orelse = self.parse_statement()?;
            return Ok(Stmt::If {
                test,
                then,
                orelse: Some(Box::new(orelse)),
            });
        }
        self.pos = save;
        Ok(Stmt::If {
            test,
            then,
            orelse: None,
        })
    }

    fn parse_for(&mut self) -> Result<Stmt, Error> {
        self.expect(&Tok::For)?;
        self.expect(&Tok::LParen)?;
        // `for (name in array)`, recognised by looking 4 tokens ahead, since
        // a C-style loop such as `for (i = 0; ...)` also starts with a name.
        if matches!(self.peek(0), Tok::Name(_))
            && self.peek(1) == &Tok::In
            && matches!(self.peek(2), Tok::Name(_))
            && self.peek(3) == &Tok::RParen
        {
            let t = self.advance();
            let var = self.name_use(t);
            self.advance();
            let t = self.advance();
            let array = self.name_use(t);
            self.expect(&Tok::RParen)?;
            let body = Box::new(self.parse_loop_body()?);
            return Ok(Stmt::ForIn { var, array, body });
        }
        let init = if self.at(&Tok::Semicolon) {
            None
        } else {
            Some(Box::new(self.parse_for_clause()?))
        };
        self.expect(&Tok::Semicolon)?;
        self.skip_newlines();
        let test = if self.at(&Tok::Semicolon) {
            None
        } else {
            Some(self.parse_paren_expr()?)
        };
        self.expect(&Tok::Semicolon)?;
        self.skip_newlines();
        let step = if self.at(&Tok::RParen) {
            None
        } else {
            Some(Box::new(self.parse_for_clause()?))
        };
        self.expect(&Tok::RParen)?;
        let body = Box::new(self.parse_loop_body()?);
        Ok(Stmt::For {
            init,
            test,
            step,
            body,
        })
    }

    /// The init or step part of `for (init; test; step)`.  POSIX calls these
    /// simple statements: usually an expression, but print, printf and
    /// delete are allowed too (onetrue-awk and gawk accept them).
    fn parse_for_clause(&mut self) -> Result<Stmt, Error> {
        if matches!(self.tok(), Tok::Print | Tok::Printf | Tok::Delete) {
            return self.with_no_gt(false, |p| p.parse_simple_statement());
        }
        Ok(Stmt::Expr(self.parse_paren_expr()?))
    }

    fn parse_simple_statement(&mut self) -> Result<Stmt, Error> {
        let line = self.line();
        match self.tok() {
            Tok::Print | Tok::Printf => self.parse_print(),
            Tok::Break | Tok::Continue => {
                // All three references reject these outside a loop.  In a
                // function, a loop in the caller does not count.
                let is_break = self.at(&Tok::Break);
                if self.loop_depth == 0 {
                    let word = if is_break { "break" } else { "continue" };
                    return Err(Error::syntax(format!("{word} is not inside a loop"), line));
                }
                self.advance();
                Ok(if is_break {
                    Stmt::Break
                } else {
                    Stmt::Continue
                })
            }
            Tok::Next => {
                // There is no current record in BEGIN or END.  POSIX leaves
                // `next` there undefined; mawk and gawk reject it while
                // parsing (onetrue-awk only when it runs), and so do we.
                if let Some(which) = self.in_special {
                    return Err(Error::syntax(format!("next used in {which} action"), line));
                }
                // In a function body it is allowed: POSIX restricts `next`
                // only when "invoked in a BEGIN or END action" (gawk agrees;
                // mawk and onetrue-awk reject it).  A call from BEGIN or END
                // is caught when it happens (see `Interp::run_special`).
                self.advance();
                Ok(Stmt::Next)
            }
            Tok::Nextfile => {
                // There is no current file in BEGIN or END: mawk and gawk
                // reject nextfile there while parsing (onetrue-awk only
                // when it runs).  In a function POSIX 2024 leaves it
                // undefined (unlike `next`); mawk and onetrue-awk reject
                // it, gawk allows it, and we follow the majority.
                if let Some(which) = self.in_special {
                    return Err(Error::syntax(
                        format!("nextfile used in {which} action"),
                        line,
                    ));
                }
                if let Some(f) = &self.in_function {
                    let msg = format!("nextfile is not allowed inside a function ({f})");
                    return Err(Error::syntax(msg, line));
                }
                self.advance();
                Ok(Stmt::NextFile)
            }
            Tok::Return => {
                if self.in_function.is_none() {
                    return Err(Error::syntax("return outside a function", line));
                }
                self.advance();
                Ok(Stmt::Return(self.parse_optional_expr()?))
            }
            Tok::Exit => {
                self.advance();
                Ok(Stmt::Exit(self.parse_optional_expr()?))
            }
            Tok::Delete => {
                self.advance();
                let array = self.expect_name_use()?;
                let subs = if self.accept(&Tok::LBracket) {
                    Some(self.parse_expr_list_in_brackets()?)
                } else {
                    None
                };
                Ok(Stmt::Delete { array, subs })
            }
            _ => Ok(Stmt::Expr(self.parse_expr()?)),
        }
    }

    /// The optional value after `return` or `exit`.
    fn parse_optional_expr(&mut self) -> Result<Option<Expr>, Error> {
        if ends_statement(self.tok()) {
            Ok(None)
        } else {
            Ok(Some(self.parse_expr()?))
        }
    }

    /// print/printf with an optional `> file`, `>> file` or `| command`.
    fn parse_print(&mut self) -> Result<Stmt, Error> {
        let is_printf = self.advance().tok == Tok::Printf;
        let mut args = Vec::new();
        let no_args = matches!(
            self.tok(),
            Tok::Semicolon
                | Tok::Newline
                | Tok::RBrace
                | Tok::Eof
                | Tok::Gt
                | Tok::Append
                | Tok::Pipe
                | Tok::RParen
        );
        if !no_args {
            if self.at(&Tok::LParen) {
                self.print_group_at = Some(self.pos);
            }
            args = self.with_no_gt(true, |p| p.parse_expr_list())?;
            // `print (a, b)` is the same as `print a, b`.
            if args.len() == 1
                && let Expr::Group(items) = &args[0]
            {
                args = items.clone();
            }
        }
        for a in &args {
            self.check_no_group(a)?;
        }
        let kind = match self.tok() {
            Tok::Gt => Some(Redirect::Write),
            Tok::Append => Some(Redirect::Append),
            Tok::Pipe => Some(Redirect::Pipe),
            _ => None,
        };
        let mut redirect = None;
        if let Some(kind) = kind {
            self.advance();
            // Parsed at concatenation level (like gawk): `> "a" "b"` writes
            // to file "ab"; a ?: or comparison target needs parentheses.
            let dest = self.with_no_gt(true, |p| p.parse_concat())?;
            redirect = Some((kind, dest));
        }
        // A bare `printf` (no format) prints nothing: onetrue-awk and gawk
        // accept it, mawk rejects it.
        Ok(if is_printf {
            Stmt::Printf { args, redirect }
        } else {
            Stmt::Print { args, redirect }
        })
    }

    // ---------------------------------------------------------- expressions

    fn parse_expr_list(&mut self) -> Result<Vec<Expr>, Error> {
        let mut exprs = vec![self.parse_expr()?];
        while self.accept(&Tok::Comma) {
            self.skip_newlines();
            exprs.push(self.parse_expr()?);
        }
        Ok(exprs)
    }

    /// `expr, expr ]` after `name[`.
    fn parse_expr_list_in_brackets(&mut self) -> Result<Vec<Expr>, Error> {
        let exprs = self.with_no_gt(false, |p| p.parse_expr_list())?;
        self.expect(&Tok::RBracket)?;
        Ok(exprs)
    }

    /// Assignment level (right associative).  The left side is parsed as an
    /// ordinary expression, then checked to be assignable; recursing for
    /// the right side makes `a = b = 1` group as `a = (b = 1)`.
    fn parse_expr(&mut self) -> Result<Expr, Error> {
        self.nested(Self::parse_expr_body)
    }

    fn parse_expr_body(&mut self) -> Result<Expr, Error> {
        let left = self.parse_ternary()?;
        if let Some(op) = assign_op(self.tok()) {
            if !left.is_lvalue() {
                return Err(self.error("assignment to non-lvalue"));
            }
            self.advance();
            self.skip_newlines();
            let right = self.parse_expr()?;
            self.check_no_group(&right)?;
            return Ok(Expr::Assign(op, bx(left), bx(right)));
        }
        Ok(left)
    }

    fn parse_ternary(&mut self) -> Result<Expr, Error> {
        self.nested(Self::parse_ternary_body)
    }

    fn parse_ternary_body(&mut self) -> Result<Expr, Error> {
        let test = self.parse_or()?;
        if self.accept(&Tok::Question) {
            self.skip_newlines();
            let a = self.parse_expr()?;
            self.skip_newlines();
            self.expect(&Tok::Colon)?;
            self.skip_newlines();
            let b = self.parse_expr()?;
            return Ok(Expr::Cond(bx(test), bx(a), bx(b)));
        }
        Ok(test)
    }

    /// Turn `operand` into an assignment if an assignment operator follows.
    ///
    /// Used for the right operand of `|| && ~ !~` and the comparisons,
    /// where all three references accept an assignment without
    /// parentheses: `gsub(/x/, "y") && $0 = $1` means `... && ($0 = $1)`,
    /// and `1 < x = 3` means `1 < (x = 3)`.  After arithmetic,
    /// concatenation or a unary operator it stays a syntax error, reported
    /// by `parse_expr` when it finds the `=`.
    fn maybe_assign(&mut self, operand: Expr) -> Result<Expr, Error> {
        if let Some(op) = assign_op(self.tok())
            && operand.is_lvalue()
        {
            self.advance();
            self.skip_newlines();
            let value = self.parse_expr()?;
            self.check_no_group(&value)?;
            return Ok(Expr::Assign(op, bx(operand), bx(value)));
        }
        Ok(operand)
    }

    fn parse_or(&mut self) -> Result<Expr, Error> {
        // Left-associative levels use a loop: a || b || c is (a || b) || c.
        let mut left = self.parse_and()?;
        while self.accept(&Tok::Or) {
            self.link()?;
            self.skip_newlines();
            let right = self.parse_and()?;
            left = Expr::Or(bx(left), bx(self.maybe_assign(right)?));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, Error> {
        let mut left = self.parse_in()?;
        while self.accept(&Tok::And) {
            self.link()?;
            self.skip_newlines();
            let right = self.parse_in()?;
            left = Expr::And(bx(left), bx(self.maybe_assign(right)?));
        }
        Ok(left)
    }

    fn parse_in(&mut self) -> Result<Expr, Error> {
        let mut left = self.parse_match()?;
        while self.accept(&Tok::In) {
            self.link()?;
            // `(i, j) in arr` tests the multi-dimensional key i SUBSEP j.
            let subs = match left {
                Expr::Group(items) => {
                    self.group_awaits_in = false;
                    items
                }
                // The list before this `in` became an operand of something
                // tighter first, as in `-(i, j) in arr`.
                _ if self.group_awaits_in => return Err(self.group_error()),
                other => vec![other],
            };
            let array = self.expect_name_use()?;
            left = Expr::In(subs, array);
        }
        Ok(left)
    }

    fn parse_match(&mut self) -> Result<Expr, Error> {
        let mut left = self.parse_comparison()?;
        while matches!(self.tok(), Tok::Tilde | Tok::NoMatch) {
            self.link()?;
            let negate = self.advance().tok == Tok::NoMatch;
            let right = self.parse_comparison()?;
            let regex = bx(self.maybe_assign(right)?);
            left = Expr::Match {
                negate,
                left: bx(left),
                regex,
            };
        }
        Ok(left)
    }

    fn parse_comparison(&mut self) -> Result<Expr, Error> {
        let left = self.parse_pipe_getline()?;
        // `if`, not `while`: comparisons do not chain (a < b < c is an
        // error).  Inside print arguments a bare '>' is left for redirection.
        if let Some(op) = compare_op(self.tok())
            && !(op == CmpOp::Gt && self.no_gt)
        {
            self.advance();
            let right = self.parse_pipe_getline()?;
            let right = self.maybe_assign(right)?;
            return Ok(Expr::Compare(op, bx(left), bx(right)));
        }
        Ok(left)
    }

    fn parse_pipe_getline(&mut self) -> Result<Expr, Error> {
        let mut left = self.parse_concat()?;
        while self.at(&Tok::Pipe) && self.peek(1) == &Tok::Getline {
            self.link()?;
            self.advance();
            self.advance();
            let target = self.parse_optional_lvalue()?;
            left = Expr::Getline {
                kind: GetlineKind::Cmd,
                target: target.map(bx),
                source: Some(bx(left)),
            };
            // The result is an ordinary operand for the arithmetic that
            // follows: `cmd | getline x + 1` is `(cmd | getline x) + 1`, as
            // in gawk and mawk (onetrue-awk differs).  pyawk v1.0 rejects
            // this; hawk follows the majority.  See DESIGN.md.
            if self.accept(&Tok::Caret) {
                let exponent = self.parse_power_rhs()?;
                left = Expr::Binary(BinOp::Pow, bx(left), bx(exponent));
            }
            left = self.multiplicative_rest(left)?;
            left = self.additive_rest(left)?;
            // `cmd | getline x y` is `(cmd | getline x) y`: the result may be
            // concatenated with what follows (all three references).
            while starts_concat(self.tok()) {
                self.link()?;
                let right = self.parse_additive()?;
                left = Expr::Concat(bx(left), bx(right));
            }
        }
        Ok(left)
    }

    /// Concatenation has no operator: two operands side by side.  We keep
    /// concatenating while the next token can start an operand.  `-` cannot
    /// (see [`starts_concat`]), so `a -1` is subtraction and `a " " -1` is
    /// `a (" " - 1)`, as the POSIX grammar says.
    fn parse_concat(&mut self) -> Result<Expr, Error> {
        let mut left = self.parse_additive()?;
        while starts_concat(self.tok()) {
            self.link()?;
            let right = self.parse_additive()?;
            left = Expr::Concat(bx(left), bx(right));
        }
        Ok(left)
    }

    fn parse_additive(&mut self) -> Result<Expr, Error> {
        let left = self.parse_multiplicative()?;
        self.additive_rest(left)
    }

    /// The `+ x - y ...` that may follow an already parsed left operand.
    fn additive_rest(&mut self, mut left: Expr) -> Result<Expr, Error> {
        loop {
            let op = match self.tok() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => return Ok(left),
            };
            self.link()?;
            self.advance();
            let right = self.parse_multiplicative()?;
            left = Expr::Binary(op, bx(left), bx(right));
        }
    }

    fn parse_multiplicative(&mut self) -> Result<Expr, Error> {
        let left = self.parse_unary()?;
        self.multiplicative_rest(left)
    }

    /// The `* x / y ...` that may follow an already parsed left operand.
    fn multiplicative_rest(&mut self, mut left: Expr) -> Result<Expr, Error> {
        loop {
            let op = match self.tok() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::Percent => BinOp::Mod,
                _ => return Ok(left),
            };
            self.link()?;
            self.advance();
            let right = self.parse_unary()?;
            left = Expr::Binary(op, bx(left), bx(right));
        }
    }

    fn parse_unary(&mut self) -> Result<Expr, Error> {
        self.nested(Self::parse_unary_body)
    }

    fn parse_unary_body(&mut self) -> Result<Expr, Error> {
        // Unary minus binds looser than ^, so -2^2 is -(2^2) = -4.
        if let Some(op) = unary_op(self.tok()) {
            self.advance();
            return Ok(Expr::Unary(op, bx(self.parse_unary()?)));
        }
        self.parse_power()
    }

    fn parse_power(&mut self) -> Result<Expr, Error> {
        let base = self.parse_postfix()?;
        // Right associative via recursion: 2^3^2 is 2^(3^2) = 512.
        if self.accept(&Tok::Caret) {
            let exponent = self.parse_power_rhs()?;
            return Ok(Expr::Binary(BinOp::Pow, bx(base), bx(exponent)));
        }
        Ok(base)
    }

    fn parse_power_rhs(&mut self) -> Result<Expr, Error> {
        self.nested(Self::parse_power_rhs_body)
    }

    fn parse_power_rhs_body(&mut self) -> Result<Expr, Error> {
        // The exponent may carry its own sign: 2^-1 is 0.5.
        if let Some(op) = unary_op(self.tok()) {
            self.advance();
            return Ok(Expr::Unary(op, bx(self.parse_power_rhs()?)));
        }
        self.parse_power()
    }

    /// Prefix `++x` / `--x` and postfix `x++` / `x--`.
    fn parse_postfix(&mut self) -> Result<Expr, Error> {
        self.nested(Self::parse_postfix_body)
    }

    fn parse_postfix_body(&mut self) -> Result<Expr, Error> {
        if matches!(self.tok(), Tok::Incr | Tok::Decr) {
            let incr = self.advance().tok == Tok::Incr;
            let target = self.parse_postfix()?;
            if !target.is_lvalue() {
                let op = if incr { "++" } else { "--" };
                return Err(self.error(&format!("{op} applied to non-lvalue")));
            }
            return Ok(Expr::IncDec {
                incr,
                prefix: true,
                target: bx(target),
            });
        }
        let e = self.parse_primary()?;
        if e.is_lvalue() && matches!(self.tok(), Tok::Incr | Tok::Decr) {
            return Ok(self.postfix_incdec(e));
        }
        Ok(e)
    }

    /// Wrap `target` in the postfix `++`/`--` that is the current token.
    fn postfix_incdec(&mut self, target: Expr) -> Expr {
        let incr = self.advance().tok == Tok::Incr;
        Expr::IncDec {
            incr,
            prefix: false,
            target: bx(target),
        }
    }

    /// The optional variable after `getline` (None if there is none).
    fn parse_optional_lvalue(&mut self) -> Result<Option<Expr>, Error> {
        if self.at_kind(&Tok::Name(String::new())) {
            let t = self.advance();
            let name = self.name_use(t);
            if self.accept(&Tok::LBracket) {
                return Ok(Some(Expr::Index(name, self.parse_expr_list_in_brackets()?)));
            }
            return Ok(Some(Expr::Var(name)));
        }
        if self.accept(&Tok::Dollar) {
            return Ok(Some(Expr::Field(bx(self.parse_field_operand()?))));
        }
        Ok(None)
    }

    /// What follows `$`.  It binds tightly, so `$NF-1` is `($NF) - 1` and
    /// `$i++` increments the field, not i.  Two less obvious cases, where
    /// all three references agree:
    ///
    /// * after a unary operator the operand runs on to a unary expression,
    ///   increments included: `$-i++` is `$(-(i++))` and `$-x^2` is
    ///   `$(-(x^2))`, but `$-x*2` is `($(-x)) * 2`;
    /// * a nested `$` takes a following `++`/`--` itself: `$$a++` is
    ///   `$(($a)++)`.
    fn parse_field_operand(&mut self) -> Result<Expr, Error> {
        self.nested(Self::parse_field_operand_body)
    }

    fn parse_field_operand_body(&mut self) -> Result<Expr, Error> {
        if matches!(self.tok(), Tok::Incr | Tok::Decr) {
            return self.parse_postfix();
        }
        if let Some(op) = unary_op(self.tok()) {
            self.advance();
            return Ok(Expr::Unary(op, bx(self.parse_unary()?)));
        }
        if self.accept(&Tok::Dollar) {
            let inner = Expr::Field(bx(self.parse_field_operand()?));
            if matches!(self.tok(), Tok::Incr | Tok::Decr) {
                return Ok(self.postfix_incdec(inner));
            }
            return Ok(inner);
        }
        self.parse_primary()
    }

    /// The tightest level: literals, variables, `$`, `(...)`, calls, getline.
    fn parse_primary(&mut self) -> Result<Expr, Error> {
        let line = self.line();
        // `.clone()` copies the token so that `self` is free to change below.
        match self.tok().clone() {
            Tok::Number(v) => {
                self.advance();
                Ok(Expr::Num(v))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Expr::Str(s))
            }
            Tok::Ere(s) => {
                self.advance();
                Ok(Expr::Regex(s))
            }
            Tok::Dollar => {
                self.advance();
                Ok(Expr::Field(bx(self.parse_field_operand()?)))
            }
            Tok::LParen => {
                let start = self.pos;
                self.advance();
                let mut exprs = self.with_no_gt(false, |p| -> Result<Vec<Expr>, Error> {
                    p.skip_newlines();
                    let mut exprs = vec![p.parse_expr()?];
                    while p.accept(&Tok::Comma) {
                        p.skip_newlines();
                        exprs.push(p.parse_expr()?);
                    }
                    p.skip_newlines();
                    p.expect(&Tok::RParen)?;
                    Ok(exprs)
                })?;
                // One expression in parentheses is just that expression (the
                // tree's shape records the grouping); a list is a Group.
                if exprs.len() == 1 {
                    return Ok(exprs.remove(0));
                }
                self.parse_group(start, exprs)
            }
            Tok::Name(name) => {
                let t = self.advance();
                self.name_use(t);
                if self.accept(&Tok::LBracket) {
                    return Ok(Expr::Index(name, self.parse_expr_list_in_brackets()?));
                }
                Ok(Expr::Var(name))
            }
            Tok::FuncName(name) => {
                self.advance();
                let args = self.parse_call_args(None)?;
                self.calls.push((name.clone(), args.len(), line));
                Ok(Expr::UserCall(name, args))
            }
            Tok::Builtin(b) => self.parse_builtin(b),
            Tok::Getline => {
                self.advance();
                let target = self.parse_optional_lvalue()?.map(bx);
                if self.accept(&Tok::Lt) {
                    // A tight operand: `getline < "a" "b"` is
                    // `(getline < "a") "b"`.
                    let source = self.parse_postfix()?;
                    return Ok(Expr::Getline {
                        kind: GetlineKind::File,
                        target,
                        source: Some(bx(source)),
                    });
                }
                Ok(Expr::Getline {
                    kind: GetlineKind::Simple,
                    target,
                    source: None,
                })
            }
            _ => Err(self.error("unexpected token")),
        }
    }

    fn parse_builtin(&mut self, b: Builtin) -> Result<Expr, Error> {
        let line = self.advance().line;
        if !self.at(&Tok::LParen) {
            // `length` without parentheses means length($0).
            if b == Builtin::Length {
                return Ok(Expr::Call(b, Vec::new()));
            }
            return Err(self.error(&format!("expected '(' after {}", b.name())));
        }
        let mut parenthesised = Vec::new();
        let args = self.parse_call_args(Some(&mut parenthesised))?;
        // A wrong number of arguments is a syntax error, even in code that
        // never runs (mawk and gawk; onetrue-awk for most builtins).
        let (lo, hi) = b.arity();
        if !(lo..=hi).contains(&args.len()) {
            let want = if lo == hi {
                lo.to_string()
            } else if hi == usize::MAX {
                format!("at least {lo}")
            } else {
                format!("{lo} to {hi}")
            };
            let msg = format!(
                "function {} called with {} args (takes {want})",
                b.name(),
                args.len()
            );
            return Err(Error::syntax(msg, line));
        }
        // split's array must be a plain name.  mawk and onetrue-awk reject
        // anything else while parsing (gawk only when it runs).  `(a)` makes
        // the same tree as `a`, hence the separate `parenthesised` record.
        if b == Builtin::Split && !(matches!(args[1], Expr::Var(_)) && !parenthesised[1]) {
            return Err(Error::syntax(
                "split: second argument must be an array name",
                line,
            ));
        }
        // sub/gsub assign to their target, so it must be a variable, an
        // element or a field.  POSIX leaves anything else undefined; mawk and
        // onetrue-awk reject it while parsing.  `(x)` is a value, not a
        // variable, in all three reference awks.
        if matches!(b, Builtin::Sub | Builtin::Gsub)
            && args.len() == 3
            && (!args[2].is_lvalue() || parenthesised[2])
        {
            let msg = format!(
                "{}: third argument must be a variable, element or field",
                b.name()
            );
            return Err(Error::syntax(msg, line));
        }
        Ok(Expr::Call(b, args))
    }

    /// `( [expr, expr, ...] )` after a builtin or user function name.
    /// Inside, `>` compares again, and newlines may follow the commas.
    ///
    /// If `parenthesised` is given (`Some`), one entry per argument is
    /// pushed onto it: whether that argument starts with `(`.  `&mut
    /// Vec<bool>` is a borrowed vector the function may add to; the
    /// `Option` makes it optional, like Python's `starts=None`.
    fn parse_call_args(
        &mut self,
        mut parenthesised: Option<&mut Vec<bool>>,
    ) -> Result<Vec<Expr>, Error> {
        self.expect(&Tok::LParen)?;
        let args = self.with_no_gt(false, |p| -> Result<Vec<Expr>, Error> {
            p.skip_newlines();
            let mut args = Vec::new();
            if !p.at(&Tok::RParen) {
                loop {
                    // `as_deref_mut` lends out the vector inside the
                    // Option without giving it away, so the loop can use
                    // it again next time round.
                    if let Some(starts) = parenthesised.as_deref_mut() {
                        starts.push(p.at(&Tok::LParen));
                    }
                    args.push(p.parse_expr()?);
                    if !p.accept(&Tok::Comma) {
                        break;
                    }
                    p.skip_newlines();
                }
            }
            p.skip_newlines();
            p.expect(&Tok::RParen)?;
            Ok(args)
        })?;
        for a in &args {
            self.check_no_group(a)?;
        }
        Ok(args)
    }
}

#[cfg(test)]
mod tests {
    //! A port of pyawk v1.0's `tests/test_parser.py`, plus extra cases.
    use super::*;
    use crate::ast::{AssignOp, BinOp, CmpOp, Expr, GetlineKind, Redirect, Rule, Stmt, UnOp};

    // ---- small builders, so expected trees stay readable
    fn n(v: f64) -> Expr {
        Expr::Num(v)
    }
    fn v(name: &str) -> Expr {
        Expr::Var(name.into())
    }
    fn s(text: &str) -> Expr {
        Expr::Str(text.as_bytes().to_vec())
    }
    fn re(text: &str) -> Expr {
        Expr::Regex(text.as_bytes().to_vec())
    }
    fn b(e: Expr) -> Box<Expr> {
        Box::new(e)
    }
    fn bin(op: BinOp, l: Expr, r: Expr) -> Expr {
        Expr::Binary(op, b(l), b(r))
    }
    fn cmp(op: CmpOp, l: Expr, r: Expr) -> Expr {
        Expr::Compare(op, b(l), b(r))
    }
    fn field(e: Expr) -> Expr {
        Expr::Field(b(e))
    }
    fn assign(t: Expr, val: Expr) -> Expr {
        Expr::Assign(AssignOp(None), b(t), b(val))
    }

    fn p(src: &str) -> Program {
        match parse(src.as_bytes()) {
            Ok(prog) => prog,
            Err(e) => panic!("{src:?} failed to parse: {e}"),
        }
    }

    fn err(src: &str) -> String {
        match parse(src.as_bytes()) {
            Ok(prog) => panic!("{src:?} parsed but should not: {prog:?}"),
            Err(e) => e.to_string(),
        }
    }

    /// The first statement of `BEGIN { src }`.
    fn stmt(src: &str) -> Stmt {
        p(&format!("BEGIN {{ {src} }}")).begin[0][0].clone()
    }

    /// The expression of `BEGIN { src }`, which must be an expression statement.
    fn expr(src: &str) -> Expr {
        match stmt(src) {
            Stmt::Expr(e) => e,
            other => panic!("not an expression statement: {other:?}"),
        }
    }

    /// The right-hand side of `x = ...`.
    fn rhs(src: &str) -> Expr {
        match expr(&format!("x = {src}")) {
            Expr::Assign(_, _, value) => *value,
            other => panic!("not an assignment: {other:?}"),
        }
    }

    #[test]
    fn program_structure() {
        let prog = p("BEGIN { x = 1 }\n/re/\n$1 > 2 { print }\n{ n++ }\nEND { print n }");
        assert_eq!(
            (prog.begin.len(), prog.end.len(), prog.rules.len()),
            (1, 1, 3)
        );
        assert_eq!(prog.rules[0].action, None);
        assert_eq!(prog.rules[0].pattern, Some(re("re")));
        assert_eq!(prog.rules[2].pattern, None);
    }

    #[test]
    fn precedence_arith() {
        assert_eq!(
            rhs("1 + 2 * 3"),
            bin(BinOp::Add, n(1.0), bin(BinOp::Mul, n(2.0), n(3.0)))
        );
    }

    #[test]
    fn power_right_assoc_and_unary_minus() {
        let pow = bin(BinOp::Pow, n(2.0), bin(BinOp::Pow, n(3.0), n(2.0)));
        assert_eq!(rhs("-2 ^ 3 ^ 2"), Expr::Unary(UnOp::Neg, b(pow)));
        // The exponent may carry its own sign.
        assert_eq!(
            rhs("2 ^ -1"),
            bin(BinOp::Pow, n(2.0), Expr::Unary(UnOp::Neg, b(n(1.0))))
        );
    }

    #[test]
    fn concat_lower_than_additive() {
        let sum = bin(BinOp::Add, n(1.0), n(2.0));
        assert_eq!(rhs("1 + 2 \"a\""), Expr::Concat(b(sum), b(s("a"))));
    }

    #[test]
    fn minus_is_binary_not_concat() {
        assert_eq!(rhs("a -1"), bin(BinOp::Sub, v("a"), n(1.0)));
        // a " " -1 is a (" " - 1)
        let inner = bin(BinOp::Sub, s(" "), n(1.0));
        assert_eq!(rhs("a \" \" -1"), Expr::Concat(b(v("a")), b(inner)));
    }

    #[test]
    fn comparison_lower_than_concat() {
        let e = rhs("\"a\" \"b\" < \"c\"");
        let Expr::Compare(CmpOp::Lt, left, _) = e else {
            panic!("{e:?}")
        };
        assert!(matches!(*left, Expr::Concat(..)));
    }

    #[test]
    fn match_in_and_or_ternary() {
        let e = rhs("a ~ /b/ && k in arr || c ? 1 : 2");
        let Expr::Cond(test, _, _) = e else {
            panic!("{e:?}")
        };
        let Expr::Or(left, _) = *test else { panic!() };
        let Expr::And(l, r) = *left else { panic!() };
        assert!(matches!(*l, Expr::Match { negate: false, .. }));
        assert_eq!(*r, Expr::In(vec![v("k")], "arr".into()));
    }

    #[test]
    fn assignment_right_assoc() {
        let e = expr("a = b += 3");
        let Expr::Assign(AssignOp(None), _, value) = e else {
            panic!()
        };
        assert!(matches!(
            *value,
            Expr::Assign(AssignOp(Some(BinOp::Add)), ..)
        ));
    }

    #[test]
    fn assignment_after_logical_and_comparison_operators() {
        // gsub(...) && $0 = $1  means  ... && ($0 = $1)
        let e = expr("x && $0 = $1");
        assert_eq!(
            e,
            Expr::And(b(v("x")), b(assign(field(n(0.0)), field(n(1.0)))))
        );
        assert_eq!(
            rhs("1 < y = 3"),
            cmp(CmpOp::Lt, n(1.0), assign(v("y"), n(3.0)))
        );
        // ... but not after arithmetic or concatenation.
        err("BEGIN { 1 + x = 3 }");
        err("BEGIN { x y = 3 }");
    }

    #[test]
    fn field_precedence() {
        let inc = |t: Expr, prefix: bool| Expr::IncDec {
            incr: true,
            prefix,
            target: b(t),
        };
        assert_eq!(expr("$i++"), inc(field(v("i")), false));
        assert_eq!(rhs("$NF - 1"), bin(BinOp::Sub, field(v("NF")), n(1.0)));
        assert_eq!(rhs("$++i"), field(inc(v("i"), true)));
        // $-i++ is $(-(i++)); $-x*2 is ($(-x)) * 2
        let neg = |e: Expr| Expr::Unary(UnOp::Neg, b(e));
        assert_eq!(rhs("$-i++"), field(neg(inc(v("i"), false))));
        assert_eq!(rhs("$-x*2"), bin(BinOp::Mul, field(neg(v("x"))), n(2.0)));
        // $$a++ is $(($a)++)
        assert_eq!(rhs("$$a++"), field(inc(field(v("a")), false)));
    }

    #[test]
    fn grouping_in() {
        assert_eq!(
            rhs("(1, 2) in a"),
            Expr::In(vec![n(1.0), n(2.0)], "a".into())
        );
    }

    #[test]
    fn parenthesised_lists_are_checked_while_parsing() {
        // Valid: before `in`, and as the whole argument list of print.
        for good in [
            "BEGIN { x = (1, 2) in a }",
            "BEGIN { print ((1, 2) in a) }",
            "BEGIN { print (1, 2) in a }",
            "BEGIN { print (1, 2) }",
            "BEGIN { print (1, 2)\n}",
            "BEGIN { print (1, 2); printf (\"%s\", 1) }",
            "BEGIN { print (1, 2) > \"f\"; print (1, 2) >> \"f\"; print (1, 2) | \"cat\" }",
            "BEGIN { if ((1, 2) in a) print }",
        ] {
            if let Err(e) = parse(good.as_bytes()) {
                panic!("{good}: {e}");
            }
        }
        // Anywhere else it is a syntax error (gawk, mawk and onetrue-awk
        // agree), not a run-time "used as a value" error.
        for bad in [
            "BEGIN { x = 1 + (1, 2) }",
            "BEGIN { print -(1, 2) }",
            "BEGIN { print (1, 2) (3) }",
            "BEGIN { print (1, 2) \"\" }",
            "BEGIN { print 1 ? (1, 2) : 3 }",
            "BEGIN { if ((1, 2)) print }",
            "BEGIN { print ((1, 2)) }",
            "BEGIN { print !(1, 2) in a }",
            "BEGIN { print 1 + (1, 2) in a }",
            "BEGIN { print $(1, 2) in a }",
            "BEGIN { f((1, 2)) }",
        ] {
            let e = err(bad);
            assert!(
                e.starts_with(
                    "syntax error at source line 1: parenthesised list is only valid before 'in' or after print"
                ),
                "{bad}: {e}"
            );
        }
    }

    #[test]
    fn print_redirect_vs_comparison() {
        let Stmt::Print { args, redirect } = stmt("print a > \"f\"") else {
            panic!()
        };
        assert_eq!(args, [v("a")]);
        assert_eq!(redirect, Some((Redirect::Write, s("f"))));
        let Stmt::Print { args, redirect } = stmt("print (a > 1)") else {
            panic!()
        };
        assert_eq!(redirect, None);
        assert_eq!(args, [cmp(CmpOp::Gt, v("a"), n(1.0))]);
        let Stmt::Print { args, redirect } = stmt("print (a, b) > \"f\"") else {
            panic!()
        };
        assert_eq!(args.len(), 2);
        assert_eq!(redirect.unwrap().0, Redirect::Write);
        let Stmt::Print { redirect, .. } = stmt("print \"x\" | \"sort\"") else {
            panic!()
        };
        assert_eq!(redirect.unwrap().0, Redirect::Pipe);
        // The destination is parsed at concatenation level: "a" "b" is one file.
        let Stmt::Print { redirect, .. } = stmt("print 1 >> \"a\" \"b\"") else {
            panic!()
        };
        assert_eq!(
            redirect,
            Some((Redirect::Append, Expr::Concat(b(s("a")), b(s("b")))))
        );
        // Inside call arguments and brackets '>' compares again.
        let Stmt::Print { args, redirect } = stmt("print f(a > 1), x[b > 2]") else {
            panic!()
        };
        assert_eq!(redirect, None);
        assert_eq!(args.len(), 2);
        // printf with no arguments at all is accepted.
        assert_eq!(
            stmt("printf"),
            Stmt::Printf {
                args: vec![],
                redirect: None
            }
        );
    }

    #[test]
    fn getline_forms() {
        let gl = |kind, target: Option<Expr>, source: Option<Expr>| Expr::Getline {
            kind,
            target: target.map(Box::new),
            source: source.map(Box::new),
        };
        assert_eq!(expr("getline"), gl(GetlineKind::Simple, None, None));
        assert_eq!(
            expr("getline x < f"),
            gl(GetlineKind::File, Some(v("x")), Some(v("f")))
        );
        assert_eq!(
            expr("\"cmd\" | getline"),
            gl(GetlineKind::Cmd, None, Some(s("cmd")))
        );
        // getline < "a" "b" is (getline < "a") "b"
        let first = gl(GetlineKind::File, None, Some(s("a")));
        assert_eq!(
            expr("getline < \"a\" \"b\""),
            Expr::Concat(b(first), b(s("b")))
        );
        // cmd | getline x y is (cmd | getline x) y
        let first = gl(GetlineKind::Cmd, Some(v("x")), Some(v("c")));
        assert_eq!(
            expr("c | getline x y"),
            Expr::Concat(b(first.clone()), b(v("y")))
        );
        // cmd | getline x + 1 is (cmd | getline x) + 1, as gawk and mawk
        // parse it (a deliberate difference from pyawk v1.0, which rejects it).
        let one = || b(Expr::Num(1.0));
        assert_eq!(
            expr("c | getline x + 1"),
            Expr::Binary(BinOp::Add, b(first.clone()), one())
        );
        assert_eq!(
            expr("c | getline x * 2 - 1"),
            Expr::Binary(
                BinOp::Sub,
                b(Expr::Binary(
                    BinOp::Mul,
                    b(first.clone()),
                    b(Expr::Num(2.0))
                )),
                one()
            )
        );
        assert_eq!(
            expr("c | getline x ^ 2"),
            Expr::Binary(BinOp::Pow, b(first.clone()), b(Expr::Num(2.0)))
        );
        assert_eq!(
            expr("c | getline x + 1 \"a\""),
            Expr::Concat(b(Expr::Binary(BinOp::Add, b(first), one())), b(s("a")))
        );
        // "a" "b" | getline: the command is the whole concatenation.
        let cmd = Expr::Concat(b(s("a")), b(s("b")));
        assert_eq!(
            expr("\"a\" \"b\" | getline"),
            gl(GetlineKind::Cmd, None, Some(cmd))
        );
        let target = field(n(1.0));
        assert_eq!(
            expr("getline $1"),
            gl(GetlineKind::Simple, Some(target), None)
        );
    }

    #[test]
    fn control_statements_parse() {
        let src = "
        {
            if (x) y = 1; else y = 2
            if (x)
                y = 1
            else
                y = 2
            while (i < 3) i++
            do i--; while (i > 0)
            for (i = 0; i < 3; i++) ;
            for (k in a) delete a[k]
            for (;;) break
            next
        }
        ";
        let prog = p(src);
        let body = prog.rules[0].action.as_ref().unwrap();
        let kinds: Vec<&str> = body
            .iter()
            .map(|s| match s {
                Stmt::If {
                    orelse: Some(_), ..
                } => "IfElse",
                Stmt::While { .. } => "While",
                Stmt::DoWhile { .. } => "DoWhile",
                Stmt::For { .. } => "For",
                Stmt::ForIn { .. } => "ForIn",
                Stmt::Next => "Next",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "IfElse", "IfElse", "While", "DoWhile", "For", "ForIn", "For", "Next"
            ]
        );
    }

    #[test]
    fn for_clauses_may_be_print_or_delete() {
        let Stmt::For { init, step, .. } = stmt("for (print 9; 0; delete a) ;") else {
            panic!()
        };
        assert!(matches!(init.as_deref(), Some(Stmt::Print { .. })));
        assert!(matches!(step.as_deref(), Some(Stmt::Delete { .. })));
    }

    #[test]
    fn next_in_begin_or_end_is_a_syntax_error() {
        // POSIX leaves `next` in BEGIN/END undefined; mawk and gawk reject
        // it while parsing (onetrue-awk only when it runs), so the program
        // never starts, even where the `next` could never run.
        for (src, which) in [
            ("BEGIN { next }", "BEGIN"),
            ("END { next }", "END"),
            ("BEGIN { if (0) next }", "BEGIN"),
            ("END { while (1) { next } }", "END"),
        ] {
            let msg = err(src);
            assert!(
                msg.contains(&format!("next used in {which} action")),
                "{src}: {msg}"
            );
        }
    }

    #[test]
    fn next_in_a_function_is_allowed() {
        // POSIX only restricts `next` "invoked in a BEGIN or END action";
        // gawk agrees (a call from BEGIN/END is a run-time error).
        let prog = p("function f() { next } { f() }");
        assert_eq!(prog.functions[0].body, vec![Stmt::Next]);
    }

    #[test]
    fn builtin_argument_count_is_checked_when_parsing() {
        // mawk and gawk reject these while parsing, even in code that never
        // runs; onetrue-awk does for most (it only warns for `length(1, 2)`,
        // `rand(1)` and the like).
        for (call, want) in [
            (
                "substr(\"a\")",
                "function substr called with 1 args (takes 2 to 3)",
            ),
            (
                "substr(\"a\", 1, 2, 3)",
                "function substr called with 4 args (takes 2 to 3)",
            ),
            ("index(1)", "function index called with 1 args (takes 2)"),
            (
                "split(\"a\")",
                "function split called with 1 args (takes 2 to 3)",
            ),
            (
                "split(\"a\", b, \" \", 4)",
                "function split called with 4 args",
            ),
            ("sub(/a/)", "function sub called with 1 args"),
            ("gsub(/a/, \"b\", x, 4)", "function gsub called with 4 args"),
            ("match(1)", "function match called with 1 args (takes 2)"),
            // gawk's three-argument match is an extension
            (
                "match(\"a\", /a/, m)",
                "function match called with 3 args (takes 2)",
            ),
            (
                "length(1, 2)",
                "function length called with 2 args (takes 0 to 1)",
            ),
            ("sin()", "function sin called with 0 args (takes 1)"),
            ("cos(1, 2)", "function cos called with 2 args"),
            ("exp()", "function exp called with 0 args"),
            ("log()", "function log called with 0 args"),
            ("sqrt(1, 2)", "function sqrt called with 2 args"),
            ("int()", "function int called with 0 args"),
            ("atan2(1)", "function atan2 called with 1 args (takes 2)"),
            ("rand(1)", "function rand called with 1 args (takes 0)"),
            (
                "srand(1, 2)",
                "function srand called with 2 args (takes 0 to 1)",
            ),
            ("tolower()", "function tolower called with 0 args"),
            ("toupper(1, 2)", "function toupper called with 2 args"),
            ("system()", "function system called with 0 args"),
            ("close()", "function close called with 0 args (takes 1)"),
            // gawk's two-argument close is an extension
            (
                "close(\"a\", \"b\")",
                "function close called with 2 args (takes 1)",
            ),
            (
                "fflush(1, 2)",
                "function fflush called with 2 args (takes 0 to 1)",
            ),
            (
                "sprintf()",
                "function sprintf called with 0 args (takes at least 1)",
            ),
        ] {
            let msg = err(&format!("BEGIN {{ if (0) x = {call} }}"));
            assert!(msg.contains(want), "{call}: {msg}");
        }
    }

    #[test]
    fn builtin_argument_counts_that_are_allowed() {
        for call in [
            "length",
            "length()",
            "length(1)",
            "substr(\"a\", 1)",
            "substr(\"a\", 1, 2)",
            "split(\"a\", b)",
            "split(\"a\", b, \" \")",
            "sub(/a/, \"b\")",
            "gsub(/a/, \"b\", x)",
            "rand()",
            "srand()",
            "srand(1)",
            "fflush()",
            "fflush(\"a\")",
            "sprintf(\"a\")",
            "sprintf(\"%s %s\", 1, 2)",
        ] {
            p(&format!("BEGIN {{ x = {call} }}"));
        }
    }

    #[test]
    fn split_target_must_be_a_name() {
        // mawk and onetrue-awk reject anything else while parsing (gawk
        // only when it runs).
        for arg in ["3", "x[1]", "$1", "(a)", "a b", "f()"] {
            let src = format!("function f() {{ }} BEGIN {{ if (0) split(\"a b\", {arg}) }}");
            let msg = err(&src);
            assert!(
                msg.contains("split: second argument must be an array name"),
                "{arg}: {msg}"
            );
        }
        p("function g(arr) { return split(\"a b\", arr) } BEGIN { n = split(\"a\", a) }");
    }

    #[test]
    fn sub_target_must_be_an_lvalue() {
        // POSIX leaves anything else undefined; mawk and onetrue-awk reject
        // it while parsing; gawk accepts a literal or a sum.  `(x)` is a
        // value in all three, so the parentheses count.
        for f in ["sub", "gsub"] {
            for arg in [
                "\"c\"", "1 + 2", "x y", "(x)", "($1)", "length", "x++", "$x++", "f()",
            ] {
                let src = format!("function f() {{ }} BEGIN {{ if (0) {f}(/a/, \"b\", {arg}) }}");
                let msg = err(&src);
                let want = format!("{f}: third argument must be a variable, element or field");
                assert!(msg.contains(&want), "{f} {arg}: {msg}");
            }
        }
    }

    #[test]
    fn sub_targets_that_are_allowed() {
        for arg in ["x", "x[1]", "x[1, 2]", "$1", "$(1)", "$NF", "$++x"] {
            p(&format!(
                "BEGIN {{ sub(/a/, \"b\", {arg}); gsub(/a/, \"b\", {arg}) }}"
            ));
        }
    }

    #[test]
    fn nextfile_in_rules() {
        assert_eq!(
            p("{ nextfile }").rules[0].action,
            Some(vec![Stmt::NextFile])
        );
        p("FNR == 2 { if ($1) nextfile; while (1) { nextfile } }");
    }

    #[test]
    fn nextfile_in_begin_end_or_a_function_is_a_syntax_error() {
        // No current file in BEGIN/END: mawk and gawk reject it while
        // parsing.  In a function POSIX leaves it undefined; mawk and
        // onetrue-awk reject it (gawk allows it), so we do too.
        assert!(err("BEGIN { if (0) nextfile }").contains("nextfile used in BEGIN action"));
        assert!(err("END { nextfile }").contains("nextfile used in END action"));
        assert!(
            err("function f() { nextfile }")
                .contains("nextfile is not allowed inside a function (f)")
        );
    }

    #[test]
    fn length_without_parentheses() {
        assert_eq!(
            rhs("length"),
            Expr::Call(crate::lexer::Builtin::Length, vec![])
        );
        assert!(err("BEGIN { x = substr }").contains("expected '(' after substr"));
    }

    // ------------------------------------------------------------ range patterns
    #[test]
    fn range_pattern_with_and_without_action() {
        let prog = p("/a/, /b/ { print 1 }\nNR == 2, NR == 4");
        let (r1, r2) = (&prog.rules[0], &prog.rules[1]);
        assert_eq!((&r1.pattern, &r1.end), (&Some(re("a")), &Some(re("b"))));
        assert!(r1.action.is_some());
        assert_eq!(r2.pattern, Some(cmp(CmpOp::Eq, v("NR"), n(2.0))));
        assert_eq!(r2.end, Some(cmp(CmpOp::Eq, v("NR"), n(4.0))));
        assert_eq!(r2.action, None);
    }

    #[test]
    fn plain_rule_has_no_end_pattern() {
        assert_eq!(p("/a/ { print }").rules[0].end, None);
    }

    #[test]
    fn range_comma_binds_loosest() {
        let r = &p("$1 == \"a\" || $2, /x/").rules[0];
        assert!(matches!(r.pattern, Some(Expr::Or(..))));
        assert_eq!(r.end, Some(re("x")));
        assert!(matches!(
            p("$1 > 2 ? 1 : 0, /5/").rules[0].pattern,
            Some(Expr::Cond(..))
        ));
        assert!(matches!(
            p("x = /2/, /4/").rules[0].pattern,
            Some(Expr::Assign(..))
        ));
    }

    #[test]
    fn range_allows_newline_after_comma_only() {
        assert_eq!(p("/a/,\n\n/b/").rules[0].end, Some(re("b")));
        err("/a/\n, /b/");
    }

    #[test]
    fn range_rules_separated_by_semicolon() {
        let prog = p("/1/, /2/; /5/");
        assert_eq!(prog.rules[0].end, Some(re("2")));
        assert_eq!(prog.rules[1].end, None);
    }

    #[test]
    fn range_pattern_errors() {
        for src in [
            "BEGIN, /x/ { print }", // BEGIN/END cannot be range endpoints
            "/x/, END { print }",
            "/x/, BEGIN",
            "(/a/, /b/)",           // a range is not an expression: no nesting ...
            "/a/, /b/ || /c/, /d/", // ... and only one comma
            "/a/, /b/, /c/",
            "/a/, { print }", // the end condition is required
            "{ x = (/a/, /b/) }",
        ] {
            err(src);
        }
    }

    // ------------------------------------------------------- user-defined functions
    #[test]
    fn function_definition_and_call() {
        let prog = p("function add(a, b) { return a + b }\nBEGIN { x = add(1, 2) }");
        let f = prog.function("add").unwrap();
        assert_eq!(
            (f.name.as_str(), &f.params[..]),
            ("add", &["a".to_string(), "b".to_string()][..])
        );
        assert_eq!(
            f.body,
            [Stmt::Return(Some(bin(BinOp::Add, v("a"), v("b"))))]
        );
        let call = Expr::UserCall("add".into(), vec![n(1.0), n(2.0)]);
        assert_eq!(prog.begin[0][0], Stmt::Expr(assign(v("x"), call)));
    }

    #[test]
    fn function_definition_layouts() {
        // A space before '(' is fine in a definition, and so are a newline
        // after a comma in the parameter list and newlines before the body.
        let prog = p("function f (a,\n b)\n\n{\n return\n}\n;BEGIN { f( ) }");
        let f = prog.function("f").unwrap();
        assert_eq!(f.params, ["a", "b"]);
        assert_eq!(f.body, [Stmt::Return(None)]);
        assert_eq!(
            prog.begin[0][0],
            Stmt::Expr(Expr::UserCall("f".into(), vec![]))
        );
    }

    #[test]
    fn function_may_be_used_before_its_definition() {
        let prog = p("BEGIN { print g(1) }\nfunction g(x) { return x }");
        let names: Vec<&str> = prog.functions.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["g"]);
    }

    #[test]
    fn call_to_undefined_function_parses() {
        // An undefined function is an error only if the call runs.
        assert!(p("BEGIN { if (0) nosuch(1) }").functions.is_empty());
    }

    #[test]
    fn func_is_not_a_keyword() {
        assert_eq!(
            stmt("func = 1; print func"),
            Stmt::Expr(assign(v("func"), n(1.0)))
        );
    }

    #[test]
    fn function_syntax_errors() {
        for bad in [
            "function f(a) { } function f(b) { }", // defined twice
            "function f(f) { }",                   // parameter named like its own function
            "function g() { } function f(g) { }",  // ... or like another function
            "function f(a, a) { }",                // duplicate parameter
            "function f(length) { }",              // builtin name as parameter
            "function f(NR) { }",                  // special variable as parameter
            "function f(FS) { }",
            "function length(x) { }", // builtin name as function name
            "function print(x) { }",  // keyword as function name
            "function NR(x) { }",     // special variable as function name
            "function f() { return 1 } BEGIN { f = 2 }", // function used as variable
            "function f() { return 1 } BEGIN { print f }",
            "BEGIN { f[1] = 1 } function f() { }", // ... as an array
            "BEGIN { x = f (1) } function f(a) { }", // space before '(' in a call
            "BEGIN { delete f } function f() { }",
            "BEGIN { if (1 in f) print } function f() { }",
            "BEGIN { getline f } function f() { }",
            "BEGIN { for (f in a) ; } function f() { }",
            "BEGIN { for (k in f) ; } function f() { }",
            "BEGIN { return 1 }", // return outside a function
            "{ return }",
            "function f() { break }", // break outside a loop
            "BEGIN { continue }",
            "function f(a { }",
            "function f(a,) { }",
            "function (a) { }",
            "function f(a)",
        ] {
            err(bad);
        }
    }

    #[test]
    fn func_keyword_error_mentions_function() {
        assert!(err("func f(x) { return x }").contains("'function'"));
    }

    #[test]
    fn function_name_as_variable_message() {
        let msg = err("function f(a) { } BEGIN { print f (1) }");
        assert!(
            msg.contains("function f called with a space before '(' or used as a variable"),
            "{msg}"
        );
    }

    #[test]
    fn too_many_arguments_is_a_warning() {
        let prog = p("function g(a) { return a } BEGIN { g(1, 2); g(3) }");
        assert_eq!(
            prog.warnings,
            ["line 1: function g called with 2 args, accepts only 1"]
        );
    }

    #[test]
    fn loops_inside_functions_allow_break() {
        let prog = p("function f() { while (1) { break }; for (;;) continue }");
        assert_eq!(prog.function("f").unwrap().body.len(), 2);
    }

    /// Parse `src` on a thread with an 8 MiB stack (a typical main-thread
    /// size, and what hawk falls back to), returning the error message, if
    /// any.  A stack overflow would abort the whole test run.
    fn parse_on_small_stack(src: String) -> Option<String> {
        std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(move || parse(src.as_bytes()).err().map(|e| e.to_string()))
            .unwrap()
            .join()
            .unwrap()
    }

    #[test]
    fn deep_nesting_is_an_error_not_a_crash() {
        let too_deep = "syntax error at source line 1: nesting too deep";
        let n = 100_000;
        let cases = [
            format!("BEGIN {{ x = {}1{} }}", "(".repeat(n), ")".repeat(n)),
            format!("BEGIN {{ x = {}1 }}", "- ".repeat(n)),
            format!("BEGIN {{ x = {}1 }}", "!".repeat(n)),
            format!("BEGIN {{ x = 2^{}1 }}", "- ".repeat(n)),
            format!("BEGIN {{ x = {}1 }}", "$".repeat(n)),
            format!("BEGIN {{ x = {}y }}", "++".repeat(n)),
            format!("BEGIN {{ x = {}1 }}", "y = ".repeat(n)),
            format!("BEGIN {{ x = {}1 }}", "1 ? 1 : ".repeat(n)),
            format!("BEGIN {}{}", "{".repeat(n), "}".repeat(n)),
            format!("BEGIN {{ {} x }}", "if (1) ".repeat(n)),
            format!("BEGIN {{ {} x }}", "while (1) ".repeat(n)),
        ];
        for src in cases {
            let head: String = src.chars().take(30).collect();
            // (The message goes on to say which token it was at.)
            let msg = parse_on_small_stack(src).unwrap_or_default();
            assert!(msg.starts_with(too_deep), "{head}: {msg}");
        }
        // Given hawk's big stack, ordinary depths are fine: as many
        // parentheses as gawk accepts, more than mawk and pyawk (which fail
        // at 1,000).
        let n = 5000;
        for src in [
            format!("BEGIN {{ x = {}1{} }}", "(".repeat(n), ")".repeat(n)),
            format!("BEGIN {}{}", "{".repeat(n), "}".repeat(n)),
            format!("BEGIN {{ x = {}1 }}", "- ".repeat(n)),
        ] {
            let parsed = std::thread::Builder::new()
                .stack_size(1 << 30)
                .spawn(move || parse_with_stack(src.as_bytes(), 1 << 30).err())
                .unwrap()
                .join()
                .unwrap();
            assert_eq!(parsed, None);
        }
    }

    #[test]
    fn parse_is_safe_on_a_default_thread() {
        // `parse` is not told how big its stack is, so it must not assume
        // more than a thread gets by default: 2 MiB for threads Rust
        // spawns (the main thread usually has 8 MiB).
        let src = format!(
            "BEGIN {{ x = {}1{} }}",
            "(".repeat(100_000),
            ")".repeat(100_000)
        );
        let msg = std::thread::spawn(move || parse(src.as_bytes()).err().map(|e| e.to_string()))
            .join()
            .unwrap()
            .unwrap_or_default();
        assert!(
            msg.starts_with("syntax error at source line 1: nesting too deep"),
            "{msg}"
        );
    }

    #[test]
    fn chains_fit_the_stack_of_a_default_thread() {
        // On a 2 MiB thread, whatever `parse` accepts must be safe to free
        // (which recurses over the tree's depth), and to run: running it may
        // fail, as the interpreter needs more stack per level, but with an
        // error, not a crash.  Longer chains must be a syntax error.
        let run = || {
            let mut cases = vec![];
            for n in [100, 1000, 3000, 10_000, 1_000_000] {
                for op in ["+1", " 1", "&&1", "~1", " in a"] {
                    cases.push((n, format!("BEGIN {{ x = 1{} }}", op.repeat(n))));
                }
                let (open, close) = ("(1+".repeat(n), ")".repeat(n));
                cases.push((n, format!("BEGIN {{ x = {open}1{close} }}")));
            }
            for (n, src) in cases {
                let head: String = src.chars().take(20).collect();
                match parse(src.as_bytes()) {
                    Ok(prog) => {
                        assert!(n <= 10_000, "{head} x {n} accepted");
                        let config = crate::interp::Config::default();
                        let mut interp = crate::interp::Interp::new(&prog, &config).unwrap();
                        if let Err(e) = interp.run() {
                            let e = e.to_string();
                            assert!(e.contains("expression nesting too deep"), "{e}");
                        }
                    }
                    Err(e) => assert!(e.to_string().contains("nesting too deep"), "{e}"),
                }
            }
        };
        std::thread::spawn(run).join().unwrap();
    }

    #[test]
    fn chains_inside_parentheses_add_up() {
        // `((1+1+...)+1+...)+1+...`: each level's chain starts from the
        // tree the parentheses gave it, so the depths add up, and so must
        // the links counted for them.  On a 2 MiB thread the whole tree
        // must stay within what is safe to free (CodeRabbit review).
        let run = || {
            for (levels, links) in [(10, 100), (10, 1000), (5, 1500), (40, 1000), (100, 100)] {
                let mut expr = String::from("1");
                for _ in 0..levels {
                    expr = format!("({expr}{})", "+1".repeat(links));
                }
                match parse(format!("BEGIN {{ x = {expr} }}").as_bytes()) {
                    Ok(_) => assert!(levels * links <= 2_048, "{levels} x {links} accepted"),
                    Err(e) => assert!(e.to_string().contains("nesting too deep"), "{e}"),
                }
            }
        };
        std::thread::spawn(run).join().unwrap();
    }

    #[test]
    fn link_counts_start_afresh_for_each_statement_and_pattern() {
        // Links are kept until the tree they belong to is finished: one
        // statement, or one pattern.  Many statements with long chains
        // are fine on a 2 MiB thread.
        let run = || {
            let chain = format!("x = 1{}", "+1".repeat(1_000));
            let body = vec![chain; 50].join("; ");
            let pattern = format!("$1 == 1{}", "+1".repeat(1_000));
            let src = format!("BEGIN {{ {body} }}\n{pattern}\n{pattern}, {pattern} {{ {body} }}");
            assert!(parse(src.as_bytes()).is_ok());
        };
        std::thread::spawn(run).join().unwrap();
    }

    #[test]
    fn long_operator_chains_are_an_error_not_a_crash() {
        // `1+1+1+...` is parsed by a loop, not recursion, but still builds
        // a tree as deep as the chain is long, and the interpreter (and
        // freeing the tree) recurse over that depth.  So chain links count
        // towards the nesting limit as well.
        let too_deep = "syntax error at source line 1: nesting too deep";
        let n = 1_000_000;
        let cases = [
            format!("BEGIN {{ x = 1{} }}", "+1".repeat(n)),
            format!("BEGIN {{ x = 1{} }}", "*1".repeat(n)),
            format!("BEGIN {{ x = 1{} }}", " 1".repeat(n)),
            format!("BEGIN {{ x = 1{} }}", "&&1".repeat(n)),
            format!("BEGIN {{ x = 1{} }}", "||1".repeat(n)),
            format!("BEGIN {{ x = 1{} }}", "~1".repeat(n)),
            format!("BEGIN {{ x = 1{} }}", " in a".repeat(n)),
            format!("BEGIN {{ x = 1{} }}", "|getline".repeat(n)),
            format!("BEGIN {{ x = 1{} }}", "+1*1".repeat(n)),
        ];
        for src in cases {
            let head: String = src.chars().take(30).collect();
            let msg = parse_on_small_stack(src).unwrap_or_default();
            assert!(msg.starts_with(too_deep), "{head}: {msg}");
        }
        // Long but sane chains are fine given hawk's big stack (pyawk fails
        // before 1,000 terms).
        let src = format!(
            "BEGIN {{ x = 1{}; y = 1{} }}",
            "+1".repeat(10_000),
            " 1".repeat(10_000)
        );
        let parsed = std::thread::Builder::new()
            .stack_size(1 << 30)
            .spawn(move || parse_with_stack(src.as_bytes(), 1 << 30).err())
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(parsed, None);
    }

    #[test]
    fn syntax_errors() {
        for bad in [
            "BEGIN {",
            "{ print ( }",
            "{ 1 = 2 }",
            "{ x y = 3 }",
            "BEGIN { if x }",
        ] {
            err(bad);
        }
    }

    #[test]
    fn error_messages_match_pyawk() {
        assert_eq!(
            err("BEGIN {"),
            "syntax error at source line 1: missing '}', found end of input"
        );
        assert_eq!(
            err("BEGIN { if x }"),
            "syntax error at source line 1: expected '(', found 'x'"
        );
        assert_eq!(
            err("{ 1 = 2 }"),
            "syntax error at source line 1: assignment to non-lvalue, found '='"
        );
        assert_eq!(
            err("{ print ( }"),
            "syntax error at source line 1: unexpected token, found '}'"
        );
        assert_eq!(
            err("{ x\n+ }"),
            "syntax error at source line 2: unexpected token, found '}'"
        );
        assert_eq!(
            err("BEGIN { a b c d"),
            "syntax error at source line 1: missing '}', found end of input"
        );
        assert_eq!(
            err("{ x ) }"),
            "syntax error at source line 1: expected ';' or newline, found ')'"
        );
        assert_eq!(
            err("function g() { } function f(g) { }"),
            "syntax error: function f: parameter g has the name of a function"
        );
        assert_eq!(
            err("BEGIN {\n break }"),
            "syntax error at source line 2: break is not inside a loop"
        );
        assert_eq!(
            err("{ x = \"it's\" ; ) }"),
            "syntax error at source line 1: unexpected token, found ')'"
        );
    }

    #[test]
    fn found_shows_python_style_quotes() {
        assert_eq!(
            err("{ print > \"it's\" \"x\" ) }")
                .rsplit(", ")
                .next()
                .unwrap(),
            "found ')'"
        );
        assert_eq!(py_repr(b"it's"), "\"it's\"");
        assert_eq!(py_repr(b"a\nb\\"), "'a\\nb\\\\'");
    }

    #[test]
    fn newlines_allowed_after_operators_and_in_parentheses() {
        assert_eq!(rhs("a ||\n b"), Expr::Or(b(v("a")), b(v("b"))));
        assert_eq!(rhs("(\n1\n)"), n(1.0));
        assert_eq!(
            rhs("c ?\n 1 :\n 2"),
            Expr::Cond(b(v("c")), b(n(1.0)), b(n(2.0)))
        );
        let _ = Rule {
            pattern: None,
            action: None,
            end: None,
        }; // the type is public
    }

    #[test]
    fn semicolons_and_empty_statements() {
        let prog = p(";;BEGIN { ; x ; ; }\n;");
        // Terminators between statements are skipped, so only x remains.
        assert_eq!(prog.begin[0], [Stmt::Expr(v("x"))]);
        // A lone `;` as a loop body is an empty block.
        assert!(
            matches!(stmt("while (x) ;"), Stmt::While { body, .. } if *body == Stmt::Block(vec![]))
        );
        // A ';' between `if` and `else` is fine.
        assert!(matches!(
            stmt("if (x) y; else z"),
            Stmt::If {
                orelse: Some(_),
                ..
            }
        ));
    }
}
