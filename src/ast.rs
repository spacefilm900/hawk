//! The abstract syntax tree: the program's structure with the punctuation
//! removed.  The parser builds it; later phases compile it into closures.
//!
//! For example `print $1 + 2` becomes
//! `Print { args: [Binary(Add, Field(Num(1)), Num(2))], .. }`.
//!
//! The node set mirrors pyawk v1.0's `ast.py` one for one, so the two trees
//! can be compared (see [`dump_program`] and `tools/ast_check.py`).
//!
//! Rust notes for this file:
//!
//! * An `enum` whose variants carry data is a *tagged union*: an `Expr` is
//!   exactly one of the variants listed, and `match` must handle them all.
//! * A recursive type needs a `Box`: `Box<Expr>` is a pointer to an `Expr`
//!   on the heap.  Without it an `Expr` would have to contain itself, and
//!   its size would be infinite.  A `Vec<Expr>` is already a heap pointer,
//!   so lists need no `Box`.
//! * `Option<T>` is "a T or nothing" (Python's `Optional`).

use crate::lexer::Builtin;

/// The binary arithmetic operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
}

impl BinOp {
    pub fn as_str(self) -> &'static str {
        // `&'static str` is a string literal baked into the program.
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Mod => "%",
            BinOp::Pow => "^",
        }
    }
}

/// `=` or a compound assignment such as `+=` (`Some(BinOp::Add)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssignOp(pub Option<BinOp>);

impl AssignOp {
    pub fn as_str(self) -> &'static str {
        match self.0 {
            None => "=",
            Some(BinOp::Add) => "+=",
            Some(BinOp::Sub) => "-=",
            Some(BinOp::Mul) => "*=",
            Some(BinOp::Div) => "/=",
            Some(BinOp::Mod) => "%=",
            Some(BinOp::Pow) => "^=",
        }
    }
}

/// The comparison operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Lt,
    Le,
    Eq,
    Ne,
    Gt,
    Ge,
}

impl CmpOp {
    pub fn as_str(self) -> &'static str {
        match self {
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Eq => "==",
            CmpOp::Ne => "!=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }
}

/// Unary `-`, `+` and `!`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Plus,
    Not,
}

impl UnOp {
    pub fn as_str(self) -> &'static str {
        match self {
            UnOp::Neg => "-",
            UnOp::Plus => "+",
            UnOp::Not => "!",
        }
    }
}

/// Where a `getline` reads from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GetlineKind {
    /// Plain `getline [var]`: the main input.
    Simple,
    /// `getline [var] < file`.
    File,
    /// `cmd | getline [var]`.
    Cmd,
}

/// An output redirection after `print`/`printf`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redirect {
    /// `> file`
    Write,
    /// `>> file`
    Append,
    /// `| command`
    Pipe,
}

impl Redirect {
    pub fn as_str(self) -> &'static str {
        match self {
            Redirect::Write => ">",
            Redirect::Append => ">>",
            Redirect::Pipe => "|",
        }
    }
}

/// An expression: a node that produces a value.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Num(f64),
    Str(Vec<u8>),
    /// An ERE literal.  As a plain expression it means `$0 ~ /re/`.
    Regex(Vec<u8>),
    Var(String),
    /// `name[subs...]` (several subscripts are joined with SUBSEP).
    Index(String, Vec<Expr>),
    /// `$expr`.
    Field(Box<Expr>),
    /// A parenthesised list `(a, b)`: only valid before `in` or as the
    /// argument list of print/printf.
    Group(Vec<Expr>),
    /// `target op value`; the target is a Var, Index or Field.
    Assign(AssignOp, Box<Expr>, Box<Expr>),
    /// `test ? if_true : if_false`.
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    /// `(subs) in array`.
    In(Vec<Expr>, String),
    /// `left ~ regex`, or `left !~ regex` when `negate` is true.
    /// Variants may also have *named* fields, like a struct.
    Match {
        negate: bool,
        left: Box<Expr>,
        regex: Box<Expr>,
    },
    Compare(CmpOp, Box<Expr>, Box<Expr>),
    /// Concatenation, written by juxtaposition: `a b`.
    Concat(Box<Expr>, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Unary(UnOp, Box<Expr>),
    /// `++x` / `x--`; `incr` false means `--`.
    IncDec {
        incr: bool,
        prefix: bool,
        target: Box<Expr>,
    },
    /// A builtin function call.
    Call(Builtin, Vec<Expr>),
    /// A call of a user-defined function, looked up by name when it runs.
    UserCall(String, Vec<Expr>),
    /// Every getline form.  `target` None means "set $0".
    Getline {
        kind: GetlineKind,
        target: Option<Box<Expr>>,
        source: Option<Box<Expr>>,
    },
}

impl Expr {
    /// Can this be assigned to (an "lvalue")?
    pub fn is_lvalue(&self) -> bool {
        // `matches!` is a `match` that answers true or false.
        matches!(self, Expr::Var(_) | Expr::Index(..) | Expr::Field(_))
    }
}

/// A statement: executed for its effect.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Block(Vec<Stmt>),
    Expr(Expr),
    /// `print args [> dest]`; with no args it prints `$0`.
    Print {
        args: Vec<Expr>,
        redirect: Option<(Redirect, Expr)>,
    },
    Printf {
        args: Vec<Expr>,
        redirect: Option<(Redirect, Expr)>,
    },
    If {
        test: Expr,
        then: Box<Stmt>,
        orelse: Option<Box<Stmt>>,
    },
    While {
        test: Expr,
        body: Box<Stmt>,
    },
    DoWhile {
        body: Box<Stmt>,
        test: Expr,
    },
    /// `for (init; test; step) body`.  `init` and `step` are *simple
    /// statements*: usually `Stmt::Expr`, but print/printf/delete are allowed.
    For {
        init: Option<Box<Stmt>>,
        test: Option<Expr>,
        step: Option<Box<Stmt>>,
        body: Box<Stmt>,
    },
    /// `for (var in array) body`.
    ForIn {
        var: String,
        array: String,
        body: Box<Stmt>,
    },
    Break,
    Continue,
    Next,
    /// `nextfile` (POSIX 2024): skip the rest of the current input file.
    NextFile,
    Exit(Option<Expr>),
    Return(Option<Expr>),
    /// `delete array[subs]`, or `delete array` (subs None) for all of it.
    Delete {
        array: String,
        subs: Option<Vec<Expr>>,
    },
}

/// `pattern { action }`.  A missing pattern matches every record; a missing
/// action means `{ print }`.  `end` is the second pattern of a range rule
/// `start, end { ... }`.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub pattern: Option<Expr>,
    pub action: Option<Vec<Stmt>>,
    pub end: Option<Expr>,
}

/// `function name(params) { body }`.  Extra parameters double as locals.
#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    pub name: String,
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
}

/// The whole program.  `functions` keeps definition order (pyawk's dict
/// does too); `warnings` are printed on stderr before the program runs.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Program {
    pub begin: Vec<Vec<Stmt>>,
    pub rules: Vec<Rule>,
    pub end: Vec<Vec<Stmt>>,
    pub functions: Vec<Function>,
    pub warnings: Vec<String>,
}

impl Program {
    /// The function called `name`, if defined.
    pub fn function(&self, name: &str) -> Option<&Function> {
        self.functions.iter().find(|f| f.name == name)
    }
}

// ------------------------------------------------------------------ dump
//
// A canonical text form of the tree, written the same way by
// `tools/ast_dump.py` for pyawk's tree, so the two parsers can be compared
// program by program.  Format: `Node(field, field, ...)` with fields in
// pyawk's order; strings quoted with `\xNN` escapes; numbers as integers
// when integral, else as the hex bits of the double; `None`, `True`, `False`.

/// The canonical dump of a whole program (see `hawk --dump-ast`).
pub fn dump_program(p: &Program) -> String {
    let mut d = Dumper(String::new());
    d.program(p);
    d.0
}

/// A *tuple struct* wrapping the output string; `d.0` is its one field.
struct Dumper(String);

impl Dumper {
    fn w(&mut self, s: &str) {
        self.0.push_str(s);
    }

    fn quoted(&mut self, bytes: &[u8]) {
        self.w("\"");
        for &b in bytes {
            match b {
                b'"' => self.w("\\\""),
                b'\\' => self.w("\\\\"),
                0x20..=0x7e => self.0.push(b as char),
                _ => self.w(&format!("\\x{b:02x}")),
            }
        }
        self.w("\"");
    }

    fn num(&mut self, v: f64) {
        if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e15 {
            self.w(&format!("{}", v as i64));
        } else {
            self.w(&format!("f:{:016x}", v.to_bits()));
        }
    }

    /// `[a, b, c]`, writing each item with `f`.  `impl Fn(...)` accepts any
    /// closure (an inline function such as `|d, e| d.expr(e)`).
    fn list<T>(&mut self, items: &[T], f: impl Fn(&mut Self, &T)) {
        self.w("[");
        for (i, it) in items.iter().enumerate() {
            if i > 0 {
                self.w(", ");
            }
            f(self, it);
        }
        self.w("]");
    }

    fn opt<T>(&mut self, item: Option<&T>, f: impl Fn(&mut Self, &T)) {
        match item {
            None => self.w("None"),
            Some(x) => f(self, x),
        }
    }

    fn node(&mut self, name: &str, fields: impl FnOnce(&mut Self)) {
        self.w(name);
        self.w("(");
        fields(self);
        self.w(")");
    }

    fn exprs(&mut self, es: &[Expr]) {
        self.list(es, |d, e| d.expr(e));
    }

    fn block(&mut self, body: &[Stmt]) {
        self.node("Block", |d| d.list(body, |d, s| d.stmt(s)));
    }

    fn program(&mut self, p: &Program) {
        self.node("Program", |d| {
            d.list(&p.begin, |d, b| d.block(b));
            d.w(", ");
            d.list(&p.rules, |d, r| {
                d.node("Rule", |d| {
                    d.opt(r.pattern.as_ref(), |d, e| d.expr(e));
                    d.w(", ");
                    d.opt(r.action.as_ref(), |d, b| d.block(b));
                    d.w(", ");
                    d.opt(r.end.as_ref(), |d, e| d.expr(e));
                })
            });
            d.w(", ");
            d.list(&p.end, |d, b| d.block(b));
            d.w(", ");
            d.list(&p.functions, |d, f| {
                d.node("Function", |d| {
                    d.quoted(f.name.as_bytes());
                    d.w(", ");
                    d.list(&f.params, |d, s| d.quoted(s.as_bytes()));
                    d.w(", ");
                    d.block(&f.body);
                })
            });
            d.w(", ");
            d.list(&p.warnings, |d, s| d.quoted(s.as_bytes()));
        });
    }

    /// The print/printf redirect as pyawk's two fields: `redirect, dest`.
    fn redirect(&mut self, r: &Option<(Redirect, Expr)>) {
        match r {
            None => self.w("None, None"),
            Some((kind, dest)) => {
                self.quoted(kind.as_str().as_bytes());
                self.w(", ");
                self.expr(dest);
            }
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match s {
            Stmt::Block(b) => self.block(b),
            Stmt::Expr(e) => self.node("ExprStmt", |d| d.expr(e)),
            Stmt::Print { args, redirect } => self.node("Print", |d| {
                d.exprs(args);
                d.w(", ");
                d.redirect(redirect);
            }),
            Stmt::Printf { args, redirect } => self.node("Printf", |d| {
                d.exprs(args);
                d.w(", ");
                d.redirect(redirect);
            }),
            Stmt::If { test, then, orelse } => self.node("If", |d| {
                d.expr(test);
                d.w(", ");
                d.stmt(then);
                d.w(", ");
                d.opt(orelse.as_deref(), |d, s| d.stmt(s));
            }),
            Stmt::While { test, body } => self.node("While", |d| {
                d.expr(test);
                d.w(", ");
                d.stmt(body);
            }),
            Stmt::DoWhile { body, test } => self.node("DoWhile", |d| {
                d.stmt(body);
                d.w(", ");
                d.expr(test);
            }),
            Stmt::For {
                init,
                test,
                step,
                body,
            } => self.node("For", |d| {
                d.opt(init.as_deref(), |d, s| d.for_clause(s));
                d.w(", ");
                d.opt(test.as_ref(), |d, e| d.expr(e));
                d.w(", ");
                d.opt(step.as_deref(), |d, s| d.for_clause(s));
                d.w(", ");
                d.stmt(body);
            }),
            Stmt::ForIn { var, array, body } => self.node("ForIn", |d| {
                d.node("Var", |d| d.quoted(var.as_bytes()));
                d.w(", ");
                d.quoted(array.as_bytes());
                d.w(", ");
                d.stmt(body);
            }),
            Stmt::Break => self.w("Break()"),
            Stmt::Continue => self.w("Continue()"),
            Stmt::Next => self.w("Next()"),
            Stmt::NextFile => self.w("NextFile()"),
            Stmt::Exit(e) => self.node("Exit", |d| d.opt(e.as_ref(), |d, e| d.expr(e))),
            Stmt::Return(e) => self.node("Return", |d| d.opt(e.as_ref(), |d, e| d.expr(e))),
            Stmt::Delete { array, subs } => self.node("Delete", |d| {
                d.quoted(array.as_bytes());
                d.w(", ");
                d.opt(subs.as_ref(), |d, s| d.exprs(s));
            }),
        }
    }

    /// pyawk keeps a `for` clause expression bare, not wrapped in ExprStmt.
    fn for_clause(&mut self, s: &Stmt) {
        match s {
            Stmt::Expr(e) => self.expr(e),
            other => self.stmt(other),
        }
    }

    fn bin(&mut self, name: &str, op: &str, l: &Expr, r: &Expr) {
        self.node(name, |d| {
            d.quoted(op.as_bytes());
            d.w(", ");
            d.expr(l);
            d.w(", ");
            d.expr(r);
        });
    }

    fn pair(&mut self, name: &str, l: &Expr, r: &Expr) {
        self.node(name, |d| {
            d.expr(l);
            d.w(", ");
            d.expr(r);
        });
    }

    fn expr(&mut self, e: &Expr) {
        match e {
            Expr::Num(v) => self.node("Num", |d| d.num(*v)),
            Expr::Str(s) => self.node("Str", |d| d.quoted(s)),
            Expr::Regex(s) => self.node("Regex", |d| d.quoted(s)),
            Expr::Var(n) => self.node("Var", |d| d.quoted(n.as_bytes())),
            Expr::Index(n, subs) => self.node("Index", |d| {
                d.quoted(n.as_bytes());
                d.w(", ");
                d.exprs(subs);
            }),
            Expr::Field(i) => self.node("Field", |d| d.expr(i)),
            Expr::Group(es) => self.node("Group", |d| d.exprs(es)),
            Expr::Assign(op, t, v) => self.bin("Assign", op.as_str(), t, v),
            Expr::Cond(t, a, b) => self.node("Cond", |d| {
                d.expr(t);
                d.w(", ");
                d.expr(a);
                d.w(", ");
                d.expr(b);
            }),
            Expr::Or(l, r) => self.pair("Or", l, r),
            Expr::And(l, r) => self.pair("And", l, r),
            Expr::In(subs, a) => self.node("In", |d| {
                d.exprs(subs);
                d.w(", ");
                d.quoted(a.as_bytes());
            }),
            Expr::Match {
                negate,
                left,
                regex,
            } => self.node("Match", |d| {
                d.w(if *negate { "True, " } else { "False, " });
                d.expr(left);
                d.w(", ");
                d.expr(regex);
            }),
            Expr::Compare(op, l, r) => self.bin("Compare", op.as_str(), l, r),
            Expr::Concat(l, r) => self.pair("Concat", l, r),
            Expr::Binary(op, l, r) => self.bin("Binary", op.as_str(), l, r),
            Expr::Unary(op, x) => self.node("Unary", |d| {
                d.quoted(op.as_str().as_bytes());
                d.w(", ");
                d.expr(x);
            }),
            Expr::IncDec {
                incr,
                prefix,
                target,
            } => self.node("IncDec", |d| {
                d.w(if *incr { "\"++\", " } else { "\"--\", " });
                d.w(if *prefix { "True, " } else { "False, " });
                d.expr(target);
            }),
            Expr::Call(b, args) => self.node("Call", |d| {
                d.quoted(b.name().as_bytes());
                d.w(", ");
                d.exprs(args);
            }),
            Expr::UserCall(n, args) => self.node("UserCall", |d| {
                d.quoted(n.as_bytes());
                d.w(", ");
                d.exprs(args);
            }),
            Expr::Getline {
                kind,
                target,
                source,
            } => self.node("Getline", |d| {
                d.w(match kind {
                    GetlineKind::Simple => "\"simple\", ",
                    GetlineKind::File => "\"file\", ",
                    GetlineKind::Cmd => "\"cmd\", ",
                });
                d.opt(target.as_deref(), |d, e| d.expr(e));
                d.w(", ");
                d.opt(source.as_deref(), |d, e| d.expr(e));
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_format() {
        let p = Program {
            rules: vec![Rule {
                pattern: Some(Expr::Regex(b"a\"\xff".to_vec())),
                action: Some(vec![Stmt::Print {
                    args: vec![Expr::Binary(
                        BinOp::Add,
                        Box::new(Expr::Num(1.0)),
                        Box::new(Expr::Num(0.5)),
                    )],
                    redirect: Some((Redirect::Append, Expr::Str(b"f".to_vec()))),
                }]),
                end: None,
            }],
            ..Program::default() // the remaining fields take their defaults
        };
        assert_eq!(
            dump_program(&p),
            "Program([], [Rule(Regex(\"a\\\"\\xff\"), Block([Print([Binary(\"+\", Num(1), \
             Num(f:3fe0000000000000))], \">>\", Str(\"f\"))]), None)], [], [], [])"
        );
    }
}
