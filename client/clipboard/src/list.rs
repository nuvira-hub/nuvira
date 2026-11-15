//! Files and folders copied on this side, listed for the other one: every file
//! and folder under them, with paths relative to the folder that holds them.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// At most this many files and folders in one copy (the rest is refused: the file
/// manager is the way for whole disks).
pub const MAX_ENTRIES: usize = 10_000;

/// One file or folder of a copy.
#[derive(Debug, Clone, PartialEq)]
pub struct Listed {
    /// Where it is on this computer.
    pub source: PathBuf,
    /// Relative path with "/" between parts ("photos/a.jpg").
    pub path: String,
    pub size: u64,
    pub dir: bool,
    /// Seconds since 1970, 0 when unknown.
    pub modified: i64,
}

/// Lists `paths` (what was copied) and, for folders, everything under them,
/// folders before their contents. Links are left out (they could lead anywhere,
/// also around in a circle).
pub fn expand(paths: &[PathBuf]) -> Result<Vec<Listed>> {
    let mut out = Vec::new();
    for path in paths {
        let name = match path.file_name() {
            Some(name) => name.to_string_lossy().into_owned(),
            None => bail!("{} cannot be copied", path.display()),
        };
        walk(path, name, &mut out)?;
    }
    if out.is_empty() {
        bail!("nothing to copy");
    }
    Ok(out)
}

fn walk(path: &Path, rel: String, out: &mut Vec<Listed>) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if out.len() >= MAX_ENTRIES {
        bail!("more than {MAX_ENTRIES} files and folders");
    }
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    let dir = meta.is_dir();
    out.push(Listed { source: path.to_path_buf(), path: rel.clone(), size: if dir { 0 } else { meta.len() }, dir, modified });
    if dir {
        let mut children: Vec<_> = std::fs::read_dir(path)?.filter_map(|e| e.ok()).collect();
        children.sort_by_key(|e| e.file_name());
        for child in children {
            let name = child.file_name().to_string_lossy().into_owned();
            walk(&child.path(), format!("{rel}/{name}"), out)?;
        }
    }
    Ok(())
}

/// A relative path received from the other side, made safe: its parts only
/// (no "..", no roots, no drive letters), each without characters the systems
/// refuse. `None` when nothing is left.
pub fn safe_relative(path: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in path.split(['/', '\\']) {
        if part.is_empty() || part == "." || part == ".." {
            continue;
        }
        let clean: String = part
            .chars()
            .map(|c| if c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
            .collect();
        let clean = clean.trim_end_matches([' ', '.']);
        if clean.is_empty() {
            continue;
        }
        out.push(clean);
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_folders_before_their_files() {
        let root = std::env::temp_dir().join(format!("nuvira-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/z.txt"), b"hello").unwrap();
        std::fs::write(root.join("a/y.txt"), b"hi").unwrap();
        std::fs::write(root.join("one.txt"), b"1").unwrap();
        let listed = expand(&[root.join("a"), root.join("one.txt")]).unwrap();
        let paths: Vec<_> = listed.iter().map(|l| (l.path.as_str(), l.dir, l.size)).collect();
        assert_eq!(
            paths,
            [("a", true, 0), ("a/b", true, 0), ("a/b/z.txt", false, 5), ("a/y.txt", false, 2), ("one.txt", false, 1)]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn relative_paths_stay_inside() {
        assert_eq!(safe_relative("a/b.txt"), Some(PathBuf::from("a").join("b.txt")));
        assert_eq!(safe_relative("../../etc/passwd"), Some(PathBuf::from("etc").join("passwd")));
        assert_eq!(safe_relative("/abs/x"), Some(PathBuf::from("abs").join("x")));
        assert_eq!(safe_relative("C:\\Windows\\x"), Some(PathBuf::from("C_").join("Windows").join("x")));
        assert_eq!(safe_relative("a/<b>?.txt"), Some(PathBuf::from("a").join("_b__.txt")));
        assert_eq!(safe_relative(".."), None);
        assert_eq!(safe_relative(""), None);
    }
}
