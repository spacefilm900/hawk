//! hawk ("Haviland Awk"): a POSIX AWK interpreter, ported from pyawk.
//!
//! The crate is split in two, which is the usual Rust layout for a program
//! that also wants unit tests of its parts:
//!
//! * this *library* (`src/lib.rs` and the modules it declares) holds the
//!   whole interpreter;
//! * the *binary* (`src/main.rs`) is a thin wrapper that hands the command
//!   line to [`cli::main`] and exits with the status it returns.
//!
//! `pub mod x;` tells the compiler that module `x` lives in `src/x.rs` and
//! may be used from outside this file (`pub`).  Pipeline, as in pyawk:
//!
//! ```text
//! source --lexer--> tokens --parser--> AST --interp--> output
//! ```
//!
//! See DESIGN.md for the architecture and PLAN.md for what is done.

pub mod array;
pub mod ast;
pub mod cli;
pub mod error;
pub mod format;
pub mod input;
pub mod io;
pub mod lexer;
pub mod parser;
pub mod rand;
pub mod value;
