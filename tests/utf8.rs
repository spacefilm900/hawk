//! End-to-end tests of UTF-8 mode: the binary run with LC_ALL=C.UTF-8
//! (and a few in the C locale for contrast).  Inputs and outputs are raw
//! bytes, since invalid UTF-8 must pass through unchanged.

use std::io::Write;
use std::process::{Command, Stdio};

/// Run `hawk prog` with LC_ALL set to `locale`, feeding `input`; returns
/// stdout as bytes (stderr is shown if the run fails).
fn run_in(locale: &str, prog: &str, input: &[u8]) -> Vec<u8> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_hawk"))
        .env("LC_ALL", locale)
        .arg(prog)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("hawk runs");
    child.stdin.take().unwrap().write_all(input).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{prog}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn u(prog: &str, input: &str) -> String {
    String::from_utf8(run_in("C.UTF-8", prog, input.as_bytes())).unwrap()
}

#[test]
fn the_locale_variables_choose_the_mode() {
    let prog = "{ print length($0) }";
    assert_eq!(run_in("C.UTF-8", prog, "é\n".as_bytes()), b"1\n");
    assert_eq!(run_in("en_GB.utf8", prog, "é\n".as_bytes()), b"1\n");
    assert_eq!(run_in("C", prog, "é\n".as_bytes()), b"2\n");
    // LC_ALL empty: LC_CTYPE, then LANG decide.
    let o = Command::new(env!("CARGO_BIN_EXE_hawk"))
        .env("LC_ALL", "")
        .env("LC_CTYPE", "")
        .env("LANG", "C.UTF-8")
        .args(["BEGIN { print length(\"é\") }"])
        .output()
        .unwrap();
    assert_eq!(o.stdout, b"1\n");
}

#[test]
fn string_functions_count_characters() {
    assert_eq!(
        u(
            "{ print length(), substr($0, 2, 3), index($0, \"ö\"), toupper($0) }",
            "añöbç\n"
        ),
        "5 ñöb 3 AÑÖBÇ\n"
    );
    assert_eq!(
        u("BEGIN { print tolower(\"ÀÉ STRAßE\") }", ""),
        "àé straße\n"
    );
    assert_eq!(
        u(
            "{ n = split($0, a, \"\"); print n, a[1], a[3] }",
            "日本語\n"
        ),
        "3 日 語\n"
    );
    assert_eq!(u("BEGIN { FS = \"\" } { print NF, $2 }", "éè\n"), "2 è\n");
}

#[test]
fn invalid_bytes_are_single_characters_and_survive() {
    let out = run_in(
        "C.UTF-8",
        "{ print length(), substr($0, 2, 1), toupper($0), index($0, \"b\") }",
        b"a\xffb\n",
    );
    assert_eq!(out, b"3 \xff A\xffB 3\n");
}

#[test]
fn printf_counts_characters_and_c_writes_code_points() {
    assert_eq!(
        u(
            "BEGIN { printf \"[%3s][%-3s][%.1s][%c][%2c]\\n\", \"é\", \"ñ\", \"日本\", 233, \"€x\" }",
            ""
        ),
        "[  é][ñ  ][日][é][ €]\n"
    );
    // C mode: bytes, and %c of a number is one byte.
    assert_eq!(
        run_in("C", "BEGIN { printf \"[%3s][%c]\", \"é\", 256 + 65 }", b""),
        b"[ \xc3\xa9][A]"
    );
}

#[test]
fn regexes_match_characters() {
    // `.` is one character; a repeat repeats the whole character.
    assert_eq!(u("{ gsub(/./, \"[&]\"); print }", "aé\n"), "[a][é]\n");
    assert_eq!(
        u("{ print match($0, /é+/), RSTART, RLENGTH }", "aééb\n"),
        "2 2 2\n"
    );
    // Classes use the Unicode tables; [=é=] is only é.
    assert_eq!(
        u("{ gsub(/[[:alpha:]]/, \"A\"); print }", "é1ß-\n"),
        "A1A-\n"
    );
    assert_eq!(u("{ gsub(/[[=e=]]/, \"E\"); print }", "eé\n"), "Eé\n");
    assert_eq!(u("{ gsub(/[[=é=]]/, \"E\"); print }", "eé\n"), "eE\n");
    // Escapes that spell a character are that character.
    assert_eq!(u("{ gsub(/\\303\\251/, \"E\"); print }", "aé\n"), "aE\n");
    // An empty match moves on by a whole character.
    assert_eq!(u("{ gsub(/x*/, \"-\"); print }", "aé\n"), "-a-é-\n");
    // A regex FS made of one multibyte character, and split on "".
    assert_eq!(
        u("BEGIN { FS = \"é\" } { print $2, NF }", "aébéc\n"),
        "b 3\n"
    );
    assert_eq!(
        u(
            "{ n = split($0, a, /[éb]/); print n, a[1] a[2] a[3] }",
            "aébc\n"
        ),
        "3 ac\n"
    );
}

#[test]
fn invalid_bytes_match_only_themselves() {
    let prog = "{ gsub(/[^a]/, \"<&>\"); print }";
    assert_eq!(run_in("C.UTF-8", prog, b"a\xe9b\n"), b"a\xe9<b>\n");
    let prog = "{ gsub(/\\351/, \"#\"); print }";
    assert_eq!(
        run_in("C.UTF-8", prog, b"\xe9\xc3\xa9\n"),
        "#é\n".as_bytes()
    );
    let prog = "{ print match($0, /b./), RSTART, RLENGTH }";
    assert_eq!(run_in("C.UTF-8", prog, b"b\xe9b\xc3\xa9\n"), b"3 3 2\n");
    // The C locale: every byte is a character.
    assert_eq!(run_in("C", prog, b"b\xe9b\xc3\xa9\n"), b"1 1 2\n");
}
