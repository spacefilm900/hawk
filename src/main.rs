//! The `hawk` executable: everything happens in the library (`src/lib.rs`).

use std::os::unix::ffi::OsStringExt; // adds `.into_vec()` to `OsString`

fn main() {
    // `args_os()` yields the command line as `OsString`s: raw bytes on
    // Unix, which need not be UTF-8.  `skip(1)` drops the program name.
    let args: Vec<Vec<u8>> = std::env::args_os().skip(1).map(|a| a.into_vec()).collect();
    std::process::exit(hawk::cli::main(args));
}
