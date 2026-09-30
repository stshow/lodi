//! The NAR-compatible tree hash (design `spec/03` §4, ADR-016).
//!
//! [`dump`] writes the Nix Archive serialization of a path exactly as `nix-store --dump` does,
//! and [`hash`] is the SHA-256 of those bytes, so `nix-hash --type sha256 <path>` prints the same
//! hex. Every string is a 64-bit little-endian length, the bytes, and zero padding to a multiple
//! of 8. Directory entries are sorted bytewise by name; of the mode bits only the owner-execute
//! bit is kept; mtimes, owners and xattrs are ignored; hard links are separate regular files.
//! Sockets, FIFOs and device nodes are refused (`E_TREE_UNSUPPORTED`).

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Why a path could not be serialized.
#[derive(Debug)]
pub enum NarError {
    /// A socket, FIFO or device node (`E_TREE_UNSUPPORTED`).
    Unsupported(PathBuf),
    /// A file changed size while it was read, or another I/O failure.
    Io(PathBuf, io::Error),
}

impl std::fmt::Display for NarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NarError::Unsupported(p) => {
                write!(f, "{} is a socket, FIFO or device node", p.display())
            }
            NarError::Io(p, e) => write!(f, "{}: {e}", p.display()),
        }
    }
}

fn pad(w: &mut dyn Write, len: u64) -> io::Result<()> {
    let rem = (len % 8) as usize;
    if rem != 0 {
        w.write_all(&[0u8; 8][..8 - rem])?;
    }
    Ok(())
}

fn string(w: &mut dyn Write, bytes: &[u8]) -> io::Result<()> {
    w.write_all(&(bytes.len() as u64).to_le_bytes())?;
    w.write_all(bytes)?;
    pad(w, bytes.len() as u64)
}

/// Write the NAR serialization of `path` (a file, symlink or directory) to `w`.
pub fn dump(path: &Path, w: &mut dyn Write) -> Result<(), NarError> {
    string(w, b"nix-archive-1").map_err(|e| NarError::Io(path.to_path_buf(), e))?;
    node(path, w)
}

fn node(path: &Path, w: &mut dyn Write) -> Result<(), NarError> {
    let io_err = |e| NarError::Io(path.to_path_buf(), e);
    let meta = fs::symlink_metadata(path).map_err(io_err)?;
    let kind = meta.file_type();
    string(w, b"(").map_err(io_err)?;
    string(w, b"type").map_err(io_err)?;
    if kind.is_symlink() {
        let target = fs::read_link(path).map_err(io_err)?;
        for s in [&b"symlink"[..], b"target", target.as_os_str().as_bytes()] {
            string(w, s).map_err(io_err)?;
        }
    } else if kind.is_file() {
        string(w, b"regular").map_err(io_err)?;
        if meta.permissions().mode() & 0o100 != 0 {
            string(w, b"executable").map_err(io_err)?;
            string(w, b"").map_err(io_err)?;
        }
        string(w, b"contents").map_err(io_err)?;
        let len = meta.len();
        w.write_all(&len.to_le_bytes()).map_err(io_err)?;
        let file = fs::File::open(path).map_err(io_err)?;
        let copied = io::copy(&mut file.take(len), w).map_err(io_err)?;
        if copied != len {
            return Err(io_err(io::Error::other(
                "the file changed size while hashing",
            )));
        }
        pad(w, len).map_err(io_err)?;
    } else if kind.is_dir() {
        string(w, b"directory").map_err(io_err)?;
        let mut names: Vec<OsString> = fs::read_dir(path)
            .map_err(io_err)?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<Result<_, _>>()
            .map_err(io_err)?;
        names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        for name in names {
            for s in [&b"entry"[..], b"(", b"name", name.as_bytes(), b"node"] {
                string(w, s).map_err(io_err)?;
            }
            node(&path.join(&name), w)?;
            string(w, b")").map_err(io_err)?;
        }
    } else {
        return Err(NarError::Unsupported(path.to_path_buf()));
    }
    string(w, b")").map_err(io_err)
}

struct HashWriter(Sha256);

impl Write for HashWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// `sha256:<hex>` of the NAR serialization of `path`.
pub fn hash(path: &Path) -> Result<String, NarError> {
    let mut w = io::BufWriter::with_capacity(1 << 16, HashWriter(Sha256::new()));
    dump(path, &mut w)?;
    let inner = w
        .into_inner()
        .map_err(|e| NarError::Io(path.to_path_buf(), e.into_error()))?;
    let digest = inner.0.finalize();
    Ok(format!(
        "sha256:{}",
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_length_prefixed_and_padded() {
        let mut out = Vec::new();
        string(&mut out, b"abc").unwrap();
        assert_eq!(
            out,
            [3, 0, 0, 0, 0, 0, 0, 0, b'a', b'b', b'c', 0, 0, 0, 0, 0]
        );
        let mut out = Vec::new();
        string(&mut out, b"").unwrap();
        assert_eq!(out, [0u8; 8]);
    }
}
