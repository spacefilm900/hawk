//! End-to-end tests for input and output (stage 3 of the v1.1 port):
//! exit statuses, error reports and signals, which the golden corpus
//! cannot see because it compares only standard output.  They follow
//! pyawk v1.1's tests/test_io.py; see DESIGN.md, "I/O (stage 3)".

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt; // `.signal()` on an exit status
use std::process::{Command, ExitStatus, Stdio};

/// The signal numbers these tests need (the same on every Linux).
const SIGINT: i32 = 2;
const SIGPIPE: i32 = 13;

/// What a run gave: standard output, standard error, and how it ended.
struct Ran {
    out: String,
    err: String,
    status: ExitStatus,
}

/// Run `hawk prog` with `input` on stdin.  `stdout` / `stderr` replace
/// the captured streams (to point one at /dev/full, say); a replaced
/// stream reads back as "".
fn run_with(prog: &str, input: &str, stdout: Option<File>, stderr: Option<File>) -> Ran {
    // `map_or_else` uses the file if there is one, or else a pipe.
    let out = stdout.map_or_else(Stdio::piped, Stdio::from);
    let err = stderr.map_or_else(Stdio::piped, Stdio::from);
    let mut child = Command::new(env!("CARGO_BIN_EXE_hawk"))
        .env("LC_ALL", "C") // C mode unless a test says otherwise
        .arg(prog)
        .stdin(Stdio::piped())
        .stdout(out)
        .stderr(err)
        .spawn()
        .expect("hawk runs");
    let mut stdin = child.stdin.take().unwrap();
    // A program that never reads its input may exit first: ignore errors.
    let _ = stdin.write_all(input.as_bytes());
    drop(stdin);
    let o = child.wait_with_output().unwrap();
    Ran {
        out: String::from_utf8_lossy(&o.stdout).into_owned(),
        err: String::from_utf8_lossy(&o.stderr).into_owned(),
        status: o.status,
    }
}

fn run(prog: &str, input: &str) -> Ran {
    run_with(prog, input, None, None)
}

/// /dev/full opened for writing: every write fails with ENOSPC.  `None`
/// where there is no /dev/full (the tests then do nothing).
fn full() -> Option<File> {
    File::options().write(true).open("/dev/full").ok()
}

// ------------------------------------------------------------- SIGPIPE

#[test]
fn reader_going_away_kills_us_with_sigpipe() {
    // Like `awk ... | head -1`: mawk, onetrue-awk and gawk are all killed
    // by SIGPIPE (the shell shows status 141), silently.
    let mut child = Command::new(env!("CARGO_BIN_EXE_hawk"))
        .env("LC_ALL", "C") // C mode unless a test says otherwise
        .arg("BEGIN { for (i = 0; i < 1000000; i++) print i }")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line, "0\n");
    drop(reader);
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.signal(), Some(SIGPIPE));
    assert_eq!(String::from_utf8_lossy(&o.stderr), "");
}

#[test]
fn command_going_away_kills_us_with_sigpipe() {
    // print | "head -1": head exits after one line and the next write
    // kills us (mawk and onetrue-awk; gawk reports a fatal error).
    let r = run(
        "BEGIN { for (i = 0; i < 1000000; i++) print i | \"head -1\"; print \"not reached\" }",
        "",
    );
    assert_eq!(r.out, "0\n");
    assert_eq!(r.status.signal(), Some(SIGPIPE));
}

#[test]
fn reader_gone_before_the_final_flush_kills_us_with_sigpipe() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_hawk"))
        .env("LC_ALL", "C") // C mode unless a test says otherwise
        .arg("BEGIN { system(\"sleep 0.3\"); print \"x\" }")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.signal(), Some(SIGPIPE));
    assert_eq!(String::from_utf8_lossy(&o.stderr), "");
}

// --------------------------------------------------------------- Ctrl-C

#[test]
fn ctrl_c_ends_the_run_with_status_130() {
    // A C awk is killed by SIGINT, which a shell reports as 130 (128 + 2).
    let mut child = Command::new(env!("CARGO_BIN_EXE_hawk"))
        .env("LC_ALL", "C") // C mode unless a test says otherwise
        .arg("BEGIN { print \"go\"; fflush(); while (1) x++ }")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap(); // it is running the loop now
    let killed = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(SIGINT));
    // What a shell's `$?` shows: 128 + the signal.
    assert_eq!(128 + status.signal().unwrap(), 130);
}

// ----------------------------------------------------------- bad names

#[test]
fn null_file_or_command_name_is_an_error() {
    // onetrue-awk and gawk stop with a fatal error (mawk returns -1 / 0).
    for prog in [
        "BEGIN { getline x < \"\" }",
        "BEGIN { getline < \"\" }",
        "BEGIN { \"\" | getline }",
        "BEGIN { print \"x\" | \"\" }",
        "BEGIN { print \"x\" > \"\" }",
        "BEGIN { print \"x\" >> \"\" }",
    ] {
        let r = run(prog, "");
        assert_eq!(r.status.code(), Some(2), "{prog}");
        assert!(
            r.err.starts_with("hawk: null file name in "),
            "{prog}: {}",
            r.err
        );
    }
}

// --------------------------------------------------------------- fflush

#[test]
fn fflush_on_a_full_stdout_gives_minus_one_then_a_write_error() {
    // fflush() and fflush("/dev/stdout") report -1 and the program carries
    // on (mawk and onetrue-awk); the final flush fails with status 2.
    let Some(f) = full() else { return };
    let prog = "BEGIN { printf \"x\"; r = fflush(\"/dev/stdout\"); printf \"y\"; \
                s = fflush(); print r, s > \"/dev/stderr\" }";
    let r = run_with(prog, "", Some(f), None);
    let mut lines = r.err.lines();
    assert_eq!(lines.next(), Some("-1 -1"));
    assert_eq!(
        lines.next(),
        Some("hawk: write error on standard output (No space left on device)")
    );
    assert_eq!(r.status.code(), Some(2));
}

#[test]
fn fflush_of_a_full_file_stops_the_program() {
    // onetrue-awk and gawk stop at the fflush (mawk returns -1).
    if full().is_none() {
        return;
    }
    for call in ["fflush(\"/dev/full\")", "fflush()", "fflush(\"\")"] {
        let r = run(
            &format!("BEGIN {{ printf \"x\" > \"/dev/full\"; {call}; print \"after\"; exit 3 }}"),
            "",
        );
        assert_eq!((r.out.as_str(), r.status.code()), ("", Some(2)), "{call}");
        assert!(
            r.err.starts_with("hawk: write error on /dev/full"),
            "{call}"
        );
    }
}

// ---------------------------------------------------------------- close

#[test]
fn close_dev_stdout_keeps_standard_output() {
    // All three references return 0; afterwards gawk keeps writing to
    // standard output (mawk and onetrue-awk lose later output).
    let r = run(
        "BEGIN { print \"a\" > \"/dev/stdout\"; r = close(\"/dev/stdout\"); print r; print \"b\" }",
        "",
    );
    assert_eq!((r.out.as_str(), r.status.code()), ("a\n0\nb\n", Some(0)));
}

#[test]
fn close_dev_stderr_after_printing_to_it_returns_zero() {
    let r = run(
        "BEGIN { print \"e\" > \"/dev/stderr\"; print close(\"/dev/stderr\"); \
         print \"x\" > \"/dev/stderr\" }",
        "",
    );
    assert_eq!((r.out.as_str(), r.err.as_str()), ("0\n", "e\nx\n"));
}

#[test]
fn close_of_a_special_name_never_used_returns_minus_one() {
    let r = run(
        "BEGIN { print close(\"/dev/stdout\"), close(\"/dev/stderr\"), close(\"-\"), \
         close(\"/dev/stdin\") }",
        "",
    );
    assert_eq!(
        (r.out.as_str(), r.status.code()),
        ("-1 -1 -1 -1\n", Some(0))
    );
}

#[test]
fn close_of_stdin_read_by_getline_uses_it_up() {
    for name in ["-", "/dev/stdin"] {
        let prog = format!(
            "BEGIN {{ getline x < \"{name}\"; print x, close(\"{name}\"); \
             print (getline y < \"{name}\"); print (getline z) }} {{ print \"main\", $0 }}"
        );
        let r = run(&prog, "1\n2\n3\n");
        assert_eq!(
            (r.out.as_str(), r.status.code()),
            ("1 0\n0\n0\n", Some(0)),
            "{name}"
        );
    }
}

#[test]
fn all_names_for_stdin_share_one_reader() {
    // onetrue-awk: consecutive lines (mawk and gawk lose data here).
    let r = run(
        "BEGIN { getline a < \"-\"; getline b < \"/dev/stdin\"; getline c; print a, b, c }",
        "1\n2\n3\n",
    );
    assert_eq!(r.out, "1 2 3\n");
}

#[test]
fn close_on_a_full_disk_stops_the_program() {
    if full().is_none() {
        return;
    }
    let r = run(
        "BEGIN { printf \"x\" > \"/dev/full\"; close(\"/dev/full\"); print \"after\"; exit 3 }",
        "",
    );
    assert_eq!(r.out, "");
    assert!(
        r.err.starts_with("hawk: write error on /dev/full"),
        "{}",
        r.err
    );
    assert_eq!(r.status.code(), Some(2));
}

#[test]
fn close_dev_stdout_on_a_full_disk_stops_the_program() {
    let Some(f) = full() else { return };
    let prog = "BEGIN { printf \"x\" > \"/dev/stdout\"; close(\"/dev/stdout\"); \
                print \"after\" > \"/dev/stderr\"; exit 3 }";
    let r = run_with(prog, "", Some(f), None);
    assert_eq!(r.err.lines().count(), 1, "{}", r.err);
    assert!(
        r.err.starts_with("hawk: write error on /dev/stdout"),
        "{}",
        r.err
    );
    assert_eq!(r.status.code(), Some(2));
}

#[test]
fn a_file_that_cannot_be_written_at_exit_is_a_write_error() {
    if full().is_none() {
        return;
    }
    let r = run("BEGIN { printf \"x\" > \"/dev/full\"; print \"done\" }", "");
    assert_eq!(r.out, "done\n");
    assert!(r.err.contains("write error on /dev/full"), "{}", r.err);
    assert_eq!(r.status.code(), Some(2));
}

#[test]
fn unwritable_stderr_gives_status_2() {
    // No message can be shown, but the status says something went wrong
    // (mawk, onetrue-awk and gawk all exit with 2).
    if full().is_none() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("hawk-io-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("out");
    for prog in [
        "printf \"x\" > \"/dev/stderr\"; close(\"/dev/stderr\"); print \"after\" > OUT; exit 3",
        "printf \"x\" > \"/dev/stderr\"; fflush(\"/dev/stderr\"); print \"after\" > OUT; exit 3",
        "printf \"x\" > \"/dev/stderr\"; exit 3",
        "print \"x\" > \"/dev/stderr\"; exit 3",
        "getline < \"\"",
    ] {
        let src = format!("BEGIN {{ OUT = \"{}\"; {prog} }}", out.display());
        let r = run_with(&src, "", None, full());
        assert_eq!(r.status.code(), Some(2), "{prog}");
        assert!(!out.exists(), "{prog}: the program went on");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_command_failing_at_exit_is_not_a_write_error() {
    let r = run("BEGIN { print \"x\" | \"exit 3\" }", "");
    assert_eq!(
        (r.out.as_str(), r.err.as_str(), r.status.code()),
        ("", "", Some(0))
    );
}

#[test]
fn close_returns_the_exit_status_of_a_command() {
    let r = run(
        "BEGIN { print \"x\" | \"cat >/dev/null; exit 3\"; print close(\"cat >/dev/null; exit 3\"); \
         \"exit 5\" | getline; print close(\"exit 5\"); print close(\"never\") }",
        "",
    );
    assert_eq!(r.out, "3\n5\n-1\n");
}

#[test]
fn dash_is_a_file_name_for_output() {
    // mawk and onetrue-awk write a file called "-" (gawk: standard output).
    let dir = std::env::temp_dir().join(format!("hawk-dash-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_hawk"))
        .env("LC_ALL", "C") // C mode unless a test says otherwise
        .arg("BEGIN { print \"dash\" > \"-\"; print \"out\" }")
        .current_dir(&dir)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&o.stdout), "out\n");
    assert_eq!(std::fs::read_to_string(dir.join("-")).unwrap(), "dash\n");
    std::fs::remove_dir_all(&dir).unwrap();
}

// ------------------------------------------- FILENAME on implicit stdin
// POSIX.1-2024 awk, "ARGV": when no file operand is left and none has been
// processed, "awk shall set FILENAME to '-' and process standard input as
// if it was given as a file operand".  The FILENAME entry agrees: "or '-'
// if the file is standard input".  mawk and gawk do this; hawk v1.2 left
// FILENAME empty.  (awk-posix-challenge cases AWK-391, 392 and 399.)

/// Run `hawk prog operands...` with `input` on stdin; return stdout.
fn run_operands(prog: &str, operands: &[&str], input: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_hawk"))
        .env("LC_ALL", "C")
        .arg(prog)
        .args(operands)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("hawk runs");
    let mut stdin = child.stdin.take().unwrap();
    let _ = stdin.write_all(input.as_bytes());
    drop(stdin); // closing stdin lets hawk see end of input
    let o = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn filename_is_dash_for_stdin_without_operands() {
    let out = run_operands(r#"{ print "[" FILENAME "]:" $0 }"#, &[], "alpha\nbeta\n");
    assert_eq!(out, "[-]:alpha\n[-]:beta\n");
}

#[test]
fn filename_is_dash_for_stdin_with_only_assignment_operands() {
    let out = run_operands(r#"{ print "[" FILENAME "]:" v ":" $0 }"#, &["v=7"], "x\n");
    assert_eq!(out, "[-]:7:x\n");
}

#[test]
fn filename_is_dash_when_argv_turned_into_an_assignment() {
    // The operand is replaced in BEGIN, so the named file is never opened.
    let prog = r#"BEGIN { ARGV[1] = "v=7" } { print "[" FILENAME "]:" v ":" $0 }"#;
    let out = run_operands(prog, &["not-opened"], "hello\n");
    assert_eq!(out, "[-]:7:hello\n");
}

#[test]
fn filename_dash_is_kept_in_end_and_set_by_getline_in_begin() {
    // END sees the last file processed, here standard input, even if empty.
    assert_eq!(
        run_operands(r#"END { print "[" FILENAME "]", NR }"#, &[], ""),
        "[-] 0\n"
    );
    // A plain getline in BEGIN opens the main input, so FILENAME is set then.
    let out = run_operands(
        r#"BEGIN { getline; print "[" FILENAME "]" $0 }"#,
        &[],
        "x\n",
    );
    assert_eq!(out, "[-]x\n");
}

#[test]
fn filename_is_still_unset_in_begin() {
    // "Inside a BEGIN action FILENAME shall be unset."
    assert_eq!(
        run(r#"BEGIN { print "[" FILENAME "]" }"#, "x\n").out,
        "[]\n"
    );
}
