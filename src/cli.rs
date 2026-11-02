//! Command-line entry point.
//!
//! For now (phase 0) hawk only knows `--version`; every other command line
//! is rejected with exit status 2, the status pyawk and the reference awks
//! use for errors.  The real option parser (`-F`, `-v`, `-f`, `--`) comes
//! with the runtime (see PLAN.md).

use std::io::Write;

/// The version string printed by `hawk --version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Run hawk with `args` (the command line without the program name) and
/// return the process exit status.
///
/// `&[String]` is a *slice*: a borrowed view of a list owned by the caller.
/// Borrowing (rather than taking ownership) means the caller keeps its
/// `Vec` and we promise not to change it.
pub fn main(args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("--version") {
        println!("hawk {VERSION}");
        return 0;
    }
    if args.first().map(String::as_str) == Some("--dump-ast") {
        return dump_ast(&args[1..]);
    }
    // `let _ =` deliberately ignores the result of the write: if stderr
    // itself is broken there is nobody left to tell.
    let _ = writeln!(std::io::stderr(), "hawk: not implemented yet");
    2
}

/// Hidden debugging aid: `hawk --dump-ast [-f progfile]... | 'program'`
/// parses the program and prints the tree in the canonical dump format
/// (see `ast::dump_program`), which `tools/ast_check.py` compares with
/// pyawk's parse.  Several `-f` files are joined with newlines, as pyawk does.
fn dump_ast(args: &[String]) -> i32 {
    let mut source: Vec<u8> = Vec::new();
    let mut files = 0;
    let mut i = 0;
    while i + 1 < args.len() && args[i] == "-f" {
        match std::fs::read(&args[i + 1]) {
            Ok(text) => {
                if files > 0 {
                    source.push(b'\n');
                }
                source.extend_from_slice(&text);
                files += 1;
            }
            Err(e) => {
                eprintln!("hawk: couldn't open file {}: {e}", args[i + 1]);
                return 2;
            }
        }
        i += 2;
    }
    if files == 0 {
        match args.get(i) {
            Some(text) => source = text.clone().into_bytes(),
            None => {
                eprintln!("hawk: no program given");
                return 2;
            }
        }
    }
    match crate::parser::parse(&source) {
        Ok(program) => {
            println!("{}", crate::ast::dump_program(&program));
            0
        }
        Err(e) => {
            eprintln!("hawk: {e}");
            2
        }
    }
}
