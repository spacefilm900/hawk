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
        .env("LC_ALL", "C") // C mode unless a test says otherwise
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
fn gsub_of_dollar0_does_not_change_a_saved_copy() {
    // The result of gsub is a string, so "1" does not compare as a
    // number.  A copy taken before gsub keeps the original bytes.
    assert_eq!(
        out(
            &["{ gsub(/a/, \"1.0\"); print ($0 == 1), ($0 == \"1.0\") }"],
            "a\n"
        ),
        "0 1\n"
    );
    assert_eq!(
        out(&["{ x = $0; gsub(/a/, \"A\"); print x, $0 }"], "ab a\n"),
        "ab a Ab A\n"
    );
    assert_eq!(
        out(&["{ print sub(/a/, \"A\"), $0 }"], "ab a\n"),
        "1 Ab a\n"
    );
}

#[test]
fn array_increment_keeps_field_text_and_insertion_order() {
    // The key is the field's text ("010", not 10) and ++ yields the old
    // number. for-in walks in insertion order.
    assert_eq!(
        out(
            &["{ c[$1]++ } END { for (k in c) print k, c[k] }"],
            "010\nb\n010\n"
        ),
        "010 2\nb 1\n"
    );
    assert_eq!(out(&["{ print ++c[$1], c[$1] }"], "a\n"), "1 1\n");
}

#[test]
fn command_line_assignments_of_the_same_length_stay_distinct() {
    // Two -v names of the same length are built in a temporary that the
    // allocator may reuse. Each assignment has to stick to its own name.
    assert_eq!(
        out(&["-v", "aa=1", "-v", "bb=2", "BEGIN { print aa, bb }"], ""),
        "1 2\n"
    );
}

#[test]
fn nested_array_increment_keeps_the_outer_key() {
    // f's own c[$1]++ must not become part of the key of a[f($2)]++.
    assert_eq!(
        out(
            &[
                "function f(x) { c[$1]++; return x } { a[f($2)]++ } END { for (k in a) print k, a[k]; for (k in c) print k, c[k] }"
            ],
            "foo bar\n"
        ),
        "bar 1\nfoo 1\n"
    );
}

#[test]
fn printf_evaluates_arguments_the_format_does_not_use() {
    assert_eq!(
        out(
            &[
                "BEGIN { printf \"x\", i++; print \"i=\" i; s = sprintf(\"y\", i++); print \"s=\" s, \"i=\" i }"
            ],
            ""
        ),
        "xi=1\ns=y i=2\n"
    );
}

#[test]
fn gsub_in_place_uses_the_current_fs() {
    assert_eq!(
        out(&["{ FS=\":\"; gsub(/a/, \"b\"); print $1 }"], "a:x\n"),
        "b\n"
    );
}

#[test]
fn a_function_sees_the_latest_global() {
    // The action and END name `s` separately, and a function reads the
    // map rather than the expression that last wrote it.
    assert_eq!(
        out(
            &["function f(x) { return x + 1 } { s += $1 } END { print f(s), s }"],
            "3\n4\n"
        ),
        "8 7\n"
    );
}

#[test]
fn arithmetic_on_fields_keeps_strnum_text() {
    // Summing uses only the number.  Printing the field still shows the
    // original text, and a non-numeric field contributes 0.
    assert_eq!(
        out(&["{ s += $2 } END { print s }"], "a 1.5 b\na 2 b\na x b\n"),
        "3.5\n"
    );
    assert_eq!(out(&["{ print ($2 == 10), $2 }"], "a 10.0 b\n"), "1 10.0\n");
    assert_eq!(
        out(&["{ s += $1; t = t $1 } END { print s, t }"], "10\n010\n"),
        "20 10010\n"
    );
}

#[test]
fn specials_round_trip_and_numeric_fs_uses_current_convfmt() {
    // A string assigned to NR stays a string until a record bumps it,
    // and the bump is arithmetic: "hello" counts as 0, so the first
    // record makes NR 1.
    assert_eq!(
        out(
            &["BEGIN { NR = \"hello\"; FS = \":\"; OFS = \",\" } END { print NR, FS }"],
            ""
        ),
        "hello,:\n"
    );
    assert_eq!(
        out(&["BEGIN { NR = \"hello\" } { print NR }"], "a\n"),
        "1\n"
    );
    // FS assigned as a number is stringified with the CONVFMT in force
    // when the record is split, including a CONVFMT set afterwards.
    assert_eq!(
        out(
            &["BEGIN { FS = 1/3; CONVFMT = \"%.2f\" } { print NF, $1, $2 }"],
            "a0.33b\n"
        ),
        "2 a b\n"
    );
    assert_eq!(
        out(
            &["BEGIN { CONVFMT = \"%.2f\"; FS = 1/3 } { print NF, $1, $2 }"],
            "a0.33b\n"
        ),
        "2 a b\n"
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

#[test]
fn failed_writes_to_files_are_reported() {
    // /dev/full accepts the open but fails every write (Linux).  The output
    // is small, so it sits in hawk's buffer until the file is closed; the
    // failure must still be reported, as gawk, mawk and onetrue-awk do.
    if !std::path::Path::new("/dev/full").exists() {
        return;
    }
    let msg = "hawk: write error on /dev/full (No space left on device)\n";
    let (_, e, rc) = hawk(&["BEGIN { print \"x\" > \"/dev/full\" }"], "");
    assert_eq!((e.as_str(), rc), (msg, 2));
    let (o, e, rc) = hawk(
        &["BEGIN { print \"x\" > \"/dev/full\"; close(\"/dev/full\"); print \"after\" }"],
        "",
    );
    assert_eq!((o.as_str(), e.as_str(), rc), ("", msg, 2));
}

#[test]
fn runs_when_the_big_stack_cannot_be_reserved() {
    // Under a small address-space limit the 1 GiB interpreter stack cannot
    // be reserved; hawk must fall back to a smaller one, not give up.
    let script = format!(
        "ulimit -v 600000 && exec '{}' 'BEGIN {{ print 1 }}'",
        env!("CARGO_BIN_EXE_hawk")
    );
    let out = Command::new("sh").args(["-c", &script]).output().unwrap();
    let (o, e) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(
        (o.as_ref(), e.as_ref(), out.status.code()),
        ("1\n", "", Some(0))
    );
}

#[test]
fn version_is_1_2_1() {
    // `--version` prints the version from Cargo.toml; v1.1 added the pyawk
    // v1.1 features, v1.2 was the performance round, and v1.2.1 is the
    // FILENAME "-" (standard input) fix plus GitHub Actions CI.
    assert_eq!(out(&["--version"], ""), "hawk 1.2.1\n");
}

#[test]
fn next_inside_a_function() {
    // The call may sit inside an expression or a pattern; `next` still
    // abandons the rest of the rules for this record.
    let prog = "function f() { next } $1 == \"b\" { x = 1 + f() } { print } END { print \"end\" }";
    assert_eq!(out(&[prog], "a\nb\nc\n"), "a\nc\nend\n");
    let prog = "function skip() { if ($1 == \"b\") next; return 1 } skip() { print }";
    assert_eq!(out(&[prog], "a\nb\nc\n"), "a\nc\n");
    // Called from BEGIN or END there is no record to skip: a fatal error.
    let (o, e, code) = hawk(
        &["function f() { next } BEGIN { print 1; f(); print 2 }"],
        "",
    );
    assert_eq!((o.as_str(), code), ("1\n", 2));
    assert!(e.contains("next called from a function in BEGIN"), "{e}");
    let (_, e, code) = hawk(&["function f() { next } END { f() }"], "x\n");
    assert_eq!(code, 2);
    assert!(e.contains("next called from a function in END"), "{e}");
}

#[test]
fn nf_assigned_on_the_command_line() {
    // `-v NF=3` sets NF before any record; BEGIN sees 3, and reading a
    // record recomputes it (pyawk v1.1 settled this; it must not crash).
    assert_eq!(
        out(
            &[
                "-v",
                "NF=3",
                "BEGIN { print NF; $0 = \"x y\"; print NF } { print NF; print }"
            ],
            "a b\n"
        ),
        "3\n2\n2\na b\n"
    );
    // An operand assignment is made just before the next file is read.
    assert_eq!(out(&["{ print NF }", "NF=5", "-"], "a b\n"), "2\n");
}

#[test]
fn deep_nesting_is_an_error() {
    // Too long for one command-line argument (Linux allows 128 KiB), so
    // the program goes in a file.
    let deep = format!(
        "BEGIN {{ x = {}1{}; print x }}",
        "(".repeat(200_000),
        ")".repeat(200_000)
    );
    let path = std::env::temp_dir().join(format!("hawk-deep-{}.awk", std::process::id()));
    std::fs::write(&path, deep).unwrap();
    let (o, e, rc) = hawk(&["-f", path.to_str().unwrap()], "");
    std::fs::remove_file(&path).unwrap();
    assert_eq!((o.as_str(), rc), ("", 2));
    assert_eq!(e, "hawk: program is nested too deeply\n");
}

#[test]
fn nextfile_skips_the_rest_of_the_file() {
    let dir = std::env::temp_dir().join(format!("hawk-nextfile-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let a = dir.join("a");
    let b = dir.join("b");
    std::fs::write(&a, "a1\na2\na3\n").unwrap();
    std::fs::write(&b, "b1\nb2\n").unwrap();
    let (a, b) = (a.to_str().unwrap(), b.to_str().unwrap());
    // FNR starts again at 1, NR does not; END still runs.
    assert_eq!(
        out(
            &[
                "FNR == 2 { nextfile } { print FNR, NR, $0 } END { print NR }",
                a,
                b
            ],
            ""
        ),
        "1 1 a1\n1 3 b1\n4\n"
    );
    // nextfile on stdin uses it up: a second "-" reads nothing.
    assert_eq!(
        out(&["{ print; nextfile }", "-", a, "-"], "s1\ns2\n"),
        "s1\na1\n"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// ENVIRON values are strnums: ` 42 ` and `1e2` compare as numbers, but
/// hex (`0x1A`) is not a number, so it compares as a string (onetrue and
/// mawk; gawk --posix reads hex). Values are not escape-processed, and a
/// child process sees the original environment even after ENVIRON changes.
#[test]
fn environ_values_are_strnums_but_hex_is_a_string() {
    let prog = r#"BEGIN {
        print (ENVIRON["SP"] == 42), (ENVIRON["EX"] == 100), (ENVIRON["HX"] == 26), (ENVIRON["HX"] < 5)
        print ENVIRON["ESC"]
        ENVIRON["SP"] = "changed"; system("echo \"[$SP]\"")
    }"#;
    let out = Command::new(env!("CARGO_BIN_EXE_hawk"))
        .env("LC_ALL", "C") // C mode unless a test says otherwise
        .arg(prog)
        .env("SP", " 42 ")
        .env("EX", "1e2")
        .env("HX", "0x1A")
        .env("ESC", r"a\tb")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "1 1 0 1\na\\tb\n[ 42 ]\n"
    );
    assert!(out.status.success());
}

/// A directory operand cannot be opened: the same message as a missing
/// file, exit 2 (pyawk, mawk, gawk; onetrue skips it).
#[test]
fn a_directory_operand_is_an_error() {
    let (o, e, rc) = hawk(&["{ print }", "/"], "");
    assert_eq!(
        (o.as_str(), e.as_str(), rc),
        ("", "hawk: cannot open / (Is a directory)\n", 2)
    );
}
