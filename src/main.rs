//! The `hawk` executable: everything happens in the library (`src/lib.rs`).

fn main() {
    // `std::env::args()` yields the command line as `String`s; `skip(1)`
    // drops the program name and `collect()` gathers the rest into a `Vec`.
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(hawk::cli::main(&args));
}
