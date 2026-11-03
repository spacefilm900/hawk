//! Output and input streams by name: `print > "file"`, `print | "cmd"`,
//! `getline < "file"`, `"cmd" | getline`, `close()`, `system()`.
//!
//! In AWK the file name or the command text *is* the handle: `print > "f"`
//! opens f once and keeps writing to it until `close("f")`.  So the streams
//! are kept in maps from name to open stream (pyawk's `IOManager`).
//!
//! Commands run through `/bin/sh -c`, as `system(3)` and `popen(3)` do.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, IsTerminal, Write};
use std::os::unix::ffi::OsStrExt; // `OsStr::from_bytes`: Unix names are bytes
use std::os::unix::process::ExitStatusExt; // `.signal()` on exit statuses
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};

use crate::ast::Redirect;
use crate::error::Error;
use crate::input::RecordReader;

/// The operating system's message for an I/O error, without the
/// " (os error 2)" that Rust appends: "No such file or directory".
pub fn strerror(e: &io::Error) -> String {
    let text = e.to_string();
    match text.find(" (os error") {
        Some(k) => text[..k].to_string(),
        None => text,
    }
}

/// A command's exit status the way awk reports it: the exit code, or
/// 256 + the signal number if a signal killed it (pyawk and gawk).
pub fn wait_status(status: ExitStatus) -> f64 {
    match (status.code(), status.signal()) {
        (Some(code), _) => code as f64,
        (None, Some(sig)) => (256 + sig) as f64,
        _ => 0.0,
    }
}

/// A shell command for `text` (`sh -c text`).
fn shell(text: &[u8]) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(OsStr::from_bytes(text));
    cmd
}

/// An open output: a file, or the input side of a command's pipe.
enum Output {
    File(BufWriter<File>),
    Pipe(Child, BufWriter<ChildStdin>),
}

impl Output {
    fn writer(&mut self) -> &mut dyn Write {
        match self {
            Output::File(w) => w,
            Output::Pipe(_, w) => w,
        }
    }
}

/// An open input for getline: a file, or a command's output.
struct Input {
    reader: RecordReader,
    child: Option<Child>,
}

/// Everything the program reads or writes besides the main input.
pub struct Io {
    /// Standard output, buffered: AWK programs print a lot, and one system
    /// call per line would be slow.  It is flushed before running a command
    /// and at the end.
    pub stdout: BufWriter<io::Stdout>,
    /// Standard output is a terminal: flush every write so that output
    /// appears as soon as it is printed (as C stdio's line buffering does).
    interactive: bool,
    /// The one reader for standard input, shared by the main input and
    /// `getline < "-"` so that neither loses data the other buffered.
    pub stdin: Option<RecordReader>,
    outputs: HashMap<Vec<u8>, Output>,
    inputs: HashMap<Vec<u8>, Input>,
}

/// Error type for writes: a broken pipe on standard output ends the run
/// quietly (`head` closing its input is not a mistake worth reporting).
pub fn write_error(e: io::Error, name: &[u8]) -> Error {
    if e.kind() == io::ErrorKind::BrokenPipe {
        Error::BrokenPipe
    } else {
        Error::Runtime(format!(
            "write error on {} ({})",
            String::from_utf8_lossy(name),
            strerror(&e)
        ))
    }
}

impl Default for Io {
    fn default() -> Self {
        Self::new()
    }
}

impl Io {
    pub fn new() -> Io {
        Io {
            stdout: BufWriter::with_capacity(64 * 1024, io::stdout()),
            interactive: io::stdout().is_terminal(),
            stdin: None,
            outputs: HashMap::new(),
            inputs: HashMap::new(),
        }
    }

    /// The shared standard-input reader, created on first use.
    pub fn stdin_reader(&mut self) -> &mut RecordReader {
        // `get_or_insert_with` fills the `Option` the first time only.
        self.stdin
            .get_or_insert_with(|| RecordReader::new(Box::new(io::stdin())))
    }

    /// Write `data` to standard output.
    pub fn write_stdout(&mut self, data: &[u8]) -> Result<(), Error> {
        let mut result = self.stdout.write_all(data);
        if self.interactive && result.is_ok() {
            result = self.stdout.flush();
        }
        result.map_err(|e| write_error(e, b"/dev/stdout"))
    }

    /// Write `data` to the redirection `kind name`, opening it the first
    /// time.  "/dev/stdout" and "-" mean standard output, "/dev/stderr"
    /// standard error.
    pub fn write_to(&mut self, kind: Redirect, name: &[u8], data: &[u8]) -> Result<(), Error> {
        if !self.outputs.contains_key(name) {
            match name {
                b"/dev/stdout" | b"-" => return self.write_stdout(data),
                b"/dev/stderr" => {
                    // Keep the order of stdout and stderr text sensible.
                    let _ = self.stdout.flush();
                    let _ = io::stderr().write_all(data);
                    return Ok(());
                }
                _ => {}
            }
            let out = self.open_output(kind, name)?;
            self.outputs.insert(name.to_vec(), out);
        }
        // `unwrap` is safe: the entry was just checked or inserted.
        let out = self.outputs.get_mut(name).unwrap();
        out.writer()
            .write_all(data)
            .map_err(|e| write_error(e, name))
    }

    fn open_output(&mut self, kind: Redirect, name: &[u8]) -> Result<Output, Error> {
        let fail = |e: io::Error| {
            Error::Runtime(format!(
                "can't redirect to {} ({})",
                String::from_utf8_lossy(name),
                strerror(&e)
            ))
        };
        let path = OsStr::from_bytes(name);
        match kind {
            Redirect::Pipe => {
                self.flush_all();
                let mut child = shell(name).stdin(Stdio::piped()).spawn().map_err(fail)?;
                // `take()` moves the pipe out of the child, leaving `None`.
                let pipe = child.stdin.take().unwrap();
                Ok(Output::Pipe(child, BufWriter::new(pipe)))
            }
            Redirect::Append => {
                let f = OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(path)
                    .map_err(fail)?;
                Ok(Output::File(BufWriter::new(f)))
            }
            Redirect::Write => Ok(Output::File(BufWriter::new(
                File::create(path).map_err(fail)?,
            ))),
        }
    }

    /// The reader for `getline < name`; `None` if the file cannot be opened.
    pub fn input_file(&mut self, name: &[u8]) -> Option<&mut RecordReader> {
        if (name == b"-" || name == b"/dev/stdin") && !self.inputs.contains_key(name) {
            return Some(self.stdin_reader());
        }
        if !self.inputs.contains_key(name) {
            let f = File::open(OsStr::from_bytes(name)).ok()?;
            // A directory opens, but reading it fails: treat it as a file
            // that cannot be read, so getline returns -1 (pyawk, whose
            // open() refuses directories; gawk and mawk also give -1).
            if f.metadata().is_ok_and(|m| m.is_dir()) {
                return None;
            }
            let input = Input {
                reader: RecordReader::new(Box::new(f)),
                child: None,
            };
            self.inputs.insert(name.to_vec(), input);
        }
        self.inputs.get_mut(name).map(|i| &mut i.reader)
    }

    /// The reader for `cmd | getline`, starting the command the first time.
    pub fn input_cmd(&mut self, cmd: &[u8]) -> Result<&mut RecordReader, Error> {
        if !self.inputs.contains_key(cmd) {
            self.flush_all();
            let mut child = shell(cmd).stdout(Stdio::piped()).spawn().map_err(|e| {
                Error::Runtime(format!(
                    "can't open pipe {} ({})",
                    String::from_utf8_lossy(cmd),
                    strerror(&e)
                ))
            })?;
            let pipe = child.stdout.take().unwrap();
            let input = Input {
                reader: RecordReader::new(Box::new(pipe)),
                child: Some(child),
            };
            self.inputs.insert(cmd.to_vec(), input);
        }
        Ok(&mut self.inputs.get_mut(cmd).unwrap().reader)
    }

    /// `close(name)`: the command's exit status, 0 for a file, -1 if
    /// nothing by that name is open.  Output still in a file's buffer is
    /// written now; if that fails (a full disk, say) the data is lost, so
    /// it is an error, as in gawk, mawk and onetrue-awk.
    pub fn close(&mut self, name: &[u8]) -> Result<f64, Error> {
        let mut status = -1.0;
        if let Some(out) = self.outputs.remove(name) {
            let _ = self.stdout.flush();
            status = match out {
                Output::File(mut w) => {
                    w.flush().map_err(|e| write_error(e, name))?;
                    0.0
                }
                Output::Pipe(mut child, mut w) => {
                    let _ = w.flush();
                    drop(w); // closing the pipe lets the command see end of input
                    child.wait().map(wait_status).unwrap_or(-1.0)
                }
            };
        }
        if let Some(input) = self.inputs.remove(name) {
            drop(input.reader);
            status = match input.child {
                Some(mut child) => child.wait().map(wait_status).unwrap_or(-1.0),
                None => 0.0,
            };
        }
        Ok(status)
    }

    /// `system(cmd)`: run a command, after flushing our own output so that
    /// the two appear in order.
    pub fn system(&mut self, cmd: &[u8]) -> f64 {
        self.flush_all();
        match shell(cmd).status() {
            Ok(status) => wait_status(status),
            Err(_) => -1.0,
        }
    }

    /// Flush standard output and every open output stream.
    pub fn flush_all(&mut self) {
        let _ = self.stdout.flush();
        for out in self.outputs.values_mut() {
            let _ = out.writer().flush();
        }
    }

    /// The end of the run: close every stream, then flush standard output.
    /// Everything is closed even if one fails; the first failure is
    /// returned.
    pub fn close_all(&mut self) -> Result<(), Error> {
        let mut first_error = None;
        // Collect the names first: closing changes the maps being walked.
        let mut names: Vec<Vec<u8>> = self.outputs.keys().cloned().collect();
        names.extend(self.inputs.keys().cloned());
        for name in names {
            if let Err(e) = self.close(&name) {
                // `get_or_insert` keeps the first error and ignores later ones.
                first_error.get_or_insert(e);
            }
        }
        let flushed = self
            .stdout
            .flush()
            .map_err(|e| write_error(e, b"/dev/stdout"));
        match first_error {
            Some(e) => Err(e),
            None => flushed,
        }
    }
}
