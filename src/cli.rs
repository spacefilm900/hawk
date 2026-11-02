//! Command line: `hawk [-F fs] [-v var=val]... ('prog' | -f file...) [operand...]`.
//!
//! Options come first; the first argument that is not an option is the
//! program text (unless `-f` gave program files) and everything after it is
//! an operand (an input file or `var=value`), passed on as ARGV.  Errors are
//! reported as "hawk: message" with exit status 2, as pyawk does.
//!
//! Arguments are bytes (`Vec<u8>`), not `String`s: on Unix a file name or
//! an argument need not be valid UTF-8, and AWK passes them through as-is.

use std::io::{Read, Write};

use crate::error::Error;
use crate::interp::{Config, Interp};

/// The version string printed by `hawk --version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "usage: hawk [-F fs][-v var=value][-f progfile | 'prog'][file ...]\n";

/// The stack size of the interpreter thread.  Deeply recursive AWK
/// functions recurse in the interpreter too; the main thread's default
/// (often 8 MiB) would overflow long before AWK's own depth limit.  The
/// memory is only reserved, not used, until the recursion needs it.
/// If even reserving it fails (under `ulimit -v`, say), smaller sizes are
/// tried in turn, and the interpreter's recursion limit shrinks to match.
const STACK_SIZES: [usize; 4] = [1 << 30, 256 << 20, 64 << 20, 16 << 20];

/// What the options asked for.
struct Parsed {
    source: Vec<u8>,
    config: Config,
}

/// `Err(Some(msg))` is a usage error; `Err(None)` means "done, exit 0"
/// (after --version or --help).
fn parse_args(args: &[Vec<u8>]) -> Result<Parsed, Option<String>> {
    let mut config = Config::default();
    let mut progfiles: Vec<Vec<u8>> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i][..];
        if arg == b"--" {
            i += 1;
            break;
        }
        if arg == b"--version" || arg == b"-V" {
            println!("hawk {VERSION}");
            return Err(None);
        }
        if arg == b"-h" || arg == b"--help" {
            print!("{USAGE}");
            return Err(None);
        }
        if !arg.starts_with(b"-") || arg == b"-" {
            break;
        }
        let flag = &arg[..2];
        // The option's value: the rest of this argument ("-F:") or the
        // next argument ("-F :").
        let mut value = || -> Result<Vec<u8>, Option<String>> {
            if arg.len() > 2 {
                return Ok(arg[2..].to_vec());
            }
            i += 1;
            match args.get(i) {
                Some(v) => Ok(v.clone()),
                None => Err(Some(format!(
                    "option {} requires an argument",
                    String::from_utf8_lossy(flag)
                ))),
            }
        };
        match flag {
            b"-F" => {
                let fs = value()?;
                config.assignments.push((b"FS".to_vec(), fs));
            }
            b"-v" => {
                let spec = value()?;
                let ok = spec.iter().position(|&b| b == b'=').filter(|&eq| {
                    let name = &spec[..eq];
                    name.first()
                        .is_some_and(|&b| b.is_ascii_alphabetic() || b == b'_')
                        && name.iter().all(|&b| b.is_ascii_alphanumeric() || b == b'_')
                });
                match ok {
                    Some(eq) => config
                        .assignments
                        .push((spec[..eq].to_vec(), spec[eq + 1..].to_vec())),
                    None => {
                        return Err(Some(format!(
                            "improper assignment: -v {}",
                            String::from_utf8_lossy(&spec)
                        )));
                    }
                }
            }
            b"-f" => progfiles.push(value()?),
            _ => {
                return Err(Some(format!(
                    "unknown option {}",
                    String::from_utf8_lossy(arg)
                )));
            }
        }
        i += 1;
    }
    let source = if !progfiles.is_empty() {
        let mut chunks = Vec::new();
        for path in &progfiles {
            let text = if path == b"-" {
                let mut buf = Vec::new();
                std::io::stdin().read_to_end(&mut buf).map(|_| buf)
            } else {
                use std::os::unix::ffi::OsStrExt;
                std::fs::read(std::ffi::OsStr::from_bytes(path))
            };
            match text {
                Ok(t) => chunks.push(t),
                Err(e) => {
                    return Err(Some(format!(
                        "couldn't open file {}: {}",
                        String::from_utf8_lossy(path),
                        crate::io::strerror(&e)
                    )));
                }
            }
        }
        chunks.join(&b'\n')
    } else {
        match args.get(i) {
            Some(text) => {
                i += 1;
                text.clone()
            }
            None => return Err(Some("no program given".into())),
        }
    };
    config.operands = args[i..].to_vec();
    Ok(Parsed { source, config })
}

/// Run hawk with `args` (the command line without the program name) and
/// return the process exit status.
pub fn main(args: Vec<Vec<u8>>) -> i32 {
    if args.first().map(|a| &a[..]) == Some(b"--dump-ast") {
        return dump_ast(&args[1..]);
    }
    let parsed = match parse_args(&args) {
        Ok(p) => p,
        Err(None) => return 0,
        Err(Some(msg)) => {
            eprint!("hawk: {msg}\n{USAGE}");
            return 2;
        }
    };
    on_big_stack(move |stack_size| {
        let mut parsed = parsed;
        parsed.config.stack_size = stack_size;
        run(parsed)
    })
}

/// Run `job` on a thread with the biggest stack in STACK_SIZES that can be
/// reserved, passing it that size; if no thread can be started at all, run
/// it on this thread, passing 0 ("the usual size").
///
/// `F: FnOnce(usize) -> i32 + Send + 'static` says what `job` must be: a
/// function called once with a size, returning the exit status, that may
/// move to another thread (`Send`) and borrows nothing from this one
/// (`'static`).
fn on_big_stack<F: FnOnce(usize) -> i32 + Send + 'static>(job: F) -> i32 {
    // `spawn` consumes the function it is given even when it fails, so the
    // job sits in a shared slot (`Arc` = shared ownership across threads,
    // `Mutex` = safe access from either) that each attempt can take it from.
    let slot = std::sync::Arc::new(std::sync::Mutex::new(Some(job)));
    for size in STACK_SIZES {
        let mine = std::sync::Arc::clone(&slot);
        let worker = std::thread::Builder::new().stack_size(size).spawn(move || {
            // Take the job out first, so the lock is not held while it runs.
            let job = mine.lock().unwrap().take();
            job.map(|job| job(size))
        });
        // If the thread started, `join` waits for it.  `Ok(Some(status))` is
        // its exit status; `Err` means it panicked (the panic message has
        // already been printed).  If it did not start, try a smaller stack.
        if let Ok(handle) = worker {
            return match handle.join() {
                Ok(Some(status)) => status,
                _ => {
                    eprintln!("hawk: internal error");
                    2
                }
            };
        }
    }
    match slot.lock().unwrap().take() {
        Some(job) => job(0),
        // Unreachable: a thread that took the job returned above.
        None => 2,
    }
}

fn run(parsed: Parsed) -> i32 {
    let result = crate::parser::parse(&parsed.source).and_then(|program| {
        for warning in &program.warnings {
            eprintln!("hawk: warning: {warning}");
        }
        let mut interp = Interp::new(&program, &parsed.config).map_err(|u| match u {
            crate::interp::Unwind::Err(e) => e,
            crate::interp::Unwind::Exit => Error::Runtime("exit".into()),
        })?;
        interp.run()
    });
    match result {
        Ok(status) => status,
        Err(Error::BrokenPipe) => 2,
        Err(e) => {
            let _ = std::io::stdout().flush();
            eprintln!("hawk: {e}");
            2
        }
    }
}

/// Hidden debugging aid: `hawk --dump-ast [-f progfile]... | 'program'`
/// parses the program and prints the tree in the canonical dump format
/// (see `ast::dump_program`), which `tools/ast_check.py` compares with
/// pyawk's parse.  Several `-f` files are joined with newlines, as pyawk does.
fn dump_ast(args: &[Vec<u8>]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(p) => p,
        Err(None) => return 0,
        Err(Some(msg)) => {
            eprintln!("hawk: {msg}");
            return 2;
        }
    };
    // The parser recurses too, so it also gets the big stack.
    on_big_stack(move |_| match crate::parser::parse(&parsed.source) {
        Ok(program) => {
            println!("{}", crate::ast::dump_program(&program));
            0
        }
        Err(e) => {
            eprintln!("hawk: {e}");
            2
        }
    })
}
