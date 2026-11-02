//! End-to-end tests: run the real `hawk` binary and check what it prints.
//!
//! Cargo builds the binary before running files in `tests/` and tells us
//! where it is through the `CARGO_BIN_EXE_hawk` environment variable.

use std::io::Write;
use std::process::{Command, Stdio};

/// Run hawk with `args`, feeding `input` on stdin.
/// Returns (stdout, stderr, exit status).
fn hawk(args: &[&str], input: &str) -> (String, String, i32) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_hawk"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("hawk runs");
    // Write stdin then close it (dropping the handle) so hawk sees EOF.
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input.as_bytes()).unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// Just the standard output, asserting success.
fn out(args: &[&str], input: &str) -> String {
    let (o, e, rc) = hawk(args, input);
    assert_eq!((rc, e.as_str()), (0, ""), "args {args:?}");
    o
}

#[test]
fn begin_and_end() {
    assert_eq!(out(&["BEGIN { print 1+2, \"a\" \"b\" }"], ""), "3 ab\n");
    assert_eq!(out(&["END { print NR }"], "a\nb\nc\n"), "3\n");
}

#[test]
fn fields_and_records() {
    assert_eq!(out(&["{ print $2, NF }"], "a b c\n  d  e\n"), "b 3\ne 2\n");
    assert_eq!(
        out(&["{ $3 = \"x\"; print; print NF }"], "a\n"),
        "a  x\n3\n"
    );
    assert_eq!(out(&["{ NF = 2 } 1"], "a b c\n"), "a b\n");
    assert_eq!(out(&["-F:", "{ print $2 }"], "a:b:c\n"), "b\n");
    assert_eq!(out(&["-F", ",", "{ print $3 }"], "1,2,3\n"), "3\n");
    assert_eq!(
        out(
            &["BEGIN { RS = \"\" } { print NR \": \" $1 \"/\" $NF }"],
            "a b\nc\n\n\nd\n"
        ),
        "1: a/c\n2: d/d\n"
    );
}

#[test]
fn assignments_from_the_command_line() {
    assert_eq!(
        out(&["-v", "x=10", "BEGIN { print x + 1, (x < 9) }"], ""),
        "11 0\n"
    );
    assert_eq!(out(&["-vx=a\\tb", "BEGIN { print x }"], ""), "a\tb\n");
    // An operand `var=value` takes effect when the main loop reaches it.
    assert_eq!(out(&["{ print v, $0 }", "v=1", "-", "v=2"], "a\n"), "1 a\n");
    assert_eq!(out(&["END { print v }", "v=2", "/dev/null"], ""), "2\n");
}

#[test]
fn program_files_and_double_dash() {
    let dir = std::env::temp_dir().join(format!("hawk-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let a = dir.join("a.awk");
    let b = dir.join("b.awk");
    std::fs::write(&a, "function f(x) { return x * 2 }").unwrap();
    std::fs::write(&b, "BEGIN { print f(21) }").unwrap();
    let (a, b) = (a.to_str().unwrap(), b.to_str().unwrap());
    assert_eq!(out(&["-f", a, "-f", b], ""), "42\n");
    assert_eq!(out(&["--", "BEGIN { print ARGV[1] }", "-x"], ""), "-x\n");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn control_flow_and_exit() {
    let prog = "{ if ($1 == \"stop\") exit 3; if ($1 == \"skip\") next; print }
                END { print \"end\" }";
    let (o, _, rc) = hawk(&[prog], "a\nskip\nb\nstop\nc\n");
    assert_eq!((o.as_str(), rc), ("a\nb\nend\n", 3));
    // exit inside END stops at once; a bare exit keeps the earlier status.
    let (o, _, rc) = hawk(
        &["BEGIN { exit 4 } END { print \"x\"; exit; print \"y\" }"],
        "",
    );
    assert_eq!((o.as_str(), rc), ("x\n", 4));
    let loops =
        "BEGIN { for (i = 0; i < 10; i++) { if (i == 2) continue; if (i == 5) break; s = s i }
                 do s = s \".\"; while (0); while (j < 2) j++; print s, j }";
    assert_eq!(out(&[loops], ""), "0134. 2\n");
}

#[test]
fn functions_and_arrays() {
    let prog = "function fill(a, n,  i) { for (i = 1; i <= n; i++) a[i] = i * i }
                function fib(n) { return n < 2 ? n : fib(n-1) + fib(n-2) }
                BEGIN { fill(sq, 4); print length(sq), sq[3], fib(15)
                        for (k in sq) s = s k; print s; delete sq[2]; print (2 in sq), length(sq) }";
    assert_eq!(out(&[prog], ""), "4 9 610\n1234\n0 3\n");
}

#[test]
fn deep_recursion_is_an_error_not_a_crash() {
    let ok = "function r(n) { return n == 0 ? 0 : 1 + r(n - 1) } BEGIN { print r(10000) }";
    assert_eq!(out(&[ok], ""), "10000\n");
    let (_, e, rc) = hawk(&["function r(n) { return r(n + 1) } BEGIN { r(1) }"], "");
    assert_eq!(
        (e.as_str(), rc),
        (
            "hawk: function call nesting too deep (runaway recursion?)\n",
            2
        )
    );
}

#[test]
fn runtime_errors_exit_2_after_flushing_output() {
    let (o, e, rc) = hawk(&["BEGIN { print \"before\"; x = 1 / 0 }"], "");
    assert_eq!(
        (o.as_str(), e.as_str(), rc),
        ("before\n", "hawk: division by zero\n", 2)
    );
    let (_, e, rc) = hawk(&["{ print }", "/no/such/file"], "");
    assert_eq!(
        (e.as_str(), rc),
        (
            "hawk: cannot open /no/such/file (No such file or directory)\n",
            2
        )
    );
}

#[test]
fn usage_errors() {
    let usage = "usage: hawk [-F fs][-v var=value][-f progfile | 'prog'][file ...]\n";
    let (_, e, rc) = hawk(&[], "");
    assert_eq!((e, rc), (format!("hawk: no program given\n{usage}"), 2));
    let (_, e, rc) = hawk(&["-q", "BEGIN{}"], "");
    assert_eq!((e, rc), (format!("hawk: unknown option -q\n{usage}"), 2));
    let (_, e, rc) = hawk(&["-v", "1x=2", "BEGIN{}"], "");
    assert_eq!(
        (e, rc),
        (format!("hawk: improper assignment: -v 1x=2\n{usage}"), 2)
    );
}

#[test]
fn output_redirection_and_pipes() {
    let prog = "BEGIN { print \"b\" | \"sort\"; print \"a\" | \"sort\"; close(\"sort\");
                \"echo hi\" | getline x; print x; print system(\"exit 3\") }";
    assert_eq!(out(&[prog], ""), "a\nb\nhi\n3\n");
}

#[test]
fn getline_result_in_arithmetic() {
    // (cmd | getline x) + 1, as gawk and mawk; pyawk v1.0 rejects it.
    let prog = "BEGIN { y = \"echo 7\" | getline z + 1; print y, z }";
    assert_eq!(out(&[prog], ""), "2 7\n");
}

#[test]
fn bare_printf_prints_nothing() {
    assert_eq!(out(&["BEGIN { printf; print \"X\" }"], ""), "X\n");
}

#[test]
fn rand_matches_pyawk() {
    // pyawk uses Python's random.Random(0), so does hawk.
    assert_eq!(
        out(
            &["BEGIN { print rand(); srand(1); print rand(), srand() }"],
            ""
        ),
        "0.844422\n0.134364 1\n"
    );
}

#[test]
fn regex_matching_and_patterns() {
    let prog = r#"/ba/ { print "y" } $2 !~ "^f" { print "n" } { print ($0 ~ "a.c") }"#;
    assert_eq!(out(&[prog], "foo bar\nabc foo\n"), "y\nn\n0\n1\n");
    assert_eq!(
        out(
            &[r#"{ print match($0, /o+/), RSTART, RLENGTH; print match($0, "z") RLENGTH }"#],
            "foo\n"
        ),
        "2 2 2\n0-1\n"
    );
}

#[test]
fn sub_and_gsub() {
    let prog = r#"{ n = gsub(/o/, "<&>"); print n, $0, NF
                    x = "aaa"; sub(/a/, "\\&", x); print x
                    y = "abxd"; gsub(/x*/, "-", y); print y
                    a[1] = "q"; i = 1; sub(/q/, "Q", a[i++]); print a[1], i
                    print gsub(/z/, "", $1), $0 }"#;
    assert_eq!(
        out(&[prog], "foo bar\n"),
        "2 f<o><o> bar 2\n&aa\n-a-b-d-\nQ 2\n0 f<o><o> bar\n"
    );
}

#[test]
fn regex_split_fs_and_rs() {
    assert_eq!(
        out(&["-F", ":+", "{ print $2, NF }"], "a::b:::c\n"),
        "b 3\n"
    );
    assert_eq!(out(&["-F", "|", "{ print $2 }"], "a|b\n"), "b\n");
    assert_eq!(
        out(
            &[r#"{ n = split($0, A, /[,;]+/); print n, A[1], A[3] }"#],
            "x,,y;z\n"
        ),
        "3 x z\n"
    );
    assert_eq!(
        out(
            &[r#"BEGIN { RS = "[0-9]+" } { printf "%s.", $0 }"#],
            "a1b22c333d"
        ),
        "a.b.c.d."
    );
}

#[test]
fn bad_regex_is_a_runtime_error() {
    let (o, e, rc) = hawk(&[r#"{ print "x" ~ "a{2,1}" }"#], "\n");
    assert_eq!((o.as_str(), rc), ("", 2));
    assert!(
        e.contains("bad regular expression /a{2,1}/: interval {n,m} with m < n"),
        "{e}"
    );
}
