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
use std::rc::Rc;

use crate::error::Error;
use crate::regex::Transcoding;

/// First `needle` in `hay`, or `None`.
///
/// Eight bytes at a time: XOR the needle across a word and look for a
/// zero byte. The word is built little-endian, so the lowest byte is
/// `hay[i]` on every machine and `trailing_zeros` counts from there.
fn find_byte(hay: &[u8], needle: u8) -> Option<usize> {
    let mut i = 0;
    let n = hay.len();
    let broadcast = u64::from_le_bytes([needle; 8]);
    while i + 8 <= n {
        let w = u64::from_le_bytes(hay[i..i + 8].try_into().unwrap());
        let x = w ^ broadcast;
        let m = x.wrapping_sub(0x0101_0101_0101_0101) & !x & 0x8080_8080_8080_8080;
        if m != 0 {
            return Some(i + (m.trailing_zeros() as usize / 8));
        }
        i += 8;
    }
    while i < n {
        if hay[i] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// How many bytes to ask the operating system for at a time.
const CHUNK: usize = 64 * 1024;

/// Finds the separator of a regex RS: given the buffer and a start offset,
/// returns the `(start, end)` of the first match at or after it.  The
/// third argument is the reader's cache for the buffer (see
/// `Finder::cached`), so work on the whole buffer is done once, not once
/// per record.  (A trait object so this module needs no regex engine of
/// its own.)
pub type RegexFinder<'a> = &'a dyn Fn(&[u8], usize, &mut Transcoding) -> Option<(usize, usize)>;

/// A record source: any byte stream (`Box<dyn Read>` can hold a file, the
/// standard input or a pipe from a command).
pub struct RecordReader {
    source: Box<dyn Read>,
    buf: Vec<u8>,
    /// Where the unconsumed part of `buf` starts.
    start: usize,
    eof: bool,
    /// For a regex RS in UTF-8 mode: the transcoded copy of `buf` that a
    /// regex matching invalid bytes searches, made once for all records.
    /// Cleared whenever `buf` changes.
    trans: Transcoding,
}

impl RecordReader {
    pub fn new(source: Box<dyn Read>) -> RecordReader {
        RecordReader {
            source,
            buf: Vec::new(),
            start: 0,
            eof: false,
            trans: Transcoding::default(),
        }
    }

    /// Make this input look finished: drop what is buffered and read no
    /// more.  The rest is not read first, which could wait forever on a
    /// terminal.
    pub fn use_up(&mut self) {
        self.buf.clear();
        self.trans.clear();
        self.start = 0;
        self.eof = true;
    }

    /// Read more data into the buffer; `Ok(false)` at the end of input.
    fn fill(&mut self) -> Result<bool, Error> {
        if self.eof {
            return Ok(false);
        }
        self.trans.clear();
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

    /// Copy `buf[start..end]` into `out` and move past `skip` more bytes.
    ///
    /// `out` is the caller's record buffer.  The interpreter reuses one
    /// `Vec` for every record, so this copy does not allocate once that
    /// buffer has grown to the longest line.
    fn take_into(&mut self, end: usize, skip: usize, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.buf[self.start..end]);
        self.start = end + skip;
    }

    /// The next record, appended to `out` (which is cleared first).
    /// `Ok(false)` at the end of the input.  `regex` is needed only when
    /// RS is longer than one byte.
    pub fn read_record_into(
        &mut self,
        rs: &[u8],
        regex: Option<RegexFinder>,
        out: &mut Vec<u8>,
    ) -> Result<bool, Error> {
        out.clear();
        match rs.len() {
            0 => self.read_paragraph_into(out),
            1 => self.read_byte_into(rs[0], out),
            _ => match regex {
                Some(find) => self.read_regex_into(find, out),
                None => Err(Error::Runtime("multi-character RS needs a regex".into())),
            },
        }
    }

    /// The next record without its separator, or `None` at the end.
    /// `regex` is needed only when RS is longer than one byte.
    pub fn read_record(
        &mut self,
        rs: &[u8],
        regex: Option<RegexFinder>,
    ) -> Result<Option<Rc<[u8]>>, Error> {
        let mut rec = Vec::new();
        if self.read_record_into(rs, regex, &mut rec)? {
            Ok(Some(Rc::from(rec.as_slice())))
        } else {
            Ok(None)
        }
    }

    fn read_byte_into(&mut self, sep: u8, out: &mut Vec<u8>) -> Result<bool, Error> {
        let mut scan = self.start;
        loop {
            if let Some(k) = find_byte(&self.buf[scan..], sep) {
                self.take_into(scan + k, 1, out);
                return Ok(true);
            }
            let scanned = self.buf.len() - self.start;
            if !self.fill()? {
                break;
            }
            scan = self.start + scanned;
        }
        Ok(self.rest_into(out))
    }

    /// Whatever is left (the last, unterminated record), if anything.
    /// `true` when there was something to copy.
    fn rest_into(&mut self, out: &mut Vec<u8>) -> bool {
        if self.start < self.buf.len() {
            out.extend_from_slice(&self.buf[self.start..]);
            self.buf.clear();
            self.trans.clear();
            self.start = 0;
            true
        } else {
            false
        }
    }

    fn read_paragraph_into(&mut self, out: &mut Vec<u8>) -> Result<bool, Error> {
        loop {
            while self.start < self.buf.len() && self.buf[self.start] == b'\n' {
                self.start += 1;
            }
            if self.start < self.buf.len() || !self.fill()? {
                break;
            }
        }
        if self.start >= self.buf.len() {
            return Ok(false);
        }
        let mut scan = self.start;
        loop {
            if let Some(k) = self.buf[scan..].windows(2).position(|w| w == b"\n\n") {
                self.take_into(scan + k, 2, out);
                return Ok(true);
            }
            // Search again from one byte back: "\n" | "\n" may straddle reads.
            let scanned = (self.buf.len() - self.start).saturating_sub(1);
            if !self.fill()? {
                break;
            }
            scan = self.start + scanned;
        }
        if !self.rest_into(out) {
            return Ok(false);
        }
        while out.last() == Some(&b'\n') {
            out.pop();
        }
        Ok(true)
    }

    /// Regex RS: as pyawk does, read everything first (a match could
    /// otherwise be cut short at a chunk boundary).  Empty matches do not
    /// separate records.
    fn read_regex_into(&mut self, find: RegexFinder, out: &mut Vec<u8>) -> Result<bool, Error> {
        while self.fill()? {}
        if self.start >= self.buf.len() {
            return Ok(false);
        }
        let mut pos = self.start;
        while let Some((s, e)) = find(&self.buf, pos, &mut self.trans) {
            if e > s {
                self.take_into(s, e - s, out);
                return Ok(true);
            }
            pos = s + 1;
            if pos > self.buf.len() {
                break;
            }
        }
        Ok(self.rest_into(out))
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
            out.push(String::from_utf8(rec.to_vec()).unwrap());
        }
        out
    }

    #[test]
    fn regex_records_skip_empty_matches() {
        // A finder for "x*": empty matches everywhere, real ones at x runs.
        let find = |t: &[u8], p: usize, _: &mut Transcoding| -> Option<(usize, usize)> {
            let s = p;
            let e = t[s..].iter().take_while(|&&b| b == b'x').count() + s;
            Some((s, e))
        };
        let mut r = reader("axxbxc");
        let mut got = Vec::new();
        while let Some(rec) = r.read_record(b"x*", Some(&find)).unwrap() {
            got.push(String::from_utf8(rec.to_vec()).unwrap());
        }
        assert_eq!(got, ["a", "b", "c"]);
    }

    #[test]
    fn find_byte_matches_a_plain_scan() {
        // Every alignment, including the tail shorter than a word, and a
        // needle in the high bit (the zero-byte trick can mishandle that).
        for needle in [b'\n', b' ', 0u8, 0x80, 0xff] {
            for lead in 0..16 {
                let mut hay = vec![needle ^ 1; lead + 40];
                for at in [0, 1, 7, 8, 9, 15, lead, lead + 3, hay.len() - 1] {
                    if at >= hay.len() {
                        continue;
                    }
                    hay[at] = needle;
                    assert_eq!(find_byte(&hay, needle), Some(at), "needle {needle} at {at}");
                    hay[at] = needle ^ 1;
                }
                assert_eq!(find_byte(&hay, needle), None);
            }
        }
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
        assert_eq!(&r.read_record(b";", None).unwrap().unwrap()[..], b"a");
        assert_eq!(&r.read_record(b"\n", None).unwrap().unwrap()[..], b"b");
        assert_eq!(&r.read_record(b"\n", None).unwrap().unwrap()[..], b"c");
        assert_eq!(r.read_record(b"\n", None).unwrap(), None);
    }
}
