//! Reading records: the input is cut into records at RS.
//!
//! The program may change RS between records, so the reader cannot just
//! iterate over lines.  It keeps a buffer of bytes read but not yet
//! consumed, and cuts the next record off the front each time
//! `read_record` is called (pyawk's `RecordReader`).
//!
//! RS rules:
//! * one byte, e.g. "\n" or ";": that byte ends a record; a last record
//!   without the terminator still counts;
//! * "" (paragraph mode): records are separated by blank lines; leading
//!   newlines are skipped and trailing newlines dropped;
//! * anything longer: a regular expression (gawk and mawk do this).

use std::io::Read;

use crate::error::Error;

/// How many bytes to ask the operating system for at a time.
const CHUNK: usize = 64 * 1024;

/// Finds the separator of a regex RS: given the buffer and a start offset,
/// returns the `(start, end)` of the first non-empty match at or after it.
/// (A trait object so this module needs no regex engine of its own.)
pub type RegexFinder<'a> = &'a dyn Fn(&[u8], usize) -> Option<(usize, usize)>;

/// A record source: any byte stream (`Box<dyn Read>` can hold a file, the
/// standard input or a pipe from a command).
pub struct RecordReader {
    source: Box<dyn Read>,
    buf: Vec<u8>,
    /// Where the unconsumed part of `buf` starts.
    start: usize,
    eof: bool,
}

impl RecordReader {
    pub fn new(source: Box<dyn Read>) -> RecordReader {
        RecordReader {
            source,
            buf: Vec::new(),
            start: 0,
            eof: false,
        }
    }

    /// Read more data into the buffer; `Ok(false)` at the end of input.
    fn fill(&mut self) -> Result<bool, Error> {
        if self.eof {
            return Ok(false);
        }
        // Drop the consumed part first so the buffer does not grow forever.
        if self.start > 0 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        let old = self.buf.len();
        self.buf.resize(old + CHUNK, 0);
        loop {
            match self.source.read(&mut self.buf[old..]) {
                Ok(n) => {
                    self.buf.truncate(old + n);
                    if n == 0 {
                        self.eof = true;
                    }
                    return Ok(n > 0);
                }
                // A signal interrupted the read: just try again.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.buf.truncate(old);
                    self.eof = true;
                    return Err(Error::Runtime(format!(
                        "read error ({})",
                        crate::io::strerror(&e)
                    )));
                }
            }
        }
    }

    /// Take `buf[start..end]` as the record and move past `skip` more bytes.
    fn take(&mut self, end: usize, skip: usize) -> Vec<u8> {
        let rec = self.buf[self.start..end].to_vec();
        self.start = end + skip;
        rec
    }

    /// The next record without its separator, or `None` at the end.
    /// `regex` is needed only when RS is longer than one byte.
    pub fn read_record(
        &mut self,
        rs: &[u8],
        regex: Option<RegexFinder>,
    ) -> Result<Option<Vec<u8>>, Error> {
        match rs.len() {
            0 => self.read_paragraph(),
            1 => self.read_byte(rs[0]),
            _ => match regex {
                Some(find) => self.read_regex(find),
                None => Err(Error::Runtime("multi-character RS needs a regex".into())),
            },
        }
    }

    fn read_byte(&mut self, sep: u8) -> Result<Option<Vec<u8>>, Error> {
        let mut scan = self.start;
        loop {
            // `position` finds the first index where the test is true.
            if let Some(k) = self.buf[scan..].iter().position(|&b| b == sep) {
                return Ok(Some(self.take(scan + k, 1)));
            }
            let scanned = self.buf.len() - self.start;
            if !self.fill()? {
                break;
            }
            scan = self.start + scanned;
        }
        Ok(self.rest())
    }

    /// Whatever is left (the last, unterminated record), if anything.
    fn rest(&mut self) -> Option<Vec<u8>> {
        if self.start < self.buf.len() {
            let rec = self.buf[self.start..].to_vec();
            self.buf.clear();
            self.start = 0;
            Some(rec)
        } else {
            None
        }
    }

    fn read_paragraph(&mut self) -> Result<Option<Vec<u8>>, Error> {
        loop {
            while self.start < self.buf.len() && self.buf[self.start] == b'\n' {
                self.start += 1;
            }
            if self.start < self.buf.len() || !self.fill()? {
                break;
            }
        }
        if self.start >= self.buf.len() {
            return Ok(None);
        }
        let mut scan = self.start;
        loop {
            if let Some(k) = self.buf[scan..].windows(2).position(|w| w == b"\n\n") {
                return Ok(Some(self.take(scan + k, 2)));
            }
            // Search again from one byte back: "\n" | "\n" may straddle reads.
            let scanned = (self.buf.len() - self.start).saturating_sub(1);
            if !self.fill()? {
                break;
            }
            scan = self.start + scanned;
        }
        let mut rec = self.rest().unwrap_or_default();
        while rec.last() == Some(&b'\n') {
            rec.pop();
        }
        Ok(Some(rec))
    }

    /// Regex RS: as pyawk does, read everything first (a match could
    /// otherwise be cut short at a chunk boundary).
    fn read_regex(&mut self, find: RegexFinder) -> Result<Option<Vec<u8>>, Error> {
        while self.fill()? {}
        if self.start >= self.buf.len() {
            return Ok(None);
        }
        match find(&self.buf, self.start) {
            Some((s, e)) => Ok(Some(self.take(s, e - s))),
            None => Ok(self.rest()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reader(text: &str) -> RecordReader {
        // `Cursor` turns an in-memory byte vector into a `Read` stream.
        RecordReader::new(Box::new(std::io::Cursor::new(text.as_bytes().to_vec())))
    }

    fn all(text: &str, rs: &str) -> Vec<String> {
        let mut r = reader(text);
        let mut out = Vec::new();
        while let Some(rec) = r.read_record(rs.as_bytes(), None).unwrap() {
            out.push(String::from_utf8(rec).unwrap());
        }
        out
    }

    #[test]
    fn newline_records() {
        assert_eq!(all("a\nb\n", "\n"), ["a", "b"]);
        assert_eq!(all("a\nb", "\n"), ["a", "b"]);
        assert_eq!(all("a\n\nb\n", "\n"), ["a", "", "b"]);
        assert!(all("", "\n").is_empty());
        assert_eq!(all("x;y;", ";"), ["x", "y"]);
    }

    #[test]
    fn paragraph_mode() {
        assert_eq!(all("\n\na\nb\n\n\nc\n\n", ""), ["a\nb", "c"]);
        assert_eq!(all("a\n", ""), ["a"]);
        assert!(all("\n\n\n", "").is_empty());
    }

    #[test]
    fn long_input_crosses_chunks() {
        let line = "x".repeat(1000);
        let text = format!("{line}\n").repeat(200);
        let recs = all(&text, "\n");
        assert_eq!(recs.len(), 200);
        assert!(recs.iter().all(|r| r == &line));
    }

    #[test]
    fn rs_can_change_between_records() {
        let mut r = reader("a;b\nc\n");
        assert_eq!(r.read_record(b";", None).unwrap().unwrap(), b"a");
        assert_eq!(r.read_record(b"\n", None).unwrap().unwrap(), b"b");
        assert_eq!(r.read_record(b"\n", None).unwrap().unwrap(), b"c");
        assert_eq!(r.read_record(b"\n", None).unwrap(), None);
    }
}
