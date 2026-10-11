//! Text storage that never loads the opened file into memory.
//!
//! The document is a *piece table*: an ordered list of pieces, each pointing
//! either into the original file (read on demand with positioned reads) or into
//! an append-only buffer that holds everything typed or pasted.
//!
//! To find lines quickly in huge files, every buffer keeps a sparse newline
//! index: the number of `\n` bytes before every `BLOCK`-sized block. Finding a
//! line therefore needs a binary search plus a scan of at most one block.

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub const BLOCK: u64 = 64 * 1024;

// ---------------------------------------------------------------------------
// Positioned reads (cross-platform)

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, offset)
}

fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match read_at(file, buf, offset) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Sparse newline index

/// `blocks[i]` = number of newlines in `[0, i * BLOCK)`.
#[derive(Clone, Default)]
pub struct NlIndex {
    blocks: Vec<u64>,
    total: u64,
}

impl NlIndex {
    fn new() -> Self {
        Self { blocks: vec![0], total: 0 }
    }
}

// ---------------------------------------------------------------------------
// Original file

pub struct Original {
    file: File,
    len: u64,
    index: NlIndex,
    pub crlf: bool,
    pub read_error: AtomicBool,
}

impl Original {
    /// Opens a file and builds its newline index by streaming through it once.
    /// `progress` receives the number of bytes processed so far.
    pub fn open(path: &Path, progress: &AtomicU64, cancel: &AtomicBool) -> io::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        let mut index = NlIndex::new();
        let mut reader = &file;
        let mut buf = vec![0u8; BLOCK as usize * 16];
        let mut pos: u64 = 0;
        let mut crlf_votes: i64 = 0;
        while pos < len {
            if cancel.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            let want = ((len - pos) as usize).min(buf.len());
            reader.read_exact(&mut buf[..want])?;
            for chunk in buf[..want].chunks(BLOCK as usize) {
                let n = memchr::memchr_iter(b'\n', chunk).count() as u64;
                if pos < 4 * BLOCK {
                    for i in memchr::memchr_iter(b'\n', chunk) {
                        crlf_votes += if i > 0 && chunk[i - 1] == b'\r' { 1 } else { -1 };
                    }
                }
                index.total += n;
                pos += chunk.len() as u64;
                if chunk.len() as u64 == BLOCK {
                    index.blocks.push(index.total);
                }
            }
            progress.store(pos, Ordering::Relaxed);
        }
        Ok(Self {
            file,
            len,
            index,
            crlf: crlf_votes > 0,
            read_error: AtomicBool::new(false),
        })
    }

    fn read(&self, offset: u64, buf: &mut [u8]) {
        if read_exact_at(&self.file, buf, offset).is_err() {
            // The file was changed or truncated by someone else. Never crash.
            self.read_error.store(true, Ordering::Relaxed);
            buf.fill(b'?');
        }
    }
}

// ---------------------------------------------------------------------------
// Append-only buffer for inserted text

#[derive(Default)]
struct AddBuffer {
    data: Vec<u8>,
    index: NlIndex,
}

impl AddBuffer {
    fn append(&mut self, bytes: &[u8]) {
        if self.index.blocks.is_empty() {
            self.index = NlIndex::new();
        }
        self.data.extend_from_slice(bytes);
        self.index.total += memchr::memchr_iter(b'\n', bytes).count() as u64;
        let len = self.data.len() as u64;
        let mut next = self.index.blocks.len() as u64 * BLOCK;
        while next <= len {
            let prev = *self.index.blocks.last().unwrap();
            let start = (next - BLOCK) as usize;
            let n = memchr::memchr_iter(b'\n', &self.data[start..next as usize]).count() as u64;
            self.index.blocks.push(prev + n);
            next += BLOCK;
        }
    }
}

// ---------------------------------------------------------------------------
// Piece table

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Src {
    Orig,
    Add,
}

#[derive(Clone, Copy, Debug)]
struct Piece {
    src: Src,
    start: u64,
    len: u64,
    nl: u64,
}

/// One recorded change of the piece list: `inserted` pieces at `at`
/// replaced `removed`.
pub struct Change {
    at: usize,
    removed: Vec<Piece>,
    inserted: usize,
}

pub struct Document {
    orig: Option<Arc<Original>>,
    add: AddBuffer,
    pieces: Vec<Piece>,
    /// Document offset where each piece starts.
    starts: Vec<u64>,
    /// Newlines in pieces `[0, i]` (inclusive).
    nl_end: Vec<u64>,
    len: u64,
    /// Bumped on every change; used for caches.
    pub version: u64,
    /// Changes not yet collected by `take_changes`.
    journal: Vec<Change>,
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    pub fn new() -> Self {
        Self {
            orig: None,
            add: AddBuffer::default(),
            pieces: Vec::new(),
            starts: Vec::new(),
            nl_end: Vec::new(),
            len: 0,
            version: 0,
            journal: Vec::new(),
        }
    }

    pub fn from_original(orig: Original) -> Self {
        let mut doc = Self::new();
        if orig.len > 0 {
            doc.pieces.push(Piece {
                src: Src::Orig,
                start: 0,
                len: orig.len,
                nl: orig.index.total,
            });
        }
        doc.orig = Some(Arc::new(orig));
        doc.rebuild();
        doc
    }

    pub fn original(&self) -> Option<&Original> {
        self.orig.as_deref()
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn line_count(&self) -> u64 {
        self.nl_end.last().copied().unwrap_or(0) + 1
    }

    fn rebuild(&mut self) {
        let n = self.pieces.len();
        for v in [&mut self.starts, &mut self.nl_end] {
            v.clear();
            if v.capacity() > 2 * n + 4096 {
                v.shrink_to(n + n / 8);
            }
            reserve_moderately(v, n);
        }
        let mut pos = 0;
        let mut nl = 0;
        for p in &self.pieces {
            self.starts.push(pos);
            pos += p.len;
            nl += p.nl;
            self.nl_end.push(nl);
        }
        self.len = pos;
        self.version += 1;
    }

    // ---- raw source access ------------------------------------------------

    fn src_read(&self, src: Src, offset: u64, buf: &mut [u8]) {
        match src {
            Src::Orig => self.orig.as_ref().expect("orig").read(offset, buf),
            Src::Add => {
                let o = offset as usize;
                buf.copy_from_slice(&self.add.data[o..o + buf.len()]);
            }
        }
    }

    fn src_index(&self, src: Src) -> &NlIndex {
        match src {
            Src::Orig => &self.orig.as_ref().expect("orig").index,
            Src::Add => &self.add.index,
        }
    }

    fn count_nl_direct(&self, src: Src, a: u64, b: u64) -> u64 {
        if src == Src::Add {
            return memchr::memchr_iter(b'\n', &self.add.data[a as usize..b as usize]).count() as u64;
        }
        let mut buf = vec![0u8; (b - a) as usize];
        self.src_read(src, a, &mut buf);
        memchr::memchr_iter(b'\n', &buf).count() as u64
    }

    /// Newlines in `[0, pos)` of a source buffer.
    fn src_rank(&self, src: Src, pos: u64) -> u64 {
        let block = pos / BLOCK;
        let base = self.src_index(src).blocks[block as usize];
        base + self.count_nl_direct(src, block * BLOCK, pos)
    }

    /// Newlines in `[a, b)` of a source buffer.
    fn src_count_nl(&self, src: Src, a: u64, b: u64) -> u64 {
        if b <= a {
            0
        } else if b - a <= BLOCK {
            self.count_nl_direct(src, a, b)
        } else {
            self.src_rank(src, b) - self.src_rank(src, a)
        }
    }

    /// Position right after the `k`-th newline (k >= 1) at or after `from`.
    fn src_select(&self, src: Src, from: u64, k: u64) -> u64 {
        let index = self.src_index(src);
        let target = self.src_rank(src, from) + k;
        // First block whose end count reaches the target.
        let i = index.blocks.partition_point(|&c| c < target).saturating_sub(1);
        let mut pos = (i as u64 * BLOCK).max(from);
        let mut count = if pos == from { target - k } else { index.blocks[i] };
        let mut buf = vec![0u8; BLOCK as usize];
        loop {
            let src_len = match src {
                Src::Orig => self.orig.as_ref().unwrap().len,
                Src::Add => self.add.data.len() as u64,
            };
            let n = (BLOCK - pos % BLOCK).min(src_len - pos) as usize;
            if n == 0 {
                return pos;
            }
            self.src_read(src, pos, &mut buf[..n]);
            for i in memchr::memchr_iter(b'\n', &buf[..n]) {
                count += 1;
                if count == target {
                    return pos + i as u64 + 1;
                }
            }
            pos += n as u64;
        }
    }

    // ---- queries ------------------------------------------------------------

    /// Index of the piece containing `pos` (pos < len).
    fn piece_at(&self, pos: u64) -> usize {
        self.starts.partition_point(|&s| s <= pos) - 1
    }

    /// Byte offset where line `line` (0-based) starts.
    pub fn line_start(&self, line: u64) -> u64 {
        if line == 0 {
            return 0;
        }
        if line >= self.line_count() {
            return self.len;
        }
        let i = self.nl_end.partition_point(|&c| c < line);
        let before = if i == 0 { 0 } else { self.nl_end[i - 1] };
        let p = self.pieces[i];
        let end = self.src_select(p.src, p.start, line - before);
        self.starts[i] + (end - p.start)
    }

    /// Line (0-based) that contains byte offset `pos`.
    pub fn line_of(&self, pos: u64) -> u64 {
        if pos >= self.len {
            return self.line_count() - 1;
        }
        let i = self.piece_at(pos);
        let before = if i == 0 { 0 } else { self.nl_end[i - 1] };
        let p = self.pieces[i];
        before + self.src_count_nl(p.src, p.start, p.start + (pos - self.starts[i]))
    }

    /// Start of the line and end of its text (excluding `\n` / `\r\n`).
    pub fn line_range(&self, line: u64) -> (u64, u64) {
        let start = self.line_start(line);
        let mut end = if line + 1 >= self.line_count() {
            self.len
        } else {
            self.line_start(line + 1) - 1
        };
        if end > start && self.byte_at(end - 1) == Some(b'\r') && end < self.len {
            end -= 1;
        }
        (start, end)
    }

    pub fn byte_at(&self, pos: u64) -> Option<u8> {
        if pos >= self.len {
            return None;
        }
        let mut b = [0u8];
        self.read_into(pos, &mut b);
        Some(b[0])
    }

    /// Reads `buf.len()` bytes starting at `pos` (must be in range).
    pub fn read_into(&self, mut pos: u64, buf: &mut [u8]) {
        let mut out = 0usize;
        if buf.is_empty() {
            return;
        }
        let mut i = self.piece_at(pos);
        while out < buf.len() {
            let p = self.pieces[i];
            let off = pos - self.starts[i];
            let n = ((p.len - off) as usize).min(buf.len() - out);
            self.src_read(p.src, p.start + off, &mut buf[out..out + n]);
            out += n;
            pos += n as u64;
            i += 1;
        }
    }

    pub fn read(&self, a: u64, b: u64) -> Vec<u8> {
        let a = a.min(self.len);
        let b = b.clamp(a, self.len);
        let mut v = vec![0u8; (b - a) as usize];
        self.read_into(a, &mut v);
        v
    }

    /// Calls `f` with consecutive chunks of `[a, b)`.
    pub fn for_each_chunk(
        &self,
        a: u64,
        b: u64,
        chunk: usize,
        mut f: impl FnMut(u64, &[u8]) -> bool,
    ) {
        let mut buf = vec![0u8; chunk];
        let mut pos = a;
        while pos < b {
            let n = ((b - pos) as usize).min(chunk);
            self.read_into(pos, &mut buf[..n]);
            if !f(pos, &buf[..n]) {
                return;
            }
            pos += n as u64;
        }
    }

    // ---- edits --------------------------------------------------------------
    //
    // Every change of the piece list goes through `splice`, which records what
    // it replaced. Undo/redo stores these records instead of copies of the
    // whole piece list, so its memory cost is proportional to the edit.

    fn splice_raw(&mut self, range: Range<usize>, new: Vec<Piece>) -> Change {
        let inserted = new.len();
        let removed = if range == (0..self.pieces.len()) {
            // Whole list replaced (replace all, or undoing it): swap, don't copy.
            std::mem::replace(&mut self.pieces, new)
        } else {
            let needed = self.pieces.len() + inserted.saturating_sub(range.len());
            reserve_moderately(&mut self.pieces, needed);
            self.pieces.splice(range.clone(), new).collect()
        };
        shrink_if_oversized(&mut self.pieces);
        self.rebuild();
        Change {
            at: range.start,
            removed,
            inserted,
        }
    }

    fn splice(&mut self, range: Range<usize>, new: Vec<Piece>) {
        let change = self.splice_raw(range, new);
        self.journal.push(change);
    }

    /// Changes made since the last call (to be stored as one undo step).
    pub fn take_changes(&mut self) -> Vec<Change> {
        std::mem::take(&mut self.journal)
    }

    /// Undoes `changes` (newest last). Returns the changes that undo this
    /// revert, i.e. calling `revert` with the result re-applies the edit.
    pub fn revert(&mut self, changes: Vec<Change>) -> Vec<Change> {
        let mut inverse = Vec::with_capacity(changes.len());
        for c in changes.into_iter().rev() {
            inverse.push(self.splice_raw(c.at..c.at + c.inserted, c.removed));
        }
        inverse
    }

    /// Splits pieces so that a piece boundary exists at `pos`; returns the
    /// index of the first piece starting at `pos`.
    fn split_at(&mut self, pos: u64) -> usize {
        if pos >= self.len {
            return self.pieces.len();
        }
        let i = self.piece_at(pos);
        let off = pos - self.starts[i];
        if off == 0 {
            return i;
        }
        let p = self.pieces[i];
        let left_nl = self.src_count_nl(p.src, p.start, p.start + off);
        let left = Piece { len: off, nl: left_nl, ..p };
        let right = Piece {
            start: p.start + off,
            len: p.len - off,
            nl: p.nl - left_nl,
            ..p
        };
        self.splice(i..i + 1, vec![left, right]);
        i + 1
    }

    pub fn insert(&mut self, pos: u64, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let pos = pos.min(self.len);
        let add_start = self.add.data.len() as u64;
        self.add.append(bytes);
        let nl = memchr::memchr_iter(b'\n', bytes).count() as u64;
        let len = bytes.len() as u64;

        // Fast path: typing right after the previously inserted text.
        if pos > 0 {
            let i = self.piece_at(pos - 1);
            let p = self.pieces[i];
            if self.starts[i] + p.len == pos && p.src == Src::Add && p.start + p.len == add_start {
                let grown = Piece {
                    len: p.len + len,
                    nl: p.nl + nl,
                    ..p
                };
                self.splice(i..i + 1, vec![grown]);
                return;
            }
        }
        let i = self.split_at(pos);
        self.splice(
            i..i,
            vec![Piece {
                src: Src::Add,
                start: add_start,
                len,
                nl,
            }],
        );
    }

    pub fn delete(&mut self, a: u64, b: u64) {
        let a = a.min(self.len);
        let b = b.clamp(a, self.len);
        if a == b {
            return;
        }
        let i = self.split_at(a);
        let j = self.split_at(b);
        self.splice(i..j, Vec::new());
    }

    // ---- replace all ---------------------------------------------------------

    /// Replaces every occurrence of `needle` with `replacement` in one streaming
    /// pass, without loading the document into memory: unchanged text stays
    /// as references into the existing buffers, and all replacements share one
    /// copy of `replacement`. With `fold`, `needle` must already be passed
    /// through [`fold_case`]. Returns the number of replacements.
    pub fn replace_all(&mut self, needle: &[u8], replacement: &[u8], fold: bool) -> u64 {
        // Small chunks in tests so that matches across chunk borders are exercised.
        const CHUNK: u64 = if cfg!(test) { 1000 } else { 4 << 20 };

        struct Builder {
            /// Finished batches; collected in parts so that the final list can be
            /// allocated once with the exact size (no doubling reallocations).
            parts: Vec<Vec<Piece>>,
            out: Vec<Piece>,
            /// Old piece that the last output piece was cut from (None after a replacement).
            last_old: Option<usize>,
            piece: usize,
        }
        impl Builder {
            /// Keeps the document range `[a, b)`; `buf` holds the bytes from `buf_pos`.
            fn keep(&mut self, doc: &Document, buf: &[u8], buf_pos: u64, mut a: u64, b: u64) {
                while a < b {
                    while doc.starts[self.piece] + doc.pieces[self.piece].len <= a {
                        self.piece += 1;
                    }
                    let (pi, p) = (self.piece, doc.pieces[self.piece]);
                    let e = b.min(doc.starts[pi] + p.len);
                    let range = (a - buf_pos) as usize..(e - buf_pos) as usize;
                    let nl = memchr::memchr_iter(b'\n', &buf[range]).count() as u64;
                    match self.out.last_mut() {
                        Some(last) if self.last_old == Some(pi) => {
                            last.len += e - a;
                            last.nl += nl;
                        }
                        _ => self.out.push(Piece {
                            src: p.src,
                            start: p.start + (a - doc.starts[pi]),
                            len: e - a,
                            nl,
                        }),
                    }
                    self.last_old = Some(pi);
                    a = e;
                }
            }
        }

        let n = needle.len() as u64;
        if n == 0 || self.len < n {
            return 0;
        }
        let rep = Piece {
            src: Src::Add,
            start: self.add.data.len() as u64,
            len: replacement.len() as u64,
            nl: memchr::memchr_iter(b'\n', replacement).count() as u64,
        };
        let finder = memchr::memmem::Finder::new(needle);
        let mut bld = Builder {
            parts: Vec::new(),
            out: Vec::new(),
            last_old: None,
            piece: 0,
        };
        let mut count = 0u64;
        let mut cursor = 0u64; // everything before this is already in `bld.out`
        loop {
            let pos = cursor;
            let end = (pos + CHUNK).min(self.len);
            let mut buf = self.read(pos, end);
            if fold {
                fold_case(&mut buf);
            }
            for i in finder.find_iter(&buf) {
                let m = pos + i as u64;
                if m < cursor {
                    continue;
                }
                bld.keep(self, &buf, pos, cursor, m);
                if rep.len > 0 {
                    bld.out.push(rep);
                }
                bld.last_old = None;
                count += 1;
                cursor = m + n;
            }
            if end == self.len {
                bld.keep(self, &buf, pos, cursor, end);
                break;
            }
            if bld.out.len() >= 1 << 16 {
                let batch = std::mem::take(&mut bld.out);
                bld.parts.push(batch);
            }
            // A match may start in the last n-1 bytes; rescan them with the next chunk.
            let next = end - (n - 1);
            if next > cursor {
                bld.keep(self, &buf, pos, cursor, next);
                cursor = next;
            }
        }
        if count == 0 {
            return 0;
        }
        self.add.append(replacement);
        let total = bld.parts.iter().map(Vec::len).sum::<usize>() + bld.out.len();
        let mut pieces = Vec::with_capacity(total + total / 8 + 16);
        for part in bld.parts {
            pieces.extend_from_slice(&part);
        }
        pieces.extend_from_slice(&bld.out);
        self.splice(0..self.pieces.len(), pieces);
        count
    }

    // ---- saving -------------------------------------------------------------

    /// Writes the document to `path` safely: first into a temporary file next
    /// to it, then atomically renames it over the target. The original file
    /// stays readable through our open handle, so the document keeps working.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
        let result = (|| {
            let file = File::create(&tmp)?;
            let mut w = BufWriter::with_capacity(4 << 20, file);
            let mut err = None;
            self.for_each_chunk(0, self.len, 4 << 20, |_, chunk| match w.write_all(chunk) {
                Ok(()) => true,
                Err(e) => {
                    err = Some(e);
                    false
                }
            });
            if let Some(e) = err {
                return Err(e);
            }
            let file = w.into_inner().map_err(|e| e.into_error())?;
            file.sync_all()?;
            drop(file);
            if let Ok(meta) = std::fs::metadata(path) {
                let _ = std::fs::set_permissions(&tmp, meta.permissions());
                #[cfg(target_os = "macos")]
                copy_xattrs(path, &tmp);
            }
            std::fs::rename(&tmp, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }
}

/// Keeps Finder tags, comments, ACLs and other extended attributes of the
/// file being replaced (the save writes a new file and renames it over).
#[cfg(target_os = "macos")]
fn copy_xattrs(from: &Path, to: &Path) {
    use std::ffi::{CString, c_char, c_int, c_void};
    use std::os::unix::ffi::OsStrExt;
    unsafe extern "C" {
        fn copyfile(from: *const c_char, to: *const c_char, state: *mut c_void, flags: u32) -> c_int;
    }
    const COPYFILE_ACL: u32 = 1 << 0;
    const COPYFILE_XATTR: u32 = 1 << 2;
    let (Ok(f), Ok(t)) = (CString::new(from.as_os_str().as_bytes()), CString::new(to.as_os_str().as_bytes())) else {
        return;
    };
    unsafe {
        copyfile(f.as_ptr(), t.as_ptr(), std::ptr::null_mut(), COPYFILE_ACL | COPYFILE_XATTR);
    }
}

/// Makes room for `needed` elements, growing by ~1/8 instead of doubling:
/// after a "replace all" the piece list can have millions of entries.
fn reserve_moderately<T>(v: &mut Vec<T>, needed: usize) {
    if needed > v.capacity() {
        let target = needed + needed / 8 + 16;
        v.reserve_exact(target - v.len());
    }
}

fn shrink_if_oversized<T>(v: &mut Vec<T>) {
    if v.capacity() > 2 * v.len() + 4096 {
        v.shrink_to(v.len() + v.len() / 8);
    }
}

/// Case folding for case-insensitive search that keeps every character's
/// byte length, so positions in the folded text equal positions in the
/// original. Handles ASCII and any letter whose lowercase form has the same
/// UTF-8 length (Cyrillic, Greek, accented Latin, …).
pub fn fold_case(buf: &mut [u8]) {
    let mut i = 0;
    while i < buf.len() {
        let b = buf[i];
        if b < 0x80 {
            buf[i] = b.to_ascii_lowercase();
            i += 1;
            continue;
        }
        let len = match b {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => {
                i += 1;
                continue;
            }
        };
        if let Some(c) = buf.get(i..i + len).and_then(|s| std::str::from_utf8(s).ok()).and_then(|s| s.chars().next()) {
            let mut lower = c.to_lowercase();
            if let (Some(l), None) = (lower.next(), lower.next())
                && l != c
                && l.len_utf8() == len
            {
                l.encode_utf8(&mut buf[i..i + len]);
            }
            i += len;
        } else {
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(doc: &Document) -> String {
        String::from_utf8(doc.read(0, doc.len())).unwrap()
    }

    fn check_lines(doc: &Document) {
        let s = text(doc);
        let lines: Vec<&str> = s.split('\n').collect();
        assert_eq!(doc.line_count() as usize, lines.len());
        let mut pos = 0u64;
        for (i, l) in lines.iter().enumerate() {
            assert_eq!(doc.line_start(i as u64), pos, "line {i}");
            assert_eq!(doc.line_of(pos), i as u64);
            pos += l.len() as u64 + 1;
        }
    }

    fn temp_file(content: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("notepad-test-{}-{}", std::process::id(), content.len()));
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn edits_and_lines() {
        let mut doc = Document::new();
        doc.insert(0, b"hello\nworld");
        check_lines(&doc);
        doc.insert(5, b" there\n!");
        assert_eq!(text(&doc), "hello there\n!\nworld");
        check_lines(&doc);
        doc.delete(3, 9);
        assert_eq!(text(&doc), "helre\n!\nworld");
        check_lines(&doc);
        doc.take_changes();
        doc.delete(0, doc.len());
        doc.insert(0, b"new");
        assert_eq!(text(&doc), "new");
        let changes = doc.take_changes();
        let redo = doc.revert(changes);
        assert_eq!(text(&doc), "helre\n!\nworld");
        check_lines(&doc);
        doc.revert(redo);
        assert_eq!(text(&doc), "new");
    }

    #[test]
    fn fold_case_keeps_lengths() {
        let mut b = "Привет МИР Hello ÄÖ İ".as_bytes().to_vec();
        let len = b.len();
        fold_case(&mut b);
        assert_eq!(b.len(), len);
        assert_eq!(String::from_utf8(b).unwrap(), "привет мир hello äö İ");
    }

    #[test]
    fn replace_all_streams() {
        let mut doc = Document::new();
        doc.insert(0, b"foo bar\nFoo baz\nxfoo");
        doc.insert(4, b"FOO "); // several pieces
        let n = doc.replace_all(b"foo", b"qu\nx", true);
        assert_eq!(n, 4);
        assert_eq!(text(&doc), "qu\nx qu\nx bar\nqu\nx baz\nxqu\nx");
        check_lines(&doc);
        assert_eq!(doc.replace_all(b"qu\nx", b"", false), 4);
        assert_eq!(text(&doc), "  bar\n baz\nx");
        check_lines(&doc);
        assert_eq!(doc.replace_all(b"zzz", b"y", false), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn save_keeps_extended_attributes() {
        let path = temp_file(b"tagged file contents");
        let run = |args: &[&str]| std::process::Command::new("xattr").args(args).output().unwrap();
        run(&["-w", "com.example.notepad-test", "kept", path.to_str().unwrap()]);
        let o = Original::open(&path, &AtomicU64::new(0), &AtomicBool::new(false)).unwrap();
        let mut doc = Document::from_original(o);
        doc.insert(0, b"edited ");
        doc.save(&path).unwrap();
        let out = run(&["-p", "com.example.notepad-test", path.to_str().unwrap()]);
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "kept");
        assert_eq!(std::fs::read(&path).unwrap(), b"edited tagged file contents");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn big_original() {
        let mut content = Vec::new();
        for i in 0..200_000 {
            content.extend_from_slice(format!("line number {i}\n").as_bytes());
        }
        let path = temp_file(&content);
        let orig = Original::open(&path, &AtomicU64::new(0), &AtomicBool::new(false)).unwrap();
        let mut doc = Document::from_original(orig);
        assert_eq!(doc.line_count(), 200_001);
        assert_eq!(doc.line_range(12345), {
            let s = doc.line_start(12345);
            (s, s + "line number 12345".len() as u64)
        });
        assert_eq!(String::from_utf8(doc.read(doc.line_start(150_000), doc.line_start(150_001))).unwrap(), "line number 150000\n");
        doc.insert(100_000, b"X\nY");
        doc.delete(2_000_000, 2_000_100);
        check_lines(&doc);
        let expected = text(&doc).replace("number 12", "N°");
        let n = doc.replace_all(b"number 12", "N°".as_bytes(), false);
        assert_eq!(n as usize, text(&doc).matches("N°").count());
        assert_eq!(text(&doc), expected);
        check_lines(&doc);
        doc.save(&path).unwrap();
        let saved = std::fs::read(&path).unwrap();
        assert_eq!(saved, doc.read(0, doc.len()));
        std::fs::remove_file(&path).unwrap();
    }
}
