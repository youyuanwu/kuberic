//! Candidate PG16.15 pg_rewind frontend validator, NOT a native authorization owner.
//!
//! The caller must own the source PGDATA (no untrusted OS writer), authenticate and
//! independently revalidate the exact source/target/session/candidate lease before
//! forwarding EVERY accepted frame, and close the connection on any error. Validate
//! immediately before forwarding, not while buffering a batch for later forwarding.
//! Backend errors must also close the connection: this frontend-only state machine
//! does not infer successful preparation/execution from backend replies.
//!
//! Filesystem checks do not close check/use races against an adversarial OS writer.
//! An owned live PostgreSQL writer may remove/truncate files; only the chunk query
//! has missing_ok semantics. No PG service, repair, lineage or readiness is proved.

use sha2::{Digest, Sha256};
use std::fs::{self, Metadata, ReadDir};
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

/// Entire frontend frame, including kind and four-byte length. Enforce before allocation.
pub(super) const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 1023;
const MAX_DEPTH: usize = 64;
const MAX_TREE_ENTRIES: usize = 1_000_000;
const MAX_CHUNKS: usize = 1000;
const MAX_CHUNK_BYTES: i64 = 1024 * 1024;
const CHUNKS_NAME: &[u8] = b"fetch_chunks_stmt";
const FILE_SQL: &[u8] = b"SELECT pg_read_binary_file($1)";
const SECURE_PATH: &[u8] = b"SELECT pg_catalog.set_config('search_path', '', false);";
// Exact concatenated C string literals from REL_16_15/libpq_source.c.
const CHUNKS_SQL_SHA256: &str = "954ef65149532642e876ebfcec0ba1bf0c8df413f3c4846587992b96b82aec05";
const LIST_SQL_SHA256: &str = "aa8b4f3b9bd2659635b064b385587958a435394dadac192229ffb9fc52bc5c82";

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "unsupported rewind wire/path",
    )
}

fn require(ok: bool) -> io::Result<()> {
    if ok { Ok(()) } else { Err(denied()) }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Statement {
    File,
    Chunks,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Parsed,
    Bound,
    Described,
    Executed,
    Maintenance,
}

struct Portal {
    statement: Statement,
    paths: Vec<String>,
}

pub(super) struct RewindProtocol {
    root: PathBuf,
    device: u64,
    inode: u64,
    chunks: bool,
    unnamed: bool,
    portal: Option<Portal>,
    phase: Phase,
    closed: bool,
}

impl RewindProtocol {
    pub(super) fn new(owned_pgdata: PathBuf) -> io::Result<Self> {
        require(owned_pgdata.is_absolute())?;
        let text = owned_pgdata.to_str().ok_or_else(denied)?;
        require(text != "/" && !text.ends_with('/'))?;
        require(
            text[1..]
                .split('/')
                .all(|c| !c.is_empty() && c != "." && c != ".."),
        )?;
        no_symlink_ancestors(&owned_pgdata)?;
        require(fs::canonicalize(&owned_pgdata)? == owned_pgdata)?;
        let metadata = fs::symlink_metadata(&owned_pgdata)?;
        require(metadata.is_dir())?;
        let result = Self {
            root: owned_pgdata,
            device: metadata.dev(),
            inode: metadata.ino(),
            chunks: false,
            unnamed: false,
            portal: None,
            phase: Phase::Idle,
            closed: false,
        };
        result.check_root()?;
        Ok(result)
    }

    /// `body` excludes the kind and four-byte length; transport checks framing itself.
    /// Errors poison this instance, even if a rejected frame was never forwarded.
    pub(super) fn validate(&mut self, kind: u8, body: &[u8]) -> io::Result<()> {
        let result = self.validate_inner(kind, body);
        if result.is_err() {
            self.closed = true;
        }
        result
    }

    fn validate_inner(&mut self, kind: u8, body: &[u8]) -> io::Result<()> {
        require(!self.closed && body.len() <= MAX_FRAME_BYTES - 5)?;
        self.check_root()?;
        let mut input = Input(body);
        match kind {
            b'Q' => {
                require(self.phase == Phase::Idle)?;
                let query = input.cstr(8192)?;
                input.end()?;
                let simple = [
                    b"SET statement_timeout = 0".as_slice(),
                    b"SET lock_timeout = 0",
                    b"SET idle_in_transaction_session_timeout = 0",
                    b"SET default_transaction_read_only = on",
                    SECURE_PATH,
                    b"SHOW full_page_writes",
                    b"SELECT pg_current_wal_insert_lsn()",
                ];
                if !simple.contains(&query) {
                    require(has_digest(query, LIST_SQL_SHA256))?;
                    self.check_listing()?;
                }
                // PostgreSQL simple Query destroys the unnamed statement/portal.
                self.unnamed = false;
                self.portal = None;
            }
            b'P' => {
                require(self.phase == Phase::Idle)?;
                let name = input.cstr(CHUNKS_NAME.len())?;
                let query = input.cstr(8192)?;
                let statement = if name == CHUNKS_NAME {
                    require(!self.chunks && has_digest(query, CHUNKS_SQL_SHA256))?;
                    Statement::Chunks
                } else {
                    require(name.is_empty() && query == FILE_SQL)?;
                    Statement::File
                };
                let types = match statement {
                    Statement::Chunks => [1009u32, 1016, 1007].as_slice(),
                    Statement::File => [25u32].as_slice(),
                };
                let count = usize::from(input.u16()?);
                // PQprepare/PQexecParams with NULL paramTypes send zero: infer from exact SQL.
                require(count == 0 || count == types.len())?;
                for expected in types.iter().take(count) {
                    require(input.u32()? == *expected)?;
                }
                input.end()?;
                match statement {
                    Statement::Chunks => self.chunks = true,
                    Statement::File => {
                        self.unnamed = true;
                        self.portal = None;
                    }
                }
                self.phase = Phase::Parsed;
            }
            b'B' => {
                require(matches!(self.phase, Phase::Idle | Phase::Parsed))?;
                require(input.cstr(0)?.is_empty())?;
                let statement = self.statement(input.cstr(CHUNKS_NAME.len())?)?;
                let count = match statement {
                    Statement::Chunks => 3,
                    Statement::File => 1,
                };
                let formats = usize::from(input.u16()?);
                require(formats == 0 || formats == 1 || formats == count)?;
                for _ in 0..formats {
                    require(input.u16()? == 0)?;
                }
                require(usize::from(input.u16()?) == count)?;
                let mut parameters = Vec::with_capacity(count);
                for _ in 0..count {
                    let length = usize::try_from(input.i32()?).map_err(|_| denied())?;
                    require(length <= MAX_FRAME_BYTES - 5)?;
                    parameters.push(input.bytes(length)?);
                }
                // Actual helper asks libpq for a single all-columns binary format.
                require(input.u16()? == 1 && input.u16()? == 1)?;
                input.end()?;
                let paths = match statement {
                    Statement::File => {
                        let path = path_text(parameters[0])?;
                        require(single_file(path))?;
                        self.check_file(path, false)?;
                        vec![path.to_owned()]
                    }
                    Statement::Chunks => {
                        let paths = path_array(parameters[0])?;
                        let offsets = number_array(parameters[1])?;
                        let lengths = number_array(parameters[2])?;
                        require(paths.len() == offsets.len() && paths.len() == lengths.len())?;
                        for ((path, offset), length) in paths.iter().zip(offsets).zip(lengths) {
                            require(length > 0 && length <= MAX_CHUNK_BYTES)?;
                            require(offset.checked_add(length).is_some())?;
                            self.check_file(path, true)?;
                        }
                        paths
                    }
                };
                self.portal = Some(Portal { statement, paths });
                self.phase = Phase::Bound;
            }
            b'D' => {
                let target = input.byte()?;
                let name = input.cstr(CHUNKS_NAME.len())?;
                input.end()?;
                match target {
                    b'P' => {
                        require(name.is_empty() && self.phase == Phase::Bound)?;
                        require(self.portal.is_some())?;
                        self.phase = Phase::Described;
                    }
                    b'S' => {
                        require(matches!(self.phase, Phase::Idle | Phase::Parsed))?;
                        self.statement(name)?;
                        self.phase = Phase::Maintenance;
                    }
                    _ => return Err(denied()),
                }
            }
            b'E' => {
                require(matches!(self.phase, Phase::Bound | Phase::Described))?;
                require(input.cstr(0)?.is_empty() && input.u32()? == 0)?;
                input.end()?;
                let portal = self.portal.as_ref().ok_or_else(denied)?;
                // Recheck at execution, not just at Parse or Bind.
                for path in &portal.paths {
                    self.check_file(path, portal.statement == Statement::Chunks)?;
                }
                self.phase = Phase::Executed;
            }
            b'C' => {
                // PG16 pg_rewind does not emit Close; narrow known-object cleanup is safe.
                require(matches!(
                    self.phase,
                    Phase::Idle | Phase::Bound | Phase::Described
                ))?;
                let target = input.byte()?;
                let name = input.cstr(CHUNKS_NAME.len())?;
                input.end()?;
                match target {
                    b'S' => match self.statement(name)? {
                        Statement::Chunks => self.chunks = false,
                        Statement::File => self.unnamed = false,
                    },
                    b'P' => {
                        require(name.is_empty() && self.portal.is_some())?;
                        self.portal = None;
                    }
                    _ => return Err(denied()),
                }
                // Conservative: do not retain a portal across closing any statement.
                self.portal = None;
                self.phase = Phase::Maintenance;
            }
            b'H' => {
                input.end()?;
                require(self.phase != Phase::Idle)?;
            }
            b'S' => {
                input.end()?;
                require(matches!(
                    self.phase,
                    Phase::Parsed | Phase::Executed | Phase::Maintenance
                ))?;
                self.phase = Phase::Idle;
                self.portal = None;
            }
            b'X' => {
                input.end()?;
                self.closed = true;
            }
            _ => return Err(denied()),
        }
        Ok(())
    }

    fn statement(&self, name: &[u8]) -> io::Result<Statement> {
        if name == CHUNKS_NAME && self.chunks {
            Ok(Statement::Chunks)
        } else if name.is_empty() && self.unnamed {
            Ok(Statement::File)
        } else {
            Err(denied())
        }
    }

    fn check_root(&self) -> io::Result<()> {
        no_symlink_ancestors(&self.root)?;
        let metadata = fs::symlink_metadata(&self.root)?;
        require(metadata.is_dir() && same_identity(&metadata, self.device, self.inode))?;
        require(fs::canonicalize(&self.root)? == self.root)
    }

    fn check_file(&self, path: &str, missing_ok: bool) -> io::Result<()> {
        path_text(path.as_bytes())?;
        require(path.split('/').next() != Some("pg_tblspc"))?;
        self.check_root()?;
        let mut full = self.root.clone();
        let components: Vec<_> = path.split('/').collect();
        for (index, component) in components.iter().enumerate() {
            full.push(component);
            match fs::symlink_metadata(&full) {
                Ok(metadata) => {
                    require(!metadata.file_type().is_symlink())?;
                    if index + 1 == components.len() {
                        require(metadata.is_file())?;
                    } else {
                        require(metadata.is_dir())?;
                    }
                    match fs::canonicalize(&full) {
                        Ok(canonical) => require(canonical.starts_with(&self.root))?,
                        Err(error) if missing_ok && error.kind() == io::ErrorKind::NotFound => {
                            return self.check_root();
                        }
                        Err(error) => return Err(error),
                    }
                }
                Err(error) if missing_ok && error.kind() == io::ErrorKind::NotFound => {
                    // Remaining components have already passed strict lexical checks.
                    // The nearest existing parent was checked above (or is pinned root).
                    return self.check_root();
                }
                Err(error) => return Err(error),
            }
        }
        self.check_root()
    }

    fn check_listing(&self) -> io::Result<()> {
        self.check_root()?;
        let mut stack: Vec<ReadDir> = vec![fs::read_dir(&self.root)?];
        let mut count = 0usize;
        while let Some(directory) = stack.last_mut() {
            let Some(entry) = directory.next() else {
                stack.pop();
                continue;
            };
            let entry = entry?;
            count += 1;
            require(count <= MAX_TREE_ENTRIES)?;
            let full = entry.path();
            let relative = full.strip_prefix(&self.root).map_err(|_| denied())?;
            let relative = relative.to_str().ok_or_else(denied)?;
            path_text(relative.as_bytes())?;
            require(!relative.starts_with("pg_tblspc/"))?;
            let metadata = match fs::symlink_metadata(&full) {
                Ok(metadata) => metadata,
                // The listing SQL itself uses missing_ok for concurrently removed entries.
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let file_type = metadata.file_type();
            require(!file_type.is_symlink())?;
            if file_type.is_dir() {
                require(stack.len() < MAX_DEPTH)?;
                match fs::read_dir(&full) {
                    Ok(directory) => stack.push(directory),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                }
            } else {
                require(
                    file_type.is_file()
                        || (file_type.is_socket() && relative.starts_with("pg_stat_tmp/")),
                )?;
            }
        }
        self.check_root()
    }
}

fn no_symlink_ancestors(path: &Path) -> io::Result<()> {
    for ancestor in path.ancestors() {
        require(fs::symlink_metadata(ancestor)?.is_dir())?;
    }
    Ok(())
}

fn same_identity(metadata: &Metadata, device: u64, inode: u64) -> bool {
    metadata.dev() == device && metadata.ino() == inode
}

fn has_digest(query: &[u8], expected: &str) -> bool {
    let digest = Sha256::digest(query);
    digest.iter().enumerate().all(|(index, byte)| {
        let digits = b"0123456789abcdef";
        expected.as_bytes()[index * 2] == digits[usize::from(byte >> 4)]
            && expected.as_bytes()[index * 2 + 1] == digits[usize::from(byte & 15)]
    })
}

fn path_text(bytes: &[u8]) -> io::Result<&str> {
    require(!bytes.is_empty() && bytes.len() <= MAX_PATH_BYTES)?;
    let text = std::str::from_utf8(bytes).map_err(|_| denied())?;
    require(text.split('/').count() <= MAX_DEPTH)?;
    for component in text.split('/') {
        require(!component.is_empty() && component != "." && component != "..")?;
        require(
            component
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')),
        )?;
    }
    Ok(text)
}

fn single_file(path: &str) -> bool {
    if path == "global/pg_control" {
        return true;
    }
    let Some(history) = path
        .strip_prefix("pg_wal/")
        .and_then(|p| p.strip_suffix(".history"))
    else {
        return false;
    };
    history.len() == 8
        && history
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
        && u32::from_str_radix(history, 16).is_ok_and(|timeline| timeline > 1)
}

fn path_array(bytes: &[u8]) -> io::Result<Vec<String>> {
    let mut input = Input(bytes);
    require(input.byte()? == b'{')?;
    let mut paths = Vec::new();
    loop {
        require(paths.len() < MAX_CHUNKS && input.byte()? == b'"')?;
        let end = input.0.iter().position(|b| *b == b'"').ok_or_else(denied)?;
        // The helper quotes all paths. Escapes are unnecessary in the supported layout.
        let path = path_text(input.bytes(end)?)?;
        paths.push(path.to_owned());
        require(input.byte()? == b'"')?;
        match input.byte()? {
            b',' => continue,
            b'}' => break,
            _ => return Err(denied()),
        }
    }
    input.end()?;
    Ok(paths)
}

fn number_array(bytes: &[u8]) -> io::Result<Vec<i64>> {
    require(bytes.first() == Some(&b'{') && bytes.last() == Some(&b'}'))?;
    let mut numbers = Vec::new();
    for part in bytes[1..bytes.len() - 1].split(|b| *b == b',') {
        require(numbers.len() < MAX_CHUNKS && !part.is_empty() && part.len() <= 19)?;
        require(part.iter().all(u8::is_ascii_digit))?;
        require(part.len() == 1 || part[0] != b'0')?;
        let number = std::str::from_utf8(part)
            .map_err(|_| denied())?
            .parse::<i64>()
            .map_err(|_| denied())?;
        numbers.push(number);
    }
    Ok(numbers)
}

struct Input<'a>(&'a [u8]);

impl<'a> Input<'a> {
    fn bytes(&mut self, length: usize) -> io::Result<&'a [u8]> {
        require(length <= self.0.len())?;
        let (head, tail) = self.0.split_at(length);
        self.0 = tail;
        Ok(head)
    }

    fn byte(&mut self) -> io::Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_be_bytes(
            self.bytes(2)?.try_into().map_err(|_| denied())?,
        ))
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_be_bytes(
            self.bytes(4)?.try_into().map_err(|_| denied())?,
        ))
    }

    fn i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_be_bytes(
            self.bytes(4)?.try_into().map_err(|_| denied())?,
        ))
    }

    fn cstr(&mut self, maximum: usize) -> io::Result<&'a [u8]> {
        let length = self
            .0
            .iter()
            .take(maximum + 1)
            .position(|b| *b == 0)
            .ok_or_else(denied)?;
        let value = self.bytes(length)?;
        self.byte()?;
        Ok(value)
    }

    fn end(&self) -> io::Result<()> {
        require(self.0.is_empty())
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
