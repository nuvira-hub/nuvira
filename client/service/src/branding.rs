//! Configuration of customised installers (NuviraHub Pro): a JSON document
//! appended to the program, after which come its length (u32, little endian) and
//! [`MAGIC`]. Windows ignores bytes past the end of the executable image.
//!
//! The app reads it at start-up and keeps a copy, because an automatic update
//! replaces the program with the standard one; the installation keeps one next to
//! the program ([`installed_path`]), readable by every user.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const MAGIC: &[u8] = b"NUVIRA-BRAND1";

/// Larger documents are ignored (the logo is the big part).
const MAX_LEN: u32 = 1 << 20;

/// The document appended to the program at path, if any.
pub fn read_trailer(path: &Path) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let tail = (MAGIC.len() + 4) as u64;
    if size < tail {
        return None;
    }
    file.seek(SeekFrom::Start(size - tail)).ok()?;
    let mut end = vec![0u8; tail as usize];
    file.read_exact(&mut end).ok()?;
    if &end[4..] != MAGIC {
        return None;
    }
    let len = u32::from_le_bytes(end[..4].try_into().ok()?);
    if len == 0 || len > MAX_LEN || u64::from(len) > size - tail {
        return None;
    }
    file.seek(SeekFrom::Start(size - tail - u64::from(len))).ok()?;
    let mut doc = vec![0u8; len as usize];
    file.read_exact(&mut doc).ok()?;
    Some(doc)
}

/// The document as it is appended to a program.
pub fn trailer(doc: &[u8]) -> Vec<u8> {
    let mut out = doc.to_vec();
    out.extend_from_slice(&(doc.len() as u32).to_le_bytes());
    out.extend_from_slice(MAGIC);
    out
}

/// Copy of the installation, next to the installed program.
pub fn installed_path() -> PathBuf {
    std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| r"C:\Program Files".into())
        .join("nuvira")
        .join("branding.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailer_round_trip() {
        let dir = std::env::temp_dir().join(format!("nuvira-branding-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("program.exe");
        let mut bytes = b"MZ fake program".to_vec();
        std::fs::write(&program, &bytes).unwrap();
        assert!(read_trailer(&program).is_none());
        bytes.extend(trailer(br#"{"name":"Rossi"}"#));
        std::fs::write(&program, &bytes).unwrap();
        assert_eq!(read_trailer(&program).unwrap(), br#"{"name":"Rossi"}"#);
        // A wrong length is ignored.
        let mut broken = b"MZ".to_vec();
        broken.extend(&u32::MAX.to_le_bytes());
        broken.extend(MAGIC);
        std::fs::write(&program, &broken).unwrap();
        assert!(read_trailer(&program).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
