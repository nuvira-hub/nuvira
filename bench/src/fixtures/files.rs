//! Files to transfer (plan 5.4): 1 KiB, 1 MiB, 100 MiB, 1 GiB of xoshiro256**
//! output with a fixed seed per size (incompressible), generated into the run's temp
//! directory, never committed. A sparse 1 MiB variant with long zero runs checks that
//! nothing special-cases such data (informational).

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::rng::Xoshiro256;

pub const KIB: u64 = 1 << 10;
pub const MIB: u64 = 1 << 20;
pub const GIB: u64 = 1 << 30;

/// Sizes of the random files.
pub const SIZES: [u64; 4] = [KIB, MIB, 100 * MIB, GIB];
/// Size of the sparse file.
pub const SPARSE_SIZE: u64 = MIB;

const FILE_SEED: u64 = 0x4649_4c45_5345_4544; // "FILESEED"
const CHUNK: usize = 1 << 20;

/// Feeds the content of a file of `size` bytes to `sink`, 1 MiB at a time.
pub fn content(size: u64, sparse: bool, mut sink: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
    let mut rng = Xoshiro256::new(FILE_SEED ^ size ^ if sparse { 0x5 << 60 } else { 0 });
    let mut buf = vec![0u8; CHUNK];
    let mut left = size;
    let mut offset = 0u64;
    while left > 0 {
        let n = left.min(CHUNK as u64) as usize;
        rng.fill_bytes(&mut buf[..n]);
        if sparse {
            // 64 KiB of data, then 192 KiB of zeros, repeated.
            for (i, b) in buf[..n].iter_mut().enumerate() {
                if (offset + i as u64) % (256 * KIB) >= 64 * KIB {
                    *b = 0;
                }
            }
        }
        sink(&buf[..n])?;
        left -= n as u64;
        offset += n as u64;
    }
    Ok(())
}

/// File name used for a size.
pub fn file_name(size: u64, sparse: bool) -> String {
    format!("bench-{}{}.bin", size, if sparse { "-sparse" } else { "" })
}

/// SHA-256 of the content, without writing it.
pub fn digest(size: u64, sparse: bool) -> [u8; 32] {
    let mut h = Sha256::new();
    content(size, sparse, |b| {
        h.update(b);
        Ok(())
    })
    .expect("hashing cannot fail");
    h.finalize().into()
}

/// Writes the random file of `size` bytes into `dir`: path and SHA-256.
pub fn make(size: u64, dir: &Path) -> Result<(PathBuf, [u8; 32])> {
    write(size, false, dir)
}

/// Writes the sparse variant into `dir`: path and SHA-256.
pub fn make_sparse(size: u64, dir: &Path) -> Result<(PathBuf, [u8; 32])> {
    write(size, true, dir)
}

fn write(size: u64, sparse: bool, dir: &Path) -> Result<(PathBuf, [u8; 32])> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(file_name(size, sparse));
    let mut f = std::io::BufWriter::with_capacity(CHUNK, std::fs::File::create(&path)?);
    let mut h = Sha256::new();
    content(size, sparse, |b| {
        h.update(b);
        f.write_all(b)?;
        Ok(())
    })?;
    f.flush()?;
    Ok((path, h.finalize().into()))
}

/// SHA-256 of a file on disk (received files are checked against the manifest).
pub fn sha256_file(path: &Path) -> Result<[u8; 32]> {
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(h.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_matches_digest_and_is_deterministic() {
        let dir = std::env::temp_dir().join(format!("nuvira-bench-files-{}", std::process::id()));
        let (p, h) = make(MIB + 123, &dir).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().len(), MIB + 123);
        assert_eq!(h, digest(MIB + 123, false));
        assert_eq!(sha256_file(&p).unwrap(), h);
        let (ps, hs) = make_sparse(SPARSE_SIZE, &dir).unwrap();
        let data = std::fs::read(&ps).unwrap();
        assert_eq!(data[64 * KIB as usize..256 * KIB as usize].iter().filter(|&&b| b != 0).count(), 0);
        assert_ne!(hs, digest(SPARSE_SIZE, false));
        std::fs::remove_dir_all(&dir).ok();
    }
}
