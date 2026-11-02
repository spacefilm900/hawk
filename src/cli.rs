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
    // `let _ =` deliberately ignores the result of the write: if stderr
    // itself is broken there is nobody left to tell.
    let _ = writeln!(std::io::stderr(), "hawk: not implemented yet");
    2
}
