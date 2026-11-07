//! The interpreter: runs a parsed program (pyawk's `runtime.py` and
//! `compiler.py` together).
//!
//! hawk walks the syntax tree directly (a "tree-walking interpreter"):
//! `exec` runs a statement, `eval` computes an expression.  pyawk compiles
//! the tree to Python closures first; in Rust a direct walk is already fast
//! and much easier to read.  (DESIGN.md, "Execution model".)
//!
//! **Control flow.**  `break`, `continue`, `next` and `return` are ordinary
//! results: `exec` returns a `Flow` saying how the statement ended, and
//! loops, rules and function calls look at it.  `exit` and runtime errors
//! must also escape from inside expressions (a function called in an
//! expression may `exit`), so they travel as the error side of `Result`,
//! the `Unwind` type, and `?` passes them up automatically.
//!
//! **Lifetimes.**  `Interp<'p>` borrows the program for the lifetime `'p`
//! (read "the interpreter cannot outlive the program it runs").  That lets
//! it keep references into the tree, such as a function's parameter names,
//! without copying them.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::rc::Rc;

use crate::array::{Array, FxMap, Key};
use crate::ast::{BinOp, CmpOp, Expr, Function, GetlineKind, Program, Redirect, Stmt, UnOp};
use crate::charset;
use crate::error::Error;
use crate::format::{CompiledFormat, FmtArg, compile_format, format_compiled};
use crate::input::{RecordReader, RegexFinder};
use crate::io::{Io, strerror};
use crate::lexer::{Builtin, process_escapes};
use crate::rand::Random;
use crate::regex::{Finder, Regex, Transcoding, nfa_limit};
use crate::subst::{split_regex, split_regex_spans, substitute};
use crate::value::{Value, compare, empty, looks_numeric, num_to_str, str_to_num};

/// An array shared by reference: `Rc` lets several owners (a global and a
/// function parameter) hold the same array; `RefCell` lets us change it
/// through any of them.  (Rust otherwise allows changes only through a
/// single owner; `RefCell` moves that check to run time.)
pub type ArrayRef = Rc<RefCell<Array>>;

/// Stack kept free below the recursion limit: room for the deepest
/// expression evaluation plus error reporting after the limit is hit.
const STACK_MARGIN: usize = 4 << 20;

/// The address of a local variable: how far down the stack we are.  (The
/// stack grows toward lower addresses on every platform Rust supports for
/// hawk.)  Turning a reference into a number is safe; only *using* such a
/// number as a pointer would need `unsafe`.
#[inline(never)]
pub(crate) fn stack_position() -> usize {
    let marker = 0u8;
    std::hint::black_box(&marker) as *const u8 as usize
}

/// A global variable once its kind is known.  A name missing from the map
/// is untyped: it may still become either.
enum Var {
    Scalar(Value),
    Array(ArrayRef),
}

/// Where an untyped function parameter came from, so that if the function
/// uses it as an array the caller's variable becomes that same array.
#[derive(Clone)]
enum Link {
    Global(String),
    /// Frame number and slot of a caller's untyped local.
    Local(usize, usize),
}

/// One local variable (parameter) of a running function call.
enum Cell {
    Untyped(Option<Link>),
    Scalar(Value),
    Array(ArrayRef),
}

/// A running function call.
struct Frame<'p> {
    params: &'p [String],
    cells: Vec<Cell>,
}

/// How a statement finished, when it finished normally.
#[derive(Debug, PartialEq)]
enum Flow {
    Normal,
    Break,
    Continue,
    Next,
    /// `nextfile`.  It can only come from a rule's own action (the parser
    /// rejects it in functions, BEGIN and END), so unlike `next` it never
    /// has to unwind through a function call.
    NextFile,
    /// The value is in `Interp::ret`.
    Return,
}

/// Why evaluation stopped early: an `exit` statement (the status is in
/// `Interp::exit_code`), a `next` executed inside a function, or an error.
///
/// `next` in a rule's own action is an ordinary `Flow::Next`, but inside a
/// function the call may sit in the middle of an expression (`x = 1 + f()`),
/// so it has to unwind through `eval` like `exit` does; the record loop
/// catches it.
#[derive(Debug)]
pub enum Unwind {
    Exit,
    Next,
    Err(Error),
}

/// `From` lets `?` turn an `Error` into an `Unwind` automatically.
impl From<Error> for Unwind {
    fn from(e: Error) -> Unwind {
        Unwind::Err(e)
    }
}

/// Our `Result`: success with a `T`, or an `Unwind`.
type R<T> = Result<T, Unwind>;

/// A runtime error as an `Unwind`.
fn rt<T>(msg: impl Into<String>) -> R<T> {
    Err(Unwind::Err(Error::Runtime(msg.into())))
}

/// Something that can be assigned to (an "lvalue").
enum Place<'p> {
    Global(&'p str),
    Local(usize),
    /// `NF` itself (assigning it rebuilds the record).
    Nf,
    Elem(ArrayRef, Key),
    Field(usize),
}

/// The main input: which reader the record loop is reading.
enum Main {
    /// Not opened yet, or the last file ended.
    None,
    Stdin,
    File(RecordReader),
}

/// Most compiled regexes kept in the cache (pyawk's CACHE_ENTRIES).
const REGEX_CACHE_ENTRIES: usize = 512;

/// The fields of the current record.
///
/// Splitting only records where each field sits inside `Interp::record`.
/// The `Value` (a reference-counted string, and the parsed number if the
/// text looks numeric) is built the first time something needs the field
/// *as a value*: printing it, assigning it, using it as an array key.
/// Arithmetic never does that.  `s += $2` parses the number straight off
/// the record bytes and stores it in `nums`, so a tight sum does not
/// allocate one string per field per line.
struct Fields {
    spans: Vec<(usize, usize)>,
    values: Vec<Option<Value>>,
    nums: Vec<Option<f64>>,
    /// `spans`/`values` describe this record.  False until `$n` or NF is
    /// used; assigning `$0` clears it so the next use splits again.
    ready: bool,
    /// A field was assigned (or NF was).  The values are then the source
    /// of truth and `spans` may no longer match `record`, because `$0`
    /// was rebuilt by joining the values with OFS.
    owned: bool,
}

/// The settings that come from the command line.
#[derive(Default)]
pub struct Config {
    /// Operands after the program: input files and `var=value`.
    pub operands: Vec<Vec<u8>>,
    /// `-v var=value` and `-F fs` (as `FS=fs`), applied before BEGIN.
    pub assignments: Vec<(Vec<u8>, Vec<u8>)>,
    /// Size of the stack the interpreter runs on (0: unknown, so assume
    /// 2 MiB, the smallest a thread usually has: Rust gives the threads it
    /// spawns 2 MiB, and the main thread usually has 8 MiB).  Recursion stops with an error, rather
    /// than a crash, before it runs out.
    pub stack_size: usize,
    /// UTF-8 mode (see `charset`): characters are UTF-8 sequences, not
    /// bytes.  The command line sets it from the locale variables.
    pub utf8: bool,
}

pub struct Interp<'p> {
    prog: &'p Program,
    globals: FxMap<String, Var>,
    frames: Vec<Frame<'p>>,
    /// The value of the last `return`.
    ret: Value,
    exit_code: i32,
    io: Io,

    // ---- the current record
    /// The current record's bytes.  One buffer, reused for every record:
    /// allocating a fresh string per line was most of what was left of the
    /// record loop after the fields stopped allocating.  A value that
    /// escapes (`x = $0`) copies out of here, so the next record can
    /// overwrite the buffer.
    record: Vec<u8>,
    /// The other half of the record buffer.  The next record is read into
    /// this, and the two are swapped only when the read succeeds, so a
    /// failed read (end of input) does not wipe `$0`.
    record_swap: Vec<u8>,
    /// `$0` as a value, made on first use (a strnum unless it was assigned
    /// a string or `sub`/`gsub` replaced something).
    record_value: Option<Value>,
    /// `$0` must come back as a plain string, not a strnum, even when the
    /// bytes look numeric.  Set by assigning a string to `$0` and by a
    /// substitution that changed it.
    record_is_string: bool,
    /// The fields, split on first use (lazy splitting).
    fields: Fields,
    /// FS when the record was read: it applies to this record even if the
    /// program changes FS before splitting happens.
    record_fs: Rc<[u8]>,

    // ---- the main input
    main: Main,
    /// Next ARGV index to look at.
    argi: usize,
    opened_any: bool,
    /// One "inside the range" flag per rule (used by `start, end` rules).
    range_open: Vec<bool>,

    rng: Random,
    seed: f64,

    /// Compiled regexes by source text, for /literals/ and dynamic ones
    /// alike.  Capped like pyawk's cache: at most REGEX_CACHE_ENTRIES
    /// entries and NFA_LIMIT NFA states in all (four times that in UTF-8
    /// mode); when full it starts over.
    regexes: FxMap<Vec<u8>, Rc<Regex>>,
    regex_states: u64,

    /// `stack_position()` when the run started, and how much stack the
    /// interpreter may use below it.
    stack_base: usize,
    stack_budget: usize,
    /// How much stack the interpreter's own recursion over the program
    /// (expressions, conditions, statements) may use: halfway between the
    /// function-call budget and the end of the stack, so runaway recursion
    /// is still reported as such, and there is room left to report it.
    eval_limit: usize,

    /// UTF-8 mode (from `Config::utf8`).
    utf8: bool,

    /// Built-in scalars the record loop reads or writes on every line
    /// (FS, RS, NR, ...).  They are kept here instead of in `globals` so
    /// that touching them is a field access, not a hash lookup, and so
    /// `NR++` does not allocate a fresh key.  Every other global still
    /// lives in the map.  A name is in at most one of the two places.
    fs: Value,
    rs: Value,
    ofs: Value,
    ors: Value,
    ofmt: Value,
    convfmt_v: Value,
    subsep: Value,
    nr: Value,
    fnr: Value,

    /// Scratch for an array subscript, reused every time so `c[$1]++`
    /// does not allocate a key just to look the element up.  Grown to the
    /// longest key seen and then kept.
    key_buf: Vec<u8>,
    /// The global array named by the bytes at `arr_ptr` (an AST string,
    /// stable for the whole run).  One program refers to the same array
    /// from the same expression on every record; the pointer compare
    /// skips the hash lookup of the name.
    arr_ptr: *const u8,
    arr_len: usize,
    arr_cache: Option<ArrayRef>,

    /// Buffer recycled across `printf` lines, so each line is not a fresh
    /// allocation.  Taken out for the duration of one `printf` (an
    /// argument may itself `printf`), then put back.
    spare: Vec<u8>,
    /// Compiled form of the format string whose bytes live at `fmt_ptr`
    /// (a constant in the program).  The pointer is stable for the run.
    fmt_ptr: *const u8,
    fmt_len: usize,
    fmt_compiled: CompiledFormat,

    /// The last ordinary global read or written, kept here so `s += $2`
    /// does not hash the name on every record.  `sc_name` is the same
    /// letters, remembered so the value can be written back to `globals`
    /// when a *different* expression (a different copy of the name in the
    /// syntax tree) uses it.  Specials and arrays are not cached.
    sc_on: bool,
    sc_ptr: *const u8,
    sc_len: usize,
    sc_name: String,
    sc_val: Value,
}

/// True for `name=value` operands: the name is an AWK identifier.
fn assignment_operand(arg: &[u8]) -> Option<(&[u8], &[u8])> {
    let eq = arg.iter().position(|&b| b == b'=')?;
    let name = &arg[..eq];
    let first_ok = name
        .first()
        .is_some_and(|&b| b.is_ascii_alphabetic() || b == b'_');
    let rest_ok = name.iter().all(|&b| b.is_ascii_alphanumeric() || b == b'_');
    (first_ok && rest_ok).then(|| (name, &arg[eq + 1..]))
}

/// The number formatted as an array subscript or a string with CONVFMT.
fn bytes_to_string(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

impl<'p> Interp<'p> {
    /// The stack size from `config`.  0 means unknown: assume 2 MiB, what
    /// Rust gives the threads it spawns (see `Config::stack_size`).
    fn stack_size(config: &Config) -> usize {
        match config.stack_size {
            0 => 2 << 20,
            n => n,
        }
    }

    /// How much stack user function calls may use.  On a small stack
    /// (under 8 MiB) the margin would eat it all, so the budget is then
    /// half the stack instead.
    fn stack_budget(config: &Config) -> usize {
        let stack_size = Self::stack_size(config);
        stack_size.saturating_sub(STACK_MARGIN).max(stack_size / 2)
    }

    /// Stop with an error, not a crash, when evaluating a very deep
    /// expression (or statement) would run out of stack.  The parser
    /// bounds how deep the program's tree is, but cannot know the stack
    /// the interpreter will be given, nor how much each level takes
    /// (about 1 KiB in a release build, several in a debug build).
    fn check_stack(&self) -> R<()> {
        if self.stack_base.saturating_sub(stack_position()) > self.eval_limit {
            return rt("expression nesting too deep");
        }
        Ok(())
    }

    /// Set up an interpreter for `prog`: the special variables, ARGV,
    /// ENVIRON and the `-v` assignments from `config`.  Nothing runs yet.
    pub fn new(prog: &'p Program, config: &Config) -> R<Interp<'p>> {
        let mut it = Interp {
            prog,
            globals: FxMap::default(),
            frames: Vec::new(),
            ret: Value::Uninit,
            exit_code: 0,
            io: Io::new(),
            record: Vec::new(),
            record_swap: Vec::new(),
            record_value: None,
            record_is_string: false,
            fields: Fields {
                spans: Vec::new(),
                values: Vec::new(),
                nums: Vec::new(),
                ready: false,
                owned: false,
            },
            record_fs: Rc::from(&b" "[..]),
            main: Main::None,
            argi: 1,
            opened_any: false,
            range_open: vec![false; prog.rules.len()],
            rng: Random::new(0.0),
            seed: 0.0,
            regexes: FxMap::default(),
            regex_states: 0,
            stack_base: stack_position(),
            // 0: the stack size is unknown, so assume 2 MiB, what Rust
            // gives the threads it spawns (see `Config::stack_size`).
            // On a small stack (under 8 MiB) the margin would eat it all, so
            // the budget is then half the stack instead.
            stack_budget: Self::stack_budget(config),
            eval_limit: {
                let budget = Self::stack_budget(config);
                budget + (Self::stack_size(config) - budget) / 2
            },
            utf8: config.utf8,
            // POSIX defaults.  Strings, not numbers: their text does not
            // depend on CONVFMT (a numeric FS would; see `special_str`).
            fs: Value::str(b" "),
            rs: Value::str(b"\n"),
            ofs: Value::str(b" "),
            ors: Value::str(b"\n"),
            ofmt: Value::str(b"%.6g"),
            convfmt_v: Value::str(b"%.6g"),
            subsep: Value::str(b"\x1c"),
            nr: Value::Num(0.0),
            fnr: Value::Num(0.0),
            key_buf: Vec::new(),
            arr_ptr: std::ptr::null(),
            arr_len: 0,
            arr_cache: None,
            spare: Vec::new(),
            fmt_ptr: std::ptr::null(),
            fmt_len: 0,
            fmt_compiled: CompiledFormat::default(),
            sc_on: false,
            sc_ptr: std::ptr::null(),
            sc_len: 0,
            sc_name: String::new(),
            sc_val: Value::Uninit,
        };
        let s = |t: &str| Var::Scalar(Value::str(t.as_bytes()));
        let n = |x: f64| Var::Scalar(Value::Num(x));
        for (name, v) in [
            ("RSTART", n(0.0)),
            ("RLENGTH", n(-1.0)),
            ("FILENAME", s("")),
            ("ARGC", n(config.operands.len() as f64 + 1.0)),
        ] {
            it.globals.insert(name.to_string(), v);
        }
        let mut argv = Array::new();
        argv.set(&Rc::from(&b"0"[..]), Value::str(b"awk"));
        for (i, op) in config.operands.iter().enumerate() {
            argv.set(&Rc::from((i + 1).to_string().as_bytes()), Value::strnum(op));
        }
        it.globals
            .insert("ARGV".into(), Var::Array(Rc::new(RefCell::new(argv))));
        let mut env = Array::new();
        for (k, v) in std::env::vars_os() {
            use std::os::unix::ffi::OsStrExt;
            env.set(&Rc::from(k.as_bytes()), Value::strnum(v.as_bytes()));
        }
        it.globals
            .insert("ENVIRON".into(), Var::Array(Rc::new(RefCell::new(env))));
        for (name, value) in &config.assignments {
            it.command_line_assign(name, value)?;
        }
        Ok(it)
    }

    /// `-v`, `-F` and `var=value` operands: escapes are processed and the
    /// value is a strnum (so `-v n=10` compares as a number).
    fn command_line_assign(&mut self, name: &[u8], value: &[u8]) -> R<()> {
        let v = Value::strnum(&process_escapes(value));
        let name = bytes_to_string(name);
        self.set_global(&name, v)
    }

    // ================================================================ run

    /// Run the program: BEGIN, the record loop, END.  Returns the exit
    /// status.  `exit` in BEGIN or a rule skips the rest of the input but
    /// still runs END; `exit` in END stops at once.
    pub fn run(&mut self) -> Result<i32, Error> {
        self.stack_base = stack_position();
        let result = self.run_inner();
        // Close files and pipes, flush stdout, whatever happened.
        let closed = self.io.close_all();
        match result {
            Ok(()) | Err(Unwind::Exit) => {
                closed?;
                Ok(self.exit_code)
            }
            Err(Unwind::Err(e)) => Err(e),
            // run_special and main_loop catch every `next`.
            Err(Unwind::Next) => Err(Error::Runtime("next outside a rule".into())),
        }
    }

    /// BEGIN, the main loop and END, with `exit` handled as POSIX says:
    /// `exit` in BEGIN or a rule still runs END; `exit` in END stops.
    fn run_inner(&mut self) -> R<()> {
        let prog = self.prog;
        let body = (|| -> R<()> {
            for block in &prog.begin {
                self.run_special(block, "BEGIN")?;
            }
            if !prog.rules.is_empty() || !prog.end.is_empty() {
                self.main_loop()?;
            }
            Ok(())
        })();
        match body {
            Ok(()) | Err(Unwind::Exit) => {}
            Err(e) => return Err(e),
        }
        for block in &prog.end {
            self.run_special(block, "END")?;
        }
        Ok(())
    }

    /// Run one BEGIN (`which` is "BEGIN") or END block.  The parser rejects
    /// `next` written directly in it, but a function called from it may
    /// still execute one; with no record to skip, that is a fatal error
    /// (as in gawk).
    fn run_special(&mut self, block: &'p [Stmt], which: &str) -> R<()> {
        match self.exec_block(block) {
            Err(Unwind::Next) => rt(format!("next called from a function in {which}")),
            Err(e) => Err(e),
            Ok(_) => Ok(()),
        }
    }

    /// Read every main input record and run the pattern-action rules on
    /// it (`next` and `nextfile` end the rules early).
    fn main_loop(&mut self) -> R<()> {
        while self.next_main_record()? {
            self.bump("NR");
            self.bump("FNR");
            self.note_record();
            match self.run_rules() {
                // `next` inside a function called from a pattern or an
                // action: abandon this record's rules just the same.
                Ok(()) | Err(Unwind::Next) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Run every main rule on the current record, stopping at `next`.
    fn run_rules(&mut self) -> R<()> {
        let prog = self.prog;
        for (i, rule) in prog.rules.iter().enumerate() {
            let selected = match (&rule.pattern, &rule.end) {
                (Some(start), Some(end)) => self.in_range(i, start, end)?,
                (Some(p), None) => self.cond(p)?,
                (None, _) => true,
            };
            if !selected {
                continue;
            }
            match &rule.action {
                None => {
                    let mut line = self.record.to_vec();
                    line.extend_from_slice(&self.var_str("ORS"));
                    self.io.write_stdout(&line)?;
                }
                Some(body) => match self.exec_block(body)? {
                    Flow::Next => break,
                    Flow::NextFile => {
                        self.skip_file();
                        break;
                    }
                    _ => {}
                },
            }
        }
        Ok(())
    }

    /// Range rule `start, end`: once `start` matches, every record up to and
    /// including the one matching `end` is selected.
    fn in_range(&mut self, i: usize, start: &'p Expr, end: &'p Expr) -> R<bool> {
        if !self.range_open[i] && !self.cond(start)? {
            return Ok(false);
        }
        self.range_open[i] = !self.cond(end)?;
        Ok(true)
    }

    /// Add one to NR or FNR.  The result of the addition is a number, so
    /// a string previously stored there (`NR = "hello"`) is replaced by
    /// its numeric value plus one, which is what `NR = NR + 1` does.
    fn bump(&mut self, name: &str) {
        let Some(slot) = self.special_slot_mut(name) else {
            // Only NR and FNR are bumped.  Kept so a future caller still
            // does the obvious thing.
            let n = self.var_num(name) + 1.0;
            let _ = self.set_global(name, Value::Num(n));
            return;
        };
        let n = slot.to_num() + 1.0;
        *slot = Value::Num(n);
    }

    // ============================================================ input

    /// Read the next main-input record into `out` (cleared by the reader).
    /// `Ok(false)` when the input is finished.  Does not touch `$0`: the
    /// caller decides whether this record replaces it (`getline var` must
    /// not).
    fn read_main(&mut self, out: &mut Vec<u8>) -> R<bool> {
        loop {
            if let Main::None = self.main
                && !self.open_next_input()?
            {
                return Ok(false);
            }
            let rs = self.var_str("RS");
            let re = self.rs_regex(&rs)?;
            let find = |t: &[u8], p: usize, c: &mut Transcoding| {
                re.as_deref().and_then(|r| Finder::cached(r, t, c).find(p))
            };
            let finder: Option<RegexFinder> = re.is_some().then_some(&find);
            let got = match &mut self.main {
                Main::Stdin => self.io.stdin_reader().read_record_into(&rs, finder, out)?,
                Main::File(r) => r.read_record_into(&rs, finder, out)?,
                Main::None => false,
            };
            if got {
                return Ok(true);
            }
            self.main = Main::None;
        }
    }

    /// Read the next record into `self.record`.  `Ok(false)` at the end.
    fn next_main_record(&mut self) -> R<bool> {
        let mut rec = std::mem::take(&mut self.record_swap);
        let got = self.read_main(&mut rec)?;
        if got {
            self.record_swap = std::mem::replace(&mut self.record, rec);
        } else {
            self.record_swap = rec;
        }
        Ok(got)
    }

    /// `nextfile`: drop the rest of the current input file, so the next
    /// record comes from the next operand (and FNR starts again at 1).
    /// Standard input cannot be reopened, so skipping it uses it up: a
    /// later "-" operand or `getline < "-"` finds nothing, as in mawk and
    /// gawk (onetrue-awk carries on where it stopped).
    fn skip_file(&mut self) {
        // `std::mem::replace` puts `Main::None` in and hands back the old
        // value; dropping a `Main::File` closes the file.
        if let Main::Stdin = std::mem::replace(&mut self.main, Main::None) {
            self.io.stdin_reader().use_up();
        }
    }

    /// Move through ARGV to the next input file.  ARGV is read as we go,
    /// since BEGIN may have changed it.  Empty entries are skipped and
    /// `var=value` entries assigned; with no file operands, stdin is read.
    fn open_next_input(&mut self) -> R<bool> {
        let argv = self.global_array("ARGV")?;
        while (self.argi as f64) < self.var_num("ARGC") {
            let key = self.argi.to_string();
            self.argi += 1;
            let arg = match argv.borrow().get(key.as_bytes()) {
                Some(v) => v.clone(),
                None => continue,
            };
            let arg = self.to_str(&arg);
            if arg.is_empty() {
                continue;
            }
            if let Some((name, value)) = assignment_operand(&arg) {
                self.command_line_assign(name, value)?;
                continue;
            }
            self.opened_any = true;
            if &arg[..] == b"-" || &arg[..] == b"/dev/stdin" {
                self.main = Main::Stdin;
            } else {
                use std::os::unix::ffi::OsStrExt;
                let path = std::ffi::OsStr::from_bytes(&arg);
                // A directory opens fine on Unix but cannot be read, so
                // it is reported here as "cannot open" (EISDIR = 21).
                let opened = std::fs::File::open(path).and_then(|f| match f.metadata() {
                    Ok(m) if m.is_dir() => Err(std::io::Error::from_raw_os_error(21)),
                    _ => Ok(f),
                });
                match opened {
                    Ok(f) => self.main = Main::File(RecordReader::new(Box::new(f))),
                    Err(e) => {
                        return rt(format!(
                            "cannot open {} ({})",
                            bytes_to_string(&arg),
                            strerror(&e)
                        ));
                    }
                }
            }
            self.set_global("FILENAME", Value::Str(arg))?;
            self.set_global("FNR", Value::Num(0.0))?;
            return Ok(true);
        }
        if !self.opened_any {
            // No file operand was processed (there were none, or only
            // var=value assignments), so read standard input.  POSIX.1-2024
            // says awk "shall set FILENAME to '-'" in this case, as if "-"
            // had been given; mawk and gawk do the same.
            self.opened_any = true;
            self.main = Main::Stdin;
            self.set_global("FILENAME", Value::str(b"-"))?;
            self.set_global("FNR", Value::Num(0.0))?;
            return Ok(true);
        }
        Ok(false)
    }

    // ===================================================== records, fields

    /// Install `text` as `$0`, reusing the record buffer.  Fields are
    /// split when first needed.  `text` must not borrow `self.record`.
    fn set_record(&mut self, text: &[u8]) {
        if self.record.as_ptr() != text.as_ptr() {
            let mut rec = std::mem::take(&mut self.record);
            rec.clear();
            rec.extend_from_slice(text);
            self.record = rec;
        }
        self.record_value = None;
        self.record_is_string = false;
        self.fields.ready = false;
        self.fields.owned = false;
        self.record_fs = self.var_str("FS");
    }

    /// The record buffer already holds the next record (see
    /// `next_main_record`).  Forget the previous split and `$0` value.
    fn note_record(&mut self) {
        self.record_value = None;
        self.record_is_string = false;
        self.fields.ready = false;
        self.fields.owned = false;
        self.record_fs = self.var_str("FS");
    }

    /// Split `$0` if not done yet.  The spans keep the capacity of the
    /// previous record's vector, so a file of similar lines does not
    /// allocate a new one per record.
    fn ensure_fields(&mut self) -> R<()> {
        if self.fields.ready {
            return Ok(());
        }
        let paragraph = self.var_str("RS").is_empty();
        let fs = self.record_fs.clone();
        // Move the bytes out so `fill_spans` can borrow them and `self`
        // at the same time (a regex FS compiles while it splits).  Moving
        // a `Vec` does not copy the bytes.
        let rec = std::mem::take(&mut self.record);
        let mut spans = std::mem::take(&mut self.fields.spans);
        spans.clear();
        let filled = self.fill_spans(&rec, &fs, paragraph, &mut spans);
        self.record = rec;
        filled?;
        let n = spans.len();
        self.fields.spans = spans;
        self.fields.values.clear();
        self.fields.values.resize(n, None);
        self.fields.nums.clear();
        self.fields.nums.resize(n, None);
        self.fields.ready = true;
        self.fields.owned = false;
        Ok(())
    }

    /// Append the field spans of `s` to `out`.  Same FS rules as
    /// `split_with_fs`.
    fn fill_spans(
        &mut self,
        s: &[u8],
        fs: &[u8],
        paragraph: bool,
        out: &mut Vec<(usize, usize)>,
    ) -> R<()> {
        if fs == b" " {
            // Default FS: runs of blank, tab and newline, and the empties
            // between them are not fields.  Newline is already in that
            // set, so paragraph mode adds nothing here.
            let n = s.len();
            let mut i = 0;
            while i < n {
                while i < n && matches!(s[i], b' ' | b'\t' | b'\n') {
                    i += 1;
                }
                if i == n {
                    break;
                }
                let start = i;
                while i < n && !matches!(s[i], b' ' | b'\t' | b'\n') {
                    i += 1;
                }
                out.push((start, i));
            }
            return Ok(());
        }
        if s.is_empty() {
            return Ok(());
        }
        if fs.is_empty() {
            // One field per character.
            let mut i = 0;
            while i < s.len() {
                let j = charset::next(s, i, self.utf8);
                out.push((i, j));
                i = j;
            }
            return Ok(());
        }
        if fs.len() == 1 {
            let c = fs[0];
            let mut start = 0;
            for (i, &b) in s.iter().enumerate() {
                if b == c || (paragraph && b == b'\n') {
                    out.push((start, i));
                    start = i + 1;
                }
            }
            out.push((start, s.len()));
            return Ok(());
        }
        let re = if paragraph {
            let mut src = b"\n|(".to_vec();
            src.extend_from_slice(fs);
            src.push(b')');
            self.regex(&src)?
        } else {
            self.regex(fs)?
        };
        out.extend(split_regex_spans(&re, s));
        Ok(())
    }

    /// Split `s` by the FS rules (also used by `split()`):
    /// * `" "`: fields are runs of characters other than blank, tab and
    ///   newline;
    /// * `""`: one field per character;
    /// * one other character: split at exactly that character (even one
    ///   special in regexes, like `|`);
    /// * anything longer: a regular expression.
    ///
    /// In paragraph mode (RS = "") a newline also separates fields.
    fn split_with_fs(&mut self, s: &[u8], fs: &[u8], paragraph: bool) -> R<Vec<Vec<u8>>> {
        let mut spans = Vec::new();
        self.fill_spans(s, fs, paragraph, &mut spans)?;
        Ok(spans.into_iter().map(|(a, b)| s[a..b].to_vec()).collect())
    }

    /// Build a `Value` for every field that does not have one yet, and
    /// remember that the values (not the spans) are now authoritative.
    /// Assigning a field or NF does this, because the rebuilt `$0` is the
    /// values joined with OFS and may no longer split the same way.
    fn materialize_fields(&mut self) {
        if self.fields.owned {
            return;
        }
        for i in 0..self.fields.spans.len() {
            if self.fields.values[i].is_none() {
                let (a, b) = self.fields.spans[i];
                self.fields.values[i] = Some(Value::strnum(&self.record[a..b]));
            }
        }
        self.fields.owned = true;
    }

    // ============================================================ regexes

    /// The compiled regex for `src`, from the cache if possible.
    fn regex(&mut self, src: &[u8]) -> R<Rc<Regex>> {
        if let Some(re) = self.regexes.get(src) {
            return Ok(re.clone());
        }
        let re = Rc::new(Regex::with_mode(src, self.utf8)?);
        if self.regexes.len() >= REGEX_CACHE_ENTRIES
            || self.regex_states + re.size() > nfa_limit(self.utf8)
        {
            self.regexes.clear();
            self.regex_states = 0;
        }
        self.regex_states += re.size();
        self.regexes.insert(src.to_vec(), re.clone());
        Ok(re)
    }

    /// The regex an operand stands for: a /literal/ as written, any other
    /// expression's string value as a dynamic regex.
    fn regex_operand(&mut self, e: &'p Expr) -> R<Rc<Regex>> {
        match e {
            Expr::Regex(src) => self.regex(src),
            _ => {
                let s = self.str_arg(e)?;
                self.regex(&s)
            }
        }
    }

    /// The regex for a multi-byte RS (None for "" and single bytes).
    fn rs_regex(&mut self, rs: &[u8]) -> R<Option<Rc<Regex>>> {
        if rs.len() > 1 {
            Ok(Some(self.regex(rs)?))
        } else {
            Ok(None)
        }
    }

    /// `$i`.  A field past NF is the empty *string* (not uninitialized),
    /// as in all three reference awks.
    ///
    /// `$0` is copied out of the record buffer the first time it is needed
    /// as a value, so the next record can reuse the buffer.  A field that
    /// so far was only used as a number still has no `Value`; this builds
    /// one from the span (a strnum when the whole field is numeric).
    fn get_field(&mut self, i: usize) -> R<Value> {
        if i == 0 {
            if self.record_value.is_none() {
                // A copy: the record buffer is overwritten by the next
                // line, and this value must keep these bytes.
                let rc: Rc<[u8]> = Rc::from(&self.record[..]);
                self.record_value = Some(if self.record_is_string {
                    Value::Str(rc)
                } else {
                    match looks_numeric(&self.record) {
                        Some(n) => Value::StrNum(rc, n),
                        None => Value::Str(rc),
                    }
                });
            }
            return Ok(self.record_value.clone().unwrap());
        }
        self.ensure_fields()?;
        let idx = i - 1;
        if idx >= self.fields.values.len() {
            return Ok(Value::Str(empty()));
        }
        if self.fields.values[idx].is_none() {
            let (a, b) = self.fields.spans[idx];
            self.fields.values[idx] = Some(Value::strnum(&self.record[a..b]));
        }
        Ok(self.fields.values[idx].clone().unwrap())
    }

    /// `$i` as a number, without building a string for it.
    ///
    /// The numeric value of a field is `str_to_num` of its text, whether
    /// or not the whole field is a "numeric string".  The result is
    /// remembered, so a second use of the same field does not parse again.
    fn field_num(&mut self, i: usize) -> R<f64> {
        if i == 0 {
            return Ok(self.get_field(0)?.to_num());
        }
        self.ensure_fields()?;
        let idx = i - 1;
        if idx >= self.fields.nums.len() {
            return Ok(0.0);
        }
        if let Some(n) = self.fields.nums[idx] {
            return Ok(n);
        }
        if let Some(v) = &self.fields.values[idx] {
            let n = v.to_num();
            self.fields.nums[idx] = Some(n);
            return Ok(n);
        }
        let (a, b) = self.fields.spans[idx];
        let n = str_to_num(&self.record[a..b]);
        self.fields.nums[idx] = Some(n);
        Ok(n)
    }

    /// Assign `$i`.  `$0` re-splits; any other field rebuilds `$0` from
    /// the fields joined with OFS (`$1 = $1` reformats a line).  Assigning
    /// past NF adds empty fields.
    fn set_field(&mut self, i: usize, v: Value) -> R<()> {
        if i == 0 {
            let text = self.to_str(&v);
            let is_str = v.is_str();
            self.set_record(&text);
            if is_str {
                self.record_is_string = true;
                self.record_value = Some(v);
            }
            return Ok(());
        }
        self.ensure_fields()?;
        self.materialize_fields();
        while self.fields.values.len() < i {
            self.fields.values.push(Some(Value::Str(empty())));
            self.fields.nums.push(Some(0.0));
        }
        let n = v.to_num();
        self.fields.nums[i - 1] = Some(n);
        self.fields.values[i - 1] = Some(v);
        self.fields.owned = true;
        self.rebuild_record();
        Ok(())
    }

    /// `$0` = the fields joined with OFS (a plain string afterwards).
    fn rebuild_record(&mut self) {
        let ofs = self.var_str("OFS");
        let convfmt = self.convfmt();
        let mut rec = Vec::new();
        for (k, f) in self.fields.values.iter().enumerate() {
            if k > 0 {
                rec.extend_from_slice(&ofs);
            }
            // `unwrap`: `materialize_fields` (or `set_field` extending
            // the list) stored a value in every slot before this runs.
            rec.extend_from_slice(&f.as_ref().unwrap().to_str(&convfmt));
        }
        let rc: Rc<[u8]> = Rc::from(&rec[..]);
        self.record = rec;
        self.record_is_string = true;
        self.record_value = Some(Value::Str(rc));
        self.fields.owned = true;
        self.fields.ready = true;
    }

    fn get_nf(&mut self) -> R<f64> {
        self.ensure_fields()?;
        Ok(self.fields.values.len() as f64)
    }

    /// Assigning NF truncates or extends the fields and rebuilds `$0`.
    fn set_nf(&mut self, v: &Value) -> R<()> {
        let n = v.to_num();
        if n < 0.0 {
            return rt(format!("NF set to negative value {}", n.trunc() as i64));
        }
        let n = if n.is_finite() { n as usize } else { 0 };
        self.ensure_fields()?;
        self.materialize_fields();
        self.fields.values.truncate(n);
        self.fields.nums.truncate(n);
        while self.fields.values.len() < n {
            self.fields.values.push(Some(Value::Str(empty())));
            self.fields.nums.push(Some(0.0));
        }
        self.rebuild_record();
        Ok(())
    }

    /// Field number from a number: truncated; NaN and inf are errors,
    /// negative numbers are out of range.
    fn field_index(&self, n: f64) -> R<usize> {
        if !n.is_finite() {
            return rt("invalid field index");
        }
        let i = n.trunc();
        if i < 0.0 {
            return rt(format!("trying to access out of range field {}", i as i64));
        }
        Ok(i as usize)
    }

    /// Field number from a value: truncated; NaN and inf are errors,
    /// negative numbers are out of range.
    fn field_number(&self, v: &Value) -> R<usize> {
        self.field_index(v.to_num())
    }

    // ========================================================= variables

    /// CONVFMT as bytes.
    fn convfmt(&self) -> Rc<[u8]> {
        self.var_str("CONVFMT")
    }

    /// String value of a value, numbers converted with CONVFMT.
    fn to_str(&self, v: &Value) -> Rc<[u8]> {
        match v {
            Value::Num(_) => v.to_str(&self.convfmt()),
            _ => v.to_str(b""),
        }
    }

    /// One of the scalars stored beside the map (see `Interp::fs`), or
    /// `None` when `name` is an ordinary global.
    fn special_slot(&self, name: &str) -> Option<&Value> {
        Some(match name {
            "FS" => &self.fs,
            "RS" => &self.rs,
            "OFS" => &self.ofs,
            "ORS" => &self.ors,
            "OFMT" => &self.ofmt,
            "CONVFMT" => &self.convfmt_v,
            "SUBSEP" => &self.subsep,
            "NR" => &self.nr,
            "FNR" => &self.fnr,
            _ => return None,
        })
    }

    fn special_slot_mut(&mut self, name: &str) -> Option<&mut Value> {
        Some(match name {
            "FS" => &mut self.fs,
            "RS" => &mut self.rs,
            "OFS" => &mut self.ofs,
            "ORS" => &mut self.ors,
            "OFMT" => &mut self.ofmt,
            "CONVFMT" => &mut self.convfmt_v,
            "SUBSEP" => &mut self.subsep,
            "NR" => &mut self.nr,
            "FNR" => &mut self.fnr,
            _ => return None,
        })
    }

    /// String value of a built-in scalar.
    ///
    /// A string or strnum is shared (an `Rc` clone, no copying of bytes).
    /// A number is formatted now, with the CONVFMT current at this call:
    /// `BEGIN { FS = 1/3; CONVFMT = "%.2f" }` makes FS split on `0.33`,
    /// not on the `0.333333` it would have had when it was assigned.
    /// CONVFMT itself, when it is a number, is formatted with `%.6g` so
    /// this does not recurse.
    fn special_str(&self, name: &str) -> Option<Rc<[u8]>> {
        let n = match self.special_slot(name)? {
            Value::Uninit => return Some(empty()),
            Value::Str(s) | Value::StrNum(s, _) => return Some(s.clone()),
            Value::Num(n) => *n,
        };
        let fmt = if name == "CONVFMT" {
            Rc::from(&b"%.6g"[..])
        } else {
            self.convfmt()
        };
        Some(Rc::from(num_to_str(n, &fmt)))
    }

    /// A global's string value (for FS, OFS, ORS, RS, SUBSEP, ...).
    fn var_str(&self, name: &str) -> Rc<[u8]> {
        if let Some(s) = self.special_str(name) {
            return s;
        }
        match self.globals.get(name) {
            Some(Var::Scalar(Value::Num(n))) => {
                let fmt = if name == "CONVFMT" {
                    Rc::from(&b"%.6g"[..])
                } else {
                    self.convfmt()
                };
                Rc::from(num_to_str(*n, &fmt))
            }
            Some(Var::Scalar(v)) => v.to_str(b""),
            _ => empty(),
        }
    }

    fn var_num(&self, name: &str) -> f64 {
        if let Some(v) = self.special_slot(name) {
            return v.to_num();
        }
        match self.globals.get(name) {
            Some(Var::Scalar(v)) => v.to_num(),
            _ => 0.0,
        }
    }

    /// Index of `name` among the running function's parameters.
    fn local_index(&self, name: &str) -> Option<usize> {
        let frame = self.frames.last()?;
        frame.params.iter().position(|p| p == name)
    }

    /// Write the cached global back into the map.  Cheap when the cache
    /// is empty; otherwise one map update.  Called when a different name,
    /// or the same name from a different expression, is about to be used,
    /// and before any read of the map that bypasses the cache.
    fn store_scalar(&mut self) {
        if !self.sc_on {
            return;
        }
        let v = self.sc_val.clone();
        if let Some(Var::Scalar(slot)) = self.globals.get_mut(self.sc_name.as_str()) {
            *slot = v;
        }
    }

    /// True when `name` is the scalar already in the cache.
    ///
    /// The pointer and the length are not enough.  `-v aa=1 -v bb=2`
    /// builds each name in a temporary, and the allocator can hand the
    /// second name the same address as the first.  The letters have to
    /// match too, or `bb` would be written into `aa`.
    fn scalar_is_cached(&self, name: &str) -> bool {
        self.sc_on
            && self.sc_ptr == name.as_ptr()
            && self.sc_len == name.len()
            && self.sc_name == name
    }

    /// Point the cache at `name`, which is an ordinary scalar.  No work
    /// when this expression's name is already the one cached.
    fn touch_scalar(&mut self, name: &str) {
        let p = name.as_ptr();
        if self.scalar_is_cached(name) {
            return;
        }
        self.store_scalar();
        self.sc_on = false;
        if self.special_slot(name).is_some() || name == "NF" {
            return;
        }
        let v = match self.globals.get(name) {
            Some(Var::Scalar(v)) => v.clone(),
            // An array, or a missing name that might still become one:
            // leave it to the caller.  A missing name used as a scalar is
            // created below.
            Some(Var::Array(_)) => return,
            None => {
                self.globals
                    .insert(name.to_string(), Var::Scalar(Value::Uninit));
                Value::Uninit
            }
        };
        self.sc_ptr = p;
        self.sc_len = name.len();
        self.sc_name = name.to_string();
        self.sc_val = v;
        self.sc_on = true;
    }

    fn get_global(&mut self, name: &str) -> R<Value> {
        if name == "NF" {
            return Ok(Value::Num(self.get_nf()?));
        }
        if let Some(v) = self.special_slot(name) {
            return Ok(v.clone());
        }
        self.touch_scalar(name);
        if self.scalar_is_cached(name) {
            return Ok(self.sc_val.clone());
        }
        match self.globals.get(name) {
            Some(Var::Scalar(v)) => Ok(v.clone()),
            Some(Var::Array(_)) => rt(format!("can't use array {name} in scalar context")),
            None => {
                self.globals
                    .insert(name.to_string(), Var::Scalar(Value::Uninit));
                Ok(Value::Uninit)
            }
        }
    }

    fn set_global(&mut self, name: &str, v: Value) -> R<()> {
        if name == "NF" {
            return self.set_nf(&v);
        }
        if let Some(slot) = self.special_slot_mut(name) {
            *slot = v;
            return Ok(());
        }
        self.touch_scalar(name);
        if self.scalar_is_cached(name) {
            self.sc_val = v;
            return Ok(());
        }
        match self.globals.get_mut(name) {
            Some(Var::Array(_)) => rt(format!("can't assign to {name}; it's an array name")),
            Some(Var::Scalar(old)) => {
                *old = v;
                Ok(())
            }
            None => {
                self.globals.insert(name.to_string(), Var::Scalar(v));
                Ok(())
            }
        }
    }

    fn get_local(&mut self, slot: usize, name: &str) -> R<Value> {
        let cell = &mut self.frames.last_mut().unwrap().cells[slot];
        match cell {
            Cell::Scalar(v) => Ok(v.clone()),
            Cell::Array(_) => rt(format!("can't use array {name} in scalar context")),
            Cell::Untyped(_) => {
                *cell = Cell::Scalar(Value::Uninit);
                Ok(Value::Uninit)
            }
        }
    }

    fn set_local(&mut self, slot: usize, name: &str, v: Value) -> R<()> {
        let cell = &mut self.frames.last_mut().unwrap().cells[slot];
        if let Cell::Array(_) = cell {
            return rt(format!("can't assign to {name}; it's an array name"));
        }
        *cell = Cell::Scalar(v);
        Ok(())
    }

    /// The global array `name`, created empty on first use.
    fn global_array(&mut self, name: &str) -> R<ArrayRef> {
        if name == "NF" || self.special_slot(name).is_some() {
            return rt(format!("can't use scalar {name} as array"));
        }
        // The cached scalar, if this is its name, is the current value.
        self.store_scalar();
        match self.globals.get(name) {
            Some(Var::Array(a)) => Ok(a.clone()),
            Some(Var::Scalar(_)) => rt(format!("can't use scalar {name} as array")),
            None => {
                let a: ArrayRef = Rc::default();
                self.globals.insert(name.to_string(), Var::Array(a.clone()));
                Ok(a)
            }
        }
    }

    /// The array a local variable holds, created on first use.  If the
    /// local is linked to an untyped variable of a caller, the array is
    /// created *there* and shared, so `function f(a) { a[1] = 7 }` fills
    /// the caller's `x` in `f(x)`.
    fn cell_array(&mut self, frame: usize, slot: usize, name: &str) -> R<ArrayRef> {
        let link = match &self.frames[frame].cells[slot] {
            Cell::Array(a) => return Ok(a.clone()),
            Cell::Scalar(_) => return rt(format!("can't use scalar {name} as array")),
            Cell::Untyped(link) => link.clone(),
        };
        let arr = match link {
            None => Rc::default(),
            Some(Link::Global(g)) => self.global_array(&g)?,
            Some(Link::Local(f, s)) => self.cell_array(f, s, name)?,
        };
        self.frames[frame].cells[slot] = Cell::Array(arr.clone());
        Ok(arr)
    }

    /// The array called `name` (a local parameter or a global).
    fn array(&mut self, name: &str) -> R<ArrayRef> {
        match self.local_index(name) {
            Some(slot) => self.cell_array(self.frames.len() - 1, slot, name),
            None => self.cached_global_array(name),
        }
    }

    /// `global_array`, remembering the answer for this exact name (the
    /// pointer of the AST string, not just the letters).  A different
    /// expression that spells the same name misses once and then hits.
    fn cached_global_array(&mut self, name: &str) -> R<ArrayRef> {
        let p = name.as_ptr();
        if self.arr_ptr == p && self.arr_len == name.len() {
            return Ok(self.arr_cache.as_ref().unwrap().clone());
        }
        let a = self.global_array(name)?;
        self.arr_ptr = p;
        self.arr_len = name.len();
        self.arr_cache = Some(a.clone());
        Ok(a)
    }

    /// Append one subscript's key text to `key_buf`.  A field is copied
    /// off the record; anything else is evaluated and stringified.
    /// Evaluate `e` without losing `key_buf`.
    ///
    /// The subscript of `a[f($2)]++` is evaluated while that key is being
    /// built.  `f` may itself do `c[$1]++`, which uses the same buffer.
    /// The outer key is set aside for the call and put back afterwards.
    fn eval_keeping_key(&mut self, e: &'p Expr) -> R<Value> {
        let saved = std::mem::take(&mut self.key_buf);
        let v = self.eval(e);
        self.key_buf = saved;
        v
    }

    fn append_key_piece(&mut self, e: &'p Expr) -> R<()> {
        if let Expr::Field(index) = e {
            let i = match index.as_ref() {
                Expr::Num(n) => self.field_index(*n)?,
                _ => {
                    let v = self.eval_keeping_key(index)?;
                    self.field_number(&v)?
                }
            };
            return self.append_field_key(i);
        }
        let v = self.eval_keeping_key(e)?;
        let s = self.to_str(&v);
        self.key_buf.extend_from_slice(&s);
        Ok(())
    }

    /// Append field `i`'s text to `key_buf`.  The text is the field as it
    /// would print: the original bytes, or CONVFMT if the field was
    /// assigned a number.
    fn append_record_slice(&mut self, start: usize, end: usize) {
        // `key_buf` and `record` are both fields, so one of them steps
        // aside for the copy.
        let mut key = std::mem::take(&mut self.key_buf);
        key.extend_from_slice(&self.record[start..end]);
        self.key_buf = key;
    }

    fn append_field_key(&mut self, i: usize) -> R<()> {
        if i == 0 {
            let n = self.record.len();
            self.append_record_slice(0, n);
            return Ok(());
        }
        self.ensure_fields()?;
        let idx = i - 1;
        if idx >= self.fields.values.len() {
            return Ok(());
        }
        if let Some(v) = self.fields.values[idx].clone() {
            let s = self.to_str(&v);
            self.key_buf.extend_from_slice(&s);
            return Ok(());
        }
        let (a, b) = self.fields.spans[idx];
        self.append_record_slice(a, b);
        Ok(())
    }

    /// The key of `a[i, j, ...]`, written into `key_buf` (cleared first).
    fn fill_key(&mut self, subs: &'p [Expr]) -> R<()> {
        self.key_buf.clear();
        if let [one] = subs {
            return self.append_key_piece(one);
        }
        let sep = self.var_str("SUBSEP");
        for (k, e) in subs.iter().enumerate() {
            if k > 0 {
                self.key_buf.extend_from_slice(&sep);
            }
            self.append_key_piece(e)?;
        }
        Ok(())
    }

    /// "array", "scalar" or "untyped", without deciding an untyped name's
    /// kind (`length(x)` must not turn an unused `x` into a scalar).
    fn array_if_array(&self, name: &str) -> Option<Option<ArrayRef>> {
        if let Some(slot) = self.local_index(name) {
            let frames = &self.frames;
            let mut cell = &frames[frames.len() - 1].cells[slot];
            loop {
                match cell {
                    Cell::Array(a) => return Some(Some(a.clone())),
                    Cell::Scalar(_) => return None,
                    Cell::Untyped(None) => return Some(None),
                    Cell::Untyped(Some(Link::Local(f, s))) => cell = &frames[*f].cells[*s],
                    Cell::Untyped(Some(Link::Global(g))) => return self.global_kind(g),
                }
            }
        }
        self.global_kind(name)
    }

    fn global_kind(&self, name: &str) -> Option<Option<ArrayRef>> {
        if name == "NF" || self.special_slot(name).is_some() {
            return None; // a scalar, already decided
        }
        match self.globals.get(name) {
            Some(Var::Array(a)) => Some(Some(a.clone())),
            Some(Var::Scalar(_)) => None,
            None => Some(None),
        }
    }

    /// The array key for `a[i, j]`: the subscripts joined with SUBSEP.
    fn subscript(&mut self, subs: &'p [Expr]) -> R<Key> {
        if let [one] = subs {
            let v = self.eval(one)?;
            return Ok(self.to_str(&v));
        }
        let sep = self.var_str("SUBSEP");
        let mut key = Vec::new();
        for (k, e) in subs.iter().enumerate() {
            if k > 0 {
                key.extend_from_slice(&sep);
            }
            let v = self.eval(e)?;
            key.extend_from_slice(&self.to_str(&v));
        }
        Ok(Rc::from(key))
    }

    // ========================================================== places

    /// Work out what an lvalue refers to.  This evaluates subscripts and
    /// field numbers, so it happens before the right-hand side.
    fn place(&mut self, e: &'p Expr) -> R<Place<'p>> {
        match e {
            Expr::Var(name) => Ok(match self.local_index(name) {
                Some(slot) => Place::Local(slot),
                None if name == "NF" => Place::Nf,
                None => Place::Global(name),
            }),
            Expr::Index(name, subs) => {
                let arr = self.array(name)?;
                let key = self.subscript(subs)?;
                Ok(Place::Elem(arr, key))
            }
            Expr::Field(index) => {
                let v = self.eval(index)?;
                Ok(Place::Field(self.field_number(&v)?))
            }
            // The parser only builds assignments to lvalues.
            _ => rt("assignment to a non-lvalue"),
        }
    }

    fn get_place(&mut self, p: &Place<'p>) -> R<Value> {
        match p {
            Place::Global(name) => self.get_global(name),
            Place::Local(slot) => {
                let name = self.frames.last().unwrap().params[*slot].as_str();
                self.get_local(*slot, name)
            }
            Place::Nf => Ok(Value::Num(self.get_nf()?)),
            Place::Elem(arr, key) => Ok(arr.borrow_mut().get_or_create(key).clone()),
            Place::Field(i) => self.get_field(*i),
        }
    }

    fn set_place(&mut self, p: &Place<'p>, v: Value) -> R<()> {
        match p {
            Place::Global(name) => self.set_global(name, v),
            Place::Local(slot) => {
                let name = self.frames.last().unwrap().params[*slot].as_str();
                self.set_local(*slot, name, v)
            }
            Place::Nf => self.set_nf(&v),
            Place::Elem(arr, key) => {
                arr.borrow_mut().set(key, v);
                Ok(())
            }
            Place::Field(i) => self.set_field(*i, v),
        }
    }

    // ======================================================== statements

    fn exec_block(&mut self, stmts: &'p [Stmt]) -> R<Flow> {
        for s in stmts {
            let flow = self.exec(s)?;
            if flow != Flow::Normal {
                return Ok(flow);
            }
        }
        Ok(Flow::Normal)
    }

    /// Run a loop body; `Some(flow)` means "leave the loop and return
    /// this", `None` means "go round again".
    fn loop_body(&mut self, body: &'p Stmt) -> R<Option<Flow>> {
        Ok(match self.exec(body)? {
            Flow::Normal | Flow::Continue => None,
            Flow::Break => Some(Flow::Normal),
            other => Some(other),
        })
    }

    /// Run one statement.  `Flow` says how control leaves it (normally,
    /// or by break, continue, return...), and `R` carries errors and the
    /// unwinding of `next`, `nextfile` and `exit`.
    fn exec(&mut self, s: &'p Stmt) -> R<Flow> {
        self.check_stack()?;
        match s {
            Stmt::Block(b) => return self.exec_block(b),
            Stmt::Expr(e) => {
                self.eval(e)?;
            }
            Stmt::Print { args, redirect } => {
                let ofs = self.var_str("OFS");
                let ors = self.var_str("ORS");
                let mut line = Vec::new();
                if args.is_empty() {
                    line.extend_from_slice(&self.record);
                } else {
                    for (k, a) in args.iter().enumerate() {
                        if k > 0 {
                            line.extend_from_slice(&ofs);
                        }
                        match self.eval(a)? {
                            Value::Num(n) => {
                                line.extend_from_slice(&num_to_str(n, &self.var_str("OFMT")))
                            }
                            v => line.extend_from_slice(&v.to_str(b"")),
                        }
                    }
                }
                line.extend_from_slice(&ors);
                self.output(redirect, &line)?;
            }
            Stmt::Printf { args, redirect } => {
                if args.is_empty() {
                    // A bare `printf` prints nothing (onetrue-awk and gawk;
                    // mawk rejects it), but `printf > "f"` still opens f.
                    return self.output(redirect, b"").map(|()| Flow::Normal);
                }
                // `spare` is empty while we hold `buf`, so a printf inside
                // an argument gets its own buffer and cannot clobber this
                // one.
                let mut buf = std::mem::take(&mut self.spare);
                buf.clear();
                let wrote = self.printf_into(&mut buf, args);
                let result = wrote.and_then(|()| self.output(redirect, &buf));
                self.spare = buf;
                result?;
            }
            Stmt::If { test, then, orelse } => {
                if self.cond(test)? {
                    return self.exec(then);
                } else if let Some(o) = orelse {
                    return self.exec(o);
                }
            }
            Stmt::While { test, body } => {
                while self.cond(test)? {
                    if let Some(f) = self.loop_body(body)? {
                        return Ok(f);
                    }
                }
            }
            Stmt::DoWhile { body, test } => loop {
                if let Some(f) = self.loop_body(body)? {
                    return Ok(f);
                }
                if !self.cond(test)? {
                    break;
                }
            },
            Stmt::For {
                init,
                test,
                step,
                body,
            } => {
                if let Some(i) = init {
                    self.exec(i)?;
                }
                loop {
                    if let Some(t) = test
                        && !self.cond(t)?
                    {
                        break;
                    }
                    if let Some(f) = self.loop_body(body)? {
                        return Ok(f);
                    }
                    if let Some(st) = step {
                        self.exec(st)?;
                    }
                }
            }
            Stmt::ForIn { var, array, body } => {
                let arr = self.array(array)?;
                let keys = arr.borrow().keys();
                for key in keys {
                    if !arr.borrow().contains(&key) {
                        continue; // deleted by the loop body
                    }
                    match self.local_index(var) {
                        Some(slot) => self.set_local(slot, var, Value::Str(key))?,
                        None => self.set_global(var, Value::Str(key))?,
                    }
                    if let Some(f) = self.loop_body(body)? {
                        return Ok(f);
                    }
                }
            }
            Stmt::Break => return Ok(Flow::Break),
            Stmt::Continue => return Ok(Flow::Continue),
            Stmt::Next => return Ok(Flow::Next),
            Stmt::NextFile => return Ok(Flow::NextFile),
            Stmt::Exit(status) => {
                if let Some(e) = status {
                    let n = self.eval(e)?.to_num();
                    let n = if n.is_finite() { n as i64 } else { 0 };
                    self.exit_code = (n & 0xFF) as i32;
                }
                return Err(Unwind::Exit);
            }
            Stmt::Return(value) => {
                self.ret = match value {
                    Some(e) => self.eval(e)?,
                    None => Value::Uninit,
                };
                return Ok(Flow::Return);
            }
            Stmt::Delete { array, subs } => {
                let arr = self.array(array)?;
                match subs {
                    None => arr.borrow_mut().clear(),
                    Some(subs) => {
                        let key = self.subscript(subs)?;
                        arr.borrow_mut().remove(&key);
                    }
                }
            }
        }
        Ok(Flow::Normal)
    }

    /// Send print/printf output to stdout or a redirection.
    fn output(&mut self, redirect: &'p Option<(Redirect, Expr)>, data: &[u8]) -> R<()> {
        match redirect {
            None => self.io.write_stdout(data)?,
            Some((kind, dest)) => {
                let v = self.eval(dest)?;
                let name = self.to_str(&v);
                self.io.write_to(*kind, &name, data)?;
            }
        }
        Ok(())
    }

    /// Make `fmt_compiled` the parse of this constant format, if it is
    /// not already.
    fn ensure_format(&mut self, fmt: &[u8]) {
        let p = fmt.as_ptr();
        if self.fmt_ptr == p && self.fmt_len == fmt.len() {
            return;
        }
        self.fmt_compiled = compile_format(fmt);
        self.fmt_ptr = p;
        self.fmt_len = fmt.len();
    }

    /// Append the printf of `args` (format first) to `out`.
    ///
    /// A constant format is parsed once.  `%d` and `%f` read a field as a
    /// number, so `printf "%d", $2` does not allocate the field's text.
    /// `%s` still needs that text.
    fn printf_into(&mut self, out: &mut Vec<u8>, args: &'p [Expr]) -> R<()> {
        let convfmt = self.convfmt();
        let utf8 = self.utf8;
        // Move the compiled pieces out of `self` so arguments can be
        // evaluated (an argument may printf, and would use the same
        // slot).  The move does not copy the text; putting the pieces
        // back keeps them for the next record.
        let (compiled, restore) = match args.first() {
            Some(Expr::Str(fmt)) => {
                self.ensure_format(fmt);
                let ptr = self.fmt_ptr;
                let len = self.fmt_len;
                self.fmt_ptr = std::ptr::null();
                (std::mem::take(&mut self.fmt_compiled), Some((ptr, len)))
            }
            Some(e) => {
                let fmt = self.str_arg(e)?;
                (compile_format(&fmt), None)
            }
            None => return Ok(()),
        };
        let mut i = 1; // args[0] is the format
        let result: R<()> = format_compiled(out, &compiled, &convfmt, utf8, |kind| {
            let Some(e) = args.get(i) else {
                i += 1;
                // A missing argument is not an unset variable: `%c` prints
                // nothing rather than a NUL.  "" does that, and is 0 for
                // the numeric conversions.
                return Ok(Value::Str(empty()));
            };
            i += 1;
            Ok(match kind {
                FmtArg::Number => Value::Num(self.eval_num(e)?),
                FmtArg::Value | FmtArg::Char => self.eval(e)?,
            })
        });
        if let Some((ptr, len)) = restore {
            self.fmt_compiled = compiled;
            self.fmt_ptr = ptr;
            self.fmt_len = len;
        }
        result?;
        // `printf "x", i++` has nothing for `i++` to fill, but the
        // argument is still evaluated.  The same for sprintf.
        for e in args.iter().skip(i) {
            self.eval(e)?;
        }
        Ok(())
    }

    /// Evaluate `args` (the format first) and format them as printf does.
    fn sprintf(&mut self, args: &'p [Expr]) -> R<Vec<u8>> {
        let mut out = Vec::new();
        self.printf_into(&mut out, args)?;
        Ok(out)
    }

    // ======================================================= expressions

    /// An expression as a truth value.
    fn cond(&mut self, e: &'p Expr) -> R<bool> {
        self.check_stack()?;
        Ok(match e {
            // A bare /re/ as a condition means `$0 ~ /re/`.
            Expr::Regex(src) => {
                let re = self.regex(src)?;
                re.is_match(&self.record)
            }
            Expr::Match {
                negate,
                left,
                regex,
            } => {
                // The regex is evaluated first, as in pyawk.
                let re = self.regex_operand(regex)?;
                let text = self.str_arg(left)?;
                re.is_match(&text) != *negate
            }
            Expr::Compare(op, l, r) => self.compare(*op, l, r)?,
            Expr::And(l, r) => self.cond(l)? && self.cond(r)?,
            Expr::Or(l, r) => self.cond(l)? || self.cond(r)?,
            Expr::Unary(UnOp::Not, x) => !self.cond(x)?,
            Expr::Num(n) => *n != 0.0,
            _ => self.eval(e)?.to_bool(),
        })
    }

    /// Comparison.  NaN is unordered: `<`, `<=` and `==` are false and
    /// `>`, `>=`, `!=` true (pyawk writes `>` as "not `<=`").
    fn compare(&mut self, op: CmpOp, l: &'p Expr, r: &'p Expr) -> R<bool> {
        let a = self.eval(l)?;
        let b = self.eval(r)?;
        let ord = compare(&a, &b, &self.convfmt());
        let lt = ord == Some(Ordering::Less);
        let le = matches!(ord, Some(Ordering::Less | Ordering::Equal));
        let eq = ord == Some(Ordering::Equal);
        Ok(match op {
            CmpOp::Lt => lt,
            CmpOp::Le => le,
            CmpOp::Eq => eq,
            CmpOp::Ne => !eq,
            CmpOp::Gt => !le,
            CmpOp::Ge => !lt,
        })
    }

    fn bool_value(b: bool) -> Value {
        Value::Num(if b { 1.0 } else { 0.0 })
    }

    /// The numeric value of an expression, without building a string for
    /// a field or a variable that is already a number.
    ///
    /// `s += $2` only needs this.  Building the field's `Value` would
    /// copy its text into a fresh reference-counted string and throw that
    /// away after reading the number.  Anything this does not recognise
    /// (a function call, a comparison, a string) falls back to `eval`.
    fn eval_num(&mut self, e: &'p Expr) -> R<f64> {
        self.check_stack()?;
        match e {
            Expr::Num(n) => Ok(*n),
            Expr::Field(index) => {
                let i = match index.as_ref() {
                    Expr::Num(n) => self.field_index(*n)?,
                    _ => {
                        let v = self.eval(index)?;
                        self.field_number(&v)?
                    }
                };
                self.field_num(i)
            }
            Expr::Var(name) => match self.local_index(name) {
                Some(slot) => Ok(self.get_local(slot, name)?.to_num()),
                None if name == "NF" => self.get_nf(),
                None => Ok(match self.special_slot(name) {
                    Some(v) => v.to_num(),
                    None => self.get_global(name)?.to_num(),
                }),
            },
            Expr::Binary(op, l, r) => arith(*op, self.eval_num(l)?, self.eval_num(r)?),
            Expr::Unary(UnOp::Neg, x) => Ok(-self.eval_num(x)?),
            Expr::Unary(UnOp::Plus, x) => self.eval_num(x),
            Expr::Unary(UnOp::Not, x) => Ok(if self.cond(x)? { 0.0 } else { 1.0 }),
            _ => Ok(self.eval(e)?.to_num()),
        }
    }

    pub fn eval(&mut self, e: &'p Expr) -> R<Value> {
        self.check_stack()?;
        Ok(match e {
            Expr::Num(n) => Value::Num(*n),
            Expr::Str(s) => Value::str(s),
            Expr::Regex(_)
            | Expr::Match { .. }
            | Expr::Compare(..)
            | Expr::And(..)
            | Expr::Or(..)
            | Expr::Unary(UnOp::Not, _) => Self::bool_value(self.cond(e)?),
            Expr::Var(name) => match self.local_index(name) {
                Some(slot) => self.get_local(slot, name)?,
                None => self.get_global(name)?,
            },
            Expr::Index(name, subs) => {
                let arr = self.array(name)?;
                let key = self.subscript(subs)?;
                arr.borrow_mut().get_or_create(&key).clone()
            }
            Expr::Field(index) => {
                let v = self.eval(index)?;
                let i = self.field_number(&v)?;
                self.get_field(i)?
            }
            Expr::Group(_) => return rt("parenthesised expression list used as a value"),
            Expr::Assign(op, target, value) => {
                let place = self.place(target)?;
                // The target is resolved first, then the right-hand side,
                // then (for `+=` and friends) the current value.  A
                // function call on the right can change the variable, and
                // the addition must see that new value.
                let v = match op.0 {
                    None => self.eval(value)?,
                    Some(bin) => {
                        let rhs = self.eval_num(value)?;
                        let old = self.get_place(&place)?.to_num();
                        Value::Num(arith(bin, old, rhs)?)
                    }
                };
                self.set_place(&place, v.clone())?;
                v
            }
            Expr::Cond(test, yes, no) => {
                if self.cond(test)? {
                    self.eval(yes)?
                } else {
                    self.eval(no)?
                }
            }
            Expr::In(subs, name) => {
                let key = self.subscript(subs)?;
                let arr = self.array(name)?;
                let found = arr.borrow().contains(&key);
                Self::bool_value(found)
            }
            Expr::Concat(l, r) => {
                let a = self.eval(l)?;
                let a = self.to_str(&a);
                let b = self.eval(r)?;
                let b = self.to_str(&b);
                let mut s = Vec::with_capacity(a.len() + b.len());
                s.extend_from_slice(&a);
                s.extend_from_slice(&b);
                Value::Str(Rc::from(s))
            }
            Expr::Binary(op, l, r) => Value::Num(arith(*op, self.eval_num(l)?, self.eval_num(r)?)?),
            Expr::Unary(op, x) => {
                let n = self.eval_num(x)?;
                Value::Num(if *op == UnOp::Neg { -n } else { n })
            }
            Expr::IncDec {
                incr,
                prefix,
                target,
            } => {
                // `c[$1]++` is the hot shape: the subscript is usually a
                // field, and the element usually already exists.  Building
                // a Place would copy the key into an `Rc` on every record
                // even when the lookup hits.
                if let Expr::Index(name, subs) = target.as_ref() {
                    self.fill_key(subs)?;
                    let arr = self.array(name)?;
                    let delta = if *incr { 1.0 } else { -1.0 };
                    let old = arr.borrow_mut().add_number(&self.key_buf, delta);
                    Value::Num(if *prefix { old + delta } else { old })
                } else {
                    let place = self.place(target)?;
                    let old = self.get_place(&place)?.to_num();
                    let new = if *incr { old + 1.0 } else { old - 1.0 };
                    self.set_place(&place, Value::Num(new))?;
                    Value::Num(if *prefix { new } else { old })
                }
            }
            Expr::Call(b, args) => self.builtin(*b, args)?,
            Expr::UserCall(name, args) => self.call_user(name, args)?,
            Expr::Getline {
                kind,
                target,
                source,
            } => self.getline(*kind, target.as_deref(), source.as_deref())?,
        })
    }

    // ========================================================= functions

    /// Call a user-defined function: bind the arguments in the caller's
    /// frame, run the body in a new frame, and return what `return` gave
    /// (uninitialized when the body just ends).
    fn call_user(&mut self, name: &'p str, args: &'p [Expr]) -> R<Value> {
        let prog = self.prog;
        let func: &'p Function = match prog.function(name) {
            Some(f) => f,
            // Not a parse error: AWK only complains if the call happens.
            None => return rt(format!("calling undefined function {name}")),
        };
        let nparams = func.params.len();
        let mut cells = Vec::with_capacity(nparams);
        for (k, a) in args.iter().enumerate() {
            if k < nparams {
                cells.push(self.bind(a)?);
            } else {
                self.eval(a)?; // extra arguments: evaluated, then dropped
            }
        }
        while cells.len() < nparams {
            cells.push(Cell::Untyped(None)); // the function's own locals
        }
        if self.stack_base.saturating_sub(stack_position()) > self.stack_budget {
            return rt("function call nesting too deep (runaway recursion?)");
        }
        self.frames.push(Frame {
            params: &func.params,
            cells,
        });
        let flow = self.exec_block(&func.body);
        self.frames.pop();
        Ok(match flow? {
            Flow::Return => std::mem::replace(&mut self.ret, Value::Uninit),
            // `next` in a function body: unwind to the record loop.
            Flow::Next => return Err(Unwind::Next),
            _ => Value::Uninit,
        })
    }

    /// Make the callee's cell for one argument.  Scalars are passed by
    /// value, arrays by reference; a bare name that is still untyped is
    /// linked, so that it can still become an array (see `cell_array`).
    fn bind(&mut self, arg: &'p Expr) -> R<Cell> {
        if let Expr::Var(name) = arg {
            if let Some(slot) = self.local_index(name) {
                let frame = self.frames.len() - 1;
                return Ok(match &self.frames[frame].cells[slot] {
                    Cell::Array(a) => Cell::Array(a.clone()),
                    Cell::Scalar(v) => Cell::Scalar(v.clone()),
                    Cell::Untyped(_) => Cell::Untyped(Some(Link::Local(frame, slot))),
                });
            }
            if name == "NF" {
                return Ok(Cell::Scalar(Value::Num(self.get_nf()?)));
            }
            if let Some(v) = self.special_slot(name) {
                return Ok(Cell::Scalar(v.clone()));
            }
            self.store_scalar();
            return Ok(match self.globals.get(name.as_str()) {
                Some(Var::Array(a)) => Cell::Array(a.clone()),
                Some(Var::Scalar(v)) => Cell::Scalar(v.clone()),
                None => Cell::Untyped(Some(Link::Global(name.clone()))),
            });
        }
        Ok(Cell::Scalar(self.eval(arg)?))
    }

    // =========================================================== getline

    /// The getline forms; 1 (got a record), 0 (end of input), -1 (error).
    /// The target is resolved before reading, as gawk and mawk do.
    fn getline(
        &mut self,
        kind: GetlineKind,
        target: Option<&'p Expr>,
        source: Option<&'p Expr>,
    ) -> R<Value> {
        let rs = self.var_str("RS");
        let mut place = None;
        let rec = match kind {
            GetlineKind::Simple => {
                if let Some(t) = target {
                    place = Some(self.resolve_target(t)?);
                }
                if place.is_none() {
                    if !self.next_main_record()? {
                        return Ok(Value::Num(0.0));
                    }
                    self.bump("NR");
                    self.bump("FNR");
                    self.note_record();
                    return Ok(Value::Num(1.0));
                }
                // Into a variable: the current $0 stays as it is.
                let mut buf = Vec::new();
                if !self.read_main(&mut buf)? {
                    return Ok(Value::Num(0.0));
                }
                self.bump("NR");
                self.bump("FNR");
                self.set_place(&place.unwrap(), Value::strnum(&buf))?;
                return Ok(Value::Num(1.0));
            }
            GetlineKind::File | GetlineKind::Cmd => {
                let src = match source {
                    Some(s) => self.eval(s)?,
                    None => Value::Uninit,
                };
                let name = self.to_str(&src);
                if let Some(t) = target {
                    place = Some(self.resolve_target(t)?);
                }
                let re = self.rs_regex(&rs)?;
                let find = |t: &[u8], p: usize, c: &mut Transcoding| {
                    re.as_deref().and_then(|r| Finder::cached(r, t, c).find(p))
                };
                let finder: Option<RegexFinder> = re.is_some().then_some(&find);
                let got = if kind == GetlineKind::File {
                    match self.io.input_file(&name)? {
                        None => return Ok(Value::Num(-1.0)),
                        Some(reader) => reader.read_record(&rs, finder)?,
                    }
                } else {
                    self.io.input_cmd(&name)?.read_record(&rs, finder)?
                };
                // NR and FNR are left alone by both forms: mawk, onetrue-awk
                // and gawk agree, although POSIX says `cmd | getline`
                // increments NR (pyawk v1.1 follows the three awks).
                match got {
                    None => return Ok(Value::Num(0.0)),
                    Some(r) => r,
                }
            }
        };
        match place {
            None => self.set_record(&rec),
            Some(p) => self.set_place(&p, Value::strnum(&rec))?,
        }
        Ok(Value::Num(1.0))
    }

    fn resolve_target(&mut self, t: &'p Expr) -> R<Place<'p>> {
        let p = self.place(t)?;
        if let Place::Elem(arr, key) = &p {
            arr.borrow_mut().get_or_create(key); // referencing creates it
        }
        Ok(p)
    }

    // ========================================================== builtins

    fn arity(b: Builtin, args: &[Expr], lo: usize, hi: usize) -> R<()> {
        if args.len() < lo || args.len() > hi {
            return rt(format!(
                "function {} called with {} args",
                b.name(),
                args.len()
            ));
        }
        Ok(())
    }

    fn str_arg(&mut self, e: &'p Expr) -> R<Rc<[u8]>> {
        let v = self.eval(e)?;
        Ok(self.to_str(&v))
    }

    fn num_arg(&mut self, e: &'p Expr) -> R<f64> {
        self.eval_num(e)
    }

    /// The length of `s` in characters, as a number.
    fn length(&self, s: &[u8]) -> f64 {
        if self.utf8 {
            charset::count(s) as f64
        } else {
            s.len() as f64
        }
    }

    /// Replace a literal in `$0` without allocating, when that is safe.
    ///
    /// `Some(count)` means the replacement was done (including a count of
    /// zero: the text did not change).  `None` means the caller should
    /// use the general substitution: the pattern is not a fixed string,
    /// it is a raw regex (see `is_raw`), or the replacement is not the
    /// same number of plain bytes (`&` and backslashes are special).
    fn replace_record_inplace(&mut self, re: &Regex, repl: &[u8], global: bool) -> Option<usize> {
        // A raw regex (an invalid byte written as `\351` in UTF-8 mode)
        // is compiled to the bytes of its escaped form.  Those bytes are
        // not what the match looks for in the record; the general path
        // transcodes the record first.  Searching for them here would
        // miss the byte.
        if re.is_raw() {
            return None;
        }
        let lit = re.literal_bytes()?;
        if lit.len() != repl.len() || repl.contains(&b'&') || repl.contains(&b'\\') {
            return None;
        }
        // Copy the pattern out of `re` before borrowing the record.
        // The record is our own buffer, not shared with a saved `$0`
        // (that was copied out when the value was built), so it can
        // always be overwritten.
        let lit = lit.to_vec();
        let n = replace_literal(&mut self.record, &lit, repl, global);
        if n > 0 {
            // The bytes changed, so a previous split is wrong, and `$0`
            // is now a plain string (not a strnum), same as assigning
            // the result of the general substitution.
            self.fields.ready = false;
            self.fields.owned = false;
            self.record_is_string = true;
            self.record_value = None;
            // Assigning `$0` (what the general path does) splits the new
            // text with the FS that is current now, not the one from when
            // the record was read.  `{ FS=":"; gsub(/a/,"b"); print $1 }`
            // on `a:x` is `b`.
            self.record_fs = self.var_str("FS");
        }
        Some(n)
    }

    /// Call a builtin function.  The parser has checked the argument
    /// counts; arguments are evaluated here, as each builtin needs them.
    fn builtin(&mut self, b: Builtin, args: &'p [Expr]) -> R<Value> {
        use Builtin::*;
        let num = |n: f64| Ok(Value::Num(n));
        match b {
            Length => {
                Self::arity(b, args, 0, 1)?;
                match args.first() {
                    None => num(self.length(&self.record)),
                    // `$0` is the record buffer; counting it does not
                    // need a copy of the bytes.
                    Some(Expr::Field(ix)) if matches!(ix.as_ref(), Expr::Num(n) if *n == 0.0) => {
                        num(self.length(&self.record))
                    }
                    Some(Expr::Var(name)) => match self.array_if_array(name) {
                        Some(Some(arr)) => num(arr.borrow().len() as f64),
                        Some(None) => num(0.0), // untyped: stays untyped
                        None => {
                            let s = self.str_arg(&args[0])?;
                            num(self.length(&s))
                        }
                    },
                    Some(e) => {
                        let s = self.str_arg(e)?;
                        num(self.length(&s))
                    }
                }
            }
            Substr => {
                Self::arity(b, args, 2, 3)?;
                let s = self.str_arg(&args[0])?;
                let m = self.num_arg(&args[1])?;
                let n = match args.get(2) {
                    Some(e) => Some(self.num_arg(e)?),
                    None => None,
                };
                Ok(Value::str(substr(&s, m, n, self.utf8)))
            }
            Index => {
                Self::arity(b, args, 2, 2)?;
                let s = self.str_arg(&args[0])?;
                let t = self.str_arg(&args[1])?;
                num(index_of(&s, &t, self.utf8) as f64)
            }
            Split => {
                Self::arity(b, args, 2, 3)?;
                let s = self.eval(&args[0])?;
                let name = match &args[1] {
                    Expr::Var(name) => name,
                    _ => return rt("split: second argument must be an array name"),
                };
                let arr = self.array(name)?;
                let text = self.to_str(&s);
                let pieces = match args.get(2) {
                    // A /literal/ is always a regex, even one byte long.
                    Some(Expr::Regex(src)) => {
                        let re = self.regex(src)?;
                        split_regex(&re, &text)
                    }
                    Some(e) => {
                        let fs = self.str_arg(e)?;
                        self.split_with_fs(&text, &fs, false)?
                    }
                    None => {
                        let fs = self.var_str("FS");
                        self.split_with_fs(&text, &fs, false)?
                    }
                };
                let mut a = arr.borrow_mut();
                a.clear();
                for (k, p) in pieces.iter().enumerate() {
                    a.set(&Rc::from((k + 1).to_string().as_bytes()), Value::strnum(p));
                }
                num(pieces.len() as f64)
            }
            Match => {
                Self::arity(b, args, 2, 2)?;
                let text = self.str_arg(&args[0])?;
                let re = self.regex_operand(&args[1])?;
                let (start, length) = match re.find_at(&text, 0) {
                    Some((s, e)) => ((self.length(&text[..s]) + 1.0), self.length(&text[s..e])),
                    None => (0.0, -1.0),
                };
                self.set_global("RSTART", Value::Num(start))?;
                self.set_global("RLENGTH", Value::Num(length))?;
                num(start)
            }
            Sub | Gsub => {
                Self::arity(b, args, 2, 3)?;
                let re = self.regex_operand(&args[0])?;
                let repl = self.str_arg(&args[1])?;
                let global = b == Gsub;
                match args.get(2) {
                    // The default target is $0.
                    None => {
                        // `gsub(/a/, "A")` replaces one byte with one byte.
                        // If nothing else holds the record, that can be
                        // done in the buffer it already occupies.
                        let n = match self.replace_record_inplace(&re, &repl, global) {
                            Some(n) => n,
                            None => {
                                let text = self.record.clone();
                                let (out, n) = substitute(&re, &repl, &text, global);
                                if n > 0 {
                                    self.set_field(0, Value::Str(Rc::from(out)))?;
                                }
                                n
                            }
                        };
                        num(n as f64)
                    }
                    // An lvalue is resolved once (`arr[i++]` steps i once)
                    // and only assigned if something was replaced.
                    Some(t @ (Expr::Var(_) | Expr::Index(..) | Expr::Field(_))) => {
                        let place = self.resolve_target(t)?;
                        let old = self.get_place(&place)?;
                        let text = self.to_str(&old);
                        let (out, n) = substitute(&re, &repl, &text, global);
                        if n > 0 {
                            self.set_place(&place, Value::Str(Rc::from(out)))?;
                        }
                        num(n as f64)
                    }
                    // Any other target: computed, and the result dropped.
                    Some(e) => {
                        let text = self.str_arg(e)?;
                        num(substitute(&re, &repl, &text, global).1 as f64)
                    }
                }
            }
            Sprintf => {
                if args.is_empty() {
                    return rt("sprintf: no format");
                }
                Ok(Value::Str(Rc::from(self.sprintf(args)?)))
            }
            Sin | Cos | Exp | Log | Sqrt => {
                Self::arity(b, args, 1, 1)?;
                let x = self.num_arg(&args[0])?;
                num(match b {
                    Sin => x.sin(),
                    Cos => x.cos(),
                    Exp => x.exp(),
                    Log => x.ln(),
                    _ => x.sqrt(),
                })
            }
            Atan2 => {
                Self::arity(b, args, 2, 2)?;
                let y = self.num_arg(&args[0])?;
                let x = self.num_arg(&args[1])?;
                num(y.atan2(x))
            }
            Int => {
                Self::arity(b, args, 1, 1)?;
                num(self.num_arg(&args[0])?.trunc())
            }
            Rand => {
                Self::arity(b, args, 0, 0)?;
                num(self.rng.random())
            }
            Srand => {
                Self::arity(b, args, 0, 1)?;
                let prev = self.seed;
                self.seed = match args.first() {
                    Some(e) => self.num_arg(e)?,
                    None => std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0.0, |d| d.as_secs() as f64),
                };
                self.rng = Random::new(self.seed);
                num(prev)
            }
            Tolower | Toupper => {
                Self::arity(b, args, 1, 1)?;
                let s = self.str_arg(&args[0])?;
                let out = charset::change_case(&s, b == Toupper, self.utf8);
                Ok(Value::Str(Rc::from(out)))
            }
            System => {
                Self::arity(b, args, 1, 1)?;
                let cmd = self.str_arg(&args[0])?;
                num(self.io.system(&cmd)?)
            }
            Close => {
                Self::arity(b, args, 1, 1)?;
                let name = self.str_arg(&args[0])?;
                num(self.io.close(&name)?)
            }
            Fflush => {
                // fflush() and fflush(""): everything (POSIX 2024; gawk and
                // mawk); fflush(name): that output only.
                Self::arity(b, args, 0, 1)?;
                let name = match args.first() {
                    Some(a) => self.str_arg(a)?,
                    None => Rc::from(&b""[..]),
                };
                if name.is_empty() {
                    num(self.io.flush_all()?)
                } else {
                    num(self.io.flush(&name)?)
                }
            }
        }
    }
}

/// Overwrite non-overlapping occurrences of `lit` with `repl` (same
/// length) inside `buf`.  `global` false stops after the first.  Returns
/// how many were replaced.
fn replace_literal(buf: &mut [u8], lit: &[u8], repl: &[u8], global: bool) -> usize {
    if lit.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut i = 0;
    if lit.len() == 1 {
        let from = lit[0];
        let to = repl[0];
        while i < buf.len() {
            if buf[i] == from {
                buf[i] = to;
                count += 1;
                if !global {
                    break;
                }
            }
            i += 1;
        }
        return count;
    }
    while i + lit.len() <= buf.len() {
        if &buf[i..i + lit.len()] == lit {
            buf[i..i + lit.len()].copy_from_slice(repl);
            count += 1;
            i += lit.len();
            if !global {
                break;
            }
        } else {
            i += 1;
        }
    }
    count
}

/// Arithmetic with AWK's error rules: division by zero is fatal; `%` is
/// C's `fmod`; `^` gives inf on overflow and NaN on a domain error.
fn arith(op: BinOp, a: f64, b: f64) -> R<f64> {
    Ok(match op {
        BinOp::Add => a + b,
        BinOp::Sub => a - b,
        BinOp::Mul => a * b,
        BinOp::Div => {
            if b == 0.0 {
                return rt("division by zero");
            }
            a / b
        }
        BinOp::Mod => {
            if b == 0.0 {
                return rt("division by zero in %");
            }
            a % b // Rust's `%` on floats is C's fmod
        }
        BinOp::Pow => a.powf(b),
    })
}

/// First position of `needle` in `hay` (an empty needle is at 0).
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `substr(s, m[, n])`, counting characters from 1 (pyawk's rules:
/// truncate toward zero; a start below 1 does not shorten the length; NaN
/// start is 1, NaN length gives "").  Characters are bytes unless `utf8`;
/// an invalid byte is one character and is kept as it is.
fn substr(s: &[u8], m: f64, n: Option<f64>, utf8: bool) -> &[u8] {
    let chars = utf8 && !s.is_ascii();
    let size = if chars { charset::count(s) } else { s.len() } as f64;
    let clamp = |x: f64, lo: f64, hi: f64| {
        if x <= lo {
            lo
        } else if x >= hi {
            hi
        } else {
            x.trunc()
        }
    };
    let start = if m.is_nan() {
        1.0
    } else {
        clamp(m, 1.0, size + 1.0)
    };
    let length = match n {
        None => size + 1.0 - start,
        Some(n) if n.is_nan() => return b"",
        Some(n) => clamp(n, 0.0, size + 1.0 - start),
    };
    let from = start as usize - 1;
    if chars {
        let a = charset::byte_offset(s, from);
        let b = a + charset::byte_offset(&s[a..], length as usize);
        return &s[a..b];
    }
    &s[from..from + length as usize]
}

/// `index(s, t)`: the character position of `t` in `s` (1-based), or 0.
fn index_of(s: &[u8], t: &[u8], utf8: bool) -> usize {
    if !utf8 || s.is_ascii() {
        return find(s, t).map_or(0, |k| k + 1);
    }
    if std::str::from_utf8(t).is_ok() {
        // A valid string can only be found at character boundaries: its
        // first byte is never a continuation byte, and it ends with a
        // whole character.
        return find(s, t).map_or(0, |k| charset::count(&s[..k]) + 1);
    }
    // `t` holds invalid bytes: try each character start.
    let mut i = 0;
    let mut k = 1;
    while i <= s.len() {
        if s[i..].starts_with(t)
            && (i + t.len() == s.len() || charset::is_boundary(&s[i..], t.len()))
        {
            return k;
        }
        if i == s.len() {
            break;
        }
        i += charset::char_len(s, i);
        k += 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn substr_c(s: &[u8], m: f64, n: Option<f64>) -> &[u8] {
        substr(s, m, n, false)
    }

    #[test]
    fn utf8_substr_and_index_count_characters() {
        let u = |s: &str, m: f64, n: Option<f64>| substr(s.as_bytes(), m, n, true).to_vec();
        assert_eq!(u("héllo", 2.0, Some(3.0)), "éll".as_bytes());
        assert_eq!(u("日本語", 3.0, None), "語".as_bytes());
        assert_eq!(substr(b"a\xffb\xc3", 2.0, Some(3.0), true), b"\xffb\xc3");
        assert_eq!(index_of("héllo".as_bytes(), b"l", true), 3);
        assert_eq!(index_of("héllo".as_bytes(), b"l", false), 4);
        assert_eq!(index_of(b"\xc3\xa9\xa9", b"\xa9", true), 2);
        assert_eq!(index_of("é".as_bytes(), b"\xa9", true), 0);
        assert_eq!(index_of("é".as_bytes(), b"\xc3", true), 0);
        assert_eq!(index_of(b"\xc3", b"\xc3", true), 1);
        assert_eq!(index_of(b"ab", b"", true), 1);
    }

    #[test]
    fn substr_rules() {
        assert_eq!(substr_c(b"hello", 2.0, Some(3.0)), b"ell");
        assert_eq!(substr_c(b"hello", 0.0, Some(2.0)), b"he");
        assert_eq!(substr_c(b"hello", -1.0, None), b"hello");
        assert_eq!(substr_c(b"hello", 4.0, Some(100.0)), b"lo");
        assert_eq!(substr_c(b"hello", 2.7, Some(1.9)), b"e");
        assert_eq!(substr_c(b"hello", 9.0, None), b"");
        assert_eq!(substr_c(b"hello", 2.0, Some(-1.0)), b"");
        assert_eq!(substr_c(b"hello", f64::NAN, Some(2.0)), b"he");
        assert_eq!(substr_c(b"hello", 1.0, Some(f64::NAN)), b"");
        assert_eq!(
            substr_c(b"hello", f64::NEG_INFINITY, Some(f64::INFINITY)),
            b"hello"
        );
    }

    #[test]
    fn operands_that_assign() {
        assert_eq!(assignment_operand(b"x=1"), Some((&b"x"[..], &b"1"[..])));
        assert_eq!(
            assignment_operand(b"_a9=b=c"),
            Some((&b"_a9"[..], &b"b=c"[..]))
        );
        assert_eq!(assignment_operand(b"9x=1"), None);
        assert_eq!(assignment_operand(b"=1"), None);
        assert_eq!(assignment_operand(b"a.txt"), None);
        assert_eq!(assignment_operand(b"./x=1"), None);
    }

    #[test]
    fn runaway_recursion_is_safe_on_a_default_thread() {
        // With `stack_size` 0 the interpreter does not know its stack, so
        // it must assume no more than the 2 MiB a spawned thread gets.
        let src = b"function f(n) { return f(n + 1) } BEGIN { f(0) }";
        let msg = std::thread::spawn(move || {
            let prog = crate::parser::parse(src).unwrap();
            let mut interp = Interp::new(&prog, &Config::default()).unwrap();
            interp.run().err().map(|e| e.to_string())
        })
        .join()
        .unwrap()
        .unwrap_or_default();
        assert!(msg.contains("function call nesting too deep"), "{msg}");
    }

    #[test]
    fn runaway_recursion_is_safe_on_a_small_stack() {
        // A caller that says its stack is only 1 MiB must still get the
        // error, not a crash: the budget is then half the stack.
        let src = b"function f(n) { return f(n + 1) } BEGIN { f(0) }";
        let msg = std::thread::Builder::new()
            .stack_size(1 << 20)
            .spawn(move || {
                let prog = crate::parser::parse_with_stack(src, 1 << 20).unwrap();
                let config = Config {
                    stack_size: 1 << 20,
                    ..Config::default()
                };
                let mut interp = Interp::new(&prog, &config).unwrap();
                interp.run().err().map(|e| e.to_string())
            })
            .unwrap()
            .join()
            .unwrap()
            .unwrap_or_default();
        assert!(msg.contains("function call nesting too deep"), "{msg}");
    }

    #[test]
    fn arithmetic_errors() {
        assert!(arith(BinOp::Div, 1.0, 0.0).is_err());
        assert!(arith(BinOp::Mod, 1.0, 0.0).is_err());
        assert_eq!(arith(BinOp::Mod, -7.0, 3.0).unwrap(), -1.0);
        assert_eq!(arith(BinOp::Pow, 10.0, 400.0).unwrap(), f64::INFINITY);
        assert!(arith(BinOp::Pow, -8.0, 1.0 / 3.0).unwrap().is_nan());
    }
}
