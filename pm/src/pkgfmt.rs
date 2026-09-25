//! The `.hspkg` package archive format.
//!
//! HardScript packages are self-contained archives with a tiny, dependency-free
//! layout so extraction is safe and deterministic (sorted file order):
//!
//! ```text
//! magic "HSPKG\x00\x01"       7 bytes
//! u32  entry count            little-endian
//! per file:
//!   u64  name length          little-endian
//!   bytes relative path       UTF-8, '/' separated, '..' forbidden
//!   u64  size                 little-endian
//!   bytes file contents
//! ```
//!
//! The header is 11 bytes (7 magic + 4 count); the rest is the file records.

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

pub const MAGIC: &[u8; 7] = b"HSPKG\x00\x01";
const MAX_NAME: usize = 4096;

/// A file record before packing.
#[derive(Clone, Debug)]
pub struct FileRecord {
    pub rel_path: String,
    pub data: Vec<u8>,
}

/// Error produced while packing or unpacking an archive.
#[derive(Clone, Debug)]
pub struct PkgError {
    pub message: String,
}

impl PkgError {
    fn new(message: impl Into<String>) -> PkgError {
        PkgError {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PkgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Collect every file under `dir` with its relative path, sorted by path
/// (the packing order is deterministic).
pub fn collect_files(dir: &Path) -> Result<Vec<FileRecord>, PkgError> {
    let mut files = Vec::new();
    collect_into(dir, dir, &mut files)?;
    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(files)
}

fn collect_into(root: &Path, dir: &Path, out: &mut Vec<FileRecord>) -> Result<(), PkgError> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| PkgError::new(format!("cannot read {}: {e}", dir.display())))?
        .collect::<Result<_, _>>()
        .map_err(|e| PkgError::new(format!("cannot read dir entry: {e}")))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => match path.metadata() {
                Ok(m) => m.file_type(),
                Err(_) => continue,
            },
        };
        if ft.is_dir() {
            collect_into(root, &path, out)?;
        } else if ft.is_file() {
            let rel = path.strip_prefix(root).map_err(|_| {
                PkgError::new(format!("path {} escapes root", path.display()))
            })?;
            let data = std::fs::read(&path)
                .map_err(|e| PkgError::new(format!("cannot read {}: {e}", path.display())))?;
            out.push(FileRecord {
                rel_path: slashed(rel),
                data,
            });
        }
    }
    Ok(())
}

fn slashed(p: &Path) -> String {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Serialize file records into a `.hspkg` archive.
pub fn pack(files: &[FileRecord]) -> Result<Vec<u8>, PkgError> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(files.len() as u32).to_le_bytes());
    for f in files {
        let name = f.rel_path.as_bytes();
        if name.len() > MAX_NAME {
            return Err(PkgError::new(format!(
                "path too long in package: {}",
                f.rel_path
            )));
        }
        out.extend_from_slice(&(name.len() as u64).to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(&(f.data.len() as u64).to_le_bytes());
        out.extend_from_slice(&f.data);
    }
    Ok(out)
}

/// A single extracted file.
#[derive(Clone, Debug)]
pub struct ExtractedFile {
    pub rel_path: String,
    pub data: Vec<u8>,
}

/// Explode an archive; returns the file records without writing them.
pub fn read_archive(bytes: &[u8]) -> Result<Vec<ExtractedFile>, PkgError> {
    if bytes.len() < 7 || &bytes[..7] != MAGIC {
        return Err(PkgError::new(
            "not a HardScript package archive (bad magic)",
        ));
    }
    if bytes.len() < 11 {
        return Err(PkgError::new("archive is truncated"));
    }
    let count = u32::from_le_bytes([bytes[7], bytes[8], bytes[9], bytes[10]]) as usize;
    let mut cursor = 11usize;
    let mut files = Vec::new();
    for _ in 0..count {
        if cursor + 8 > bytes.len() {
            return Err(PkgError::new("archive truncated in name length"));
        }
        let nlen = u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap()) as usize;
        cursor += 8;
        if cursor + nlen > bytes.len() || nlen > MAX_NAME {
            return Err(PkgError::new("archive truncated in file name"));
        }
        let rel = String::from_utf8(bytes[cursor..cursor + nlen].to_vec())
            .map_err(|_| PkgError::new("file name is not valid UTF-8"))?;
        cursor += nlen;
        if cursor + 8 > bytes.len() {
            return Err(PkgError::new("archive truncated in size"));
        }
        let size = u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap()) as usize;
        cursor += 8;
        if cursor + size > bytes.len() {
            return Err(PkgError::new("archive truncated in file data"));
        }
        let data = bytes[cursor..cursor + size].to_vec();
        cursor += size;
        validate_rel(&rel)?;
        files.push(ExtractedFile { rel_path: rel, data });
    }
    if cursor != bytes.len() {
        return Err(PkgError::new("trailing bytes after package data"));
    }
    Ok(files)
}

/// Write an archive into `out_dir`, validating every path stays inside it.
pub fn unpack(bytes: &[u8], out_dir: &Path) -> Result<(), PkgError> {
    let files = read_archive(bytes)?;
    for f in &files {
        let dest = out_dir.join(&f.rel_path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| PkgError::new(format!("cannot create {}: {e}", parent.display())))?;
        }
        let mut file =
            std::fs::File::create(&dest).map_err(|e| PkgError::new(format!(
                "cannot write {}: {e}",
                dest.display()
            )))?;
        file.write_all(&f.data)
            .map_err(|e| PkgError::new(format!("write failed for {}: {e}", dest.display())))?;
    }
    Ok(())
}

/// Refuse path traversal and absolute/odd paths from an archive.
fn validate_rel(rel: &str) -> Result<(), PkgError> {
    if rel.is_empty() {
        return Err(PkgError::new("empty file name in package"));
    }
    let p = Path::new(rel);
    for c in p.components() {
        match c {
            Component::ParentDir => {
                return Err(PkgError::new(format!(
                    "package contains '..' path: '{rel}'"
                )));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(PkgError::new(format!(
                    "package contains absolute path: '{rel}'"
                )));
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(())
}

/// Convenience: pack a directory into an archive.
pub fn pack_dir(dir: &Path) -> Result<Vec<u8>, PkgError> {
    let files = collect_files(dir)?;
    pack(&files)
}

/// Convenience: extract an archive file from disk into `out_dir`.
pub fn unpack_file(archive_path: &Path, out_dir: &Path) -> Result<Vec<ExtractedFile>, PkgError> {
    let bytes = std::fs::read(archive_path)
        .map_err(|e| PkgError::new(format!("cannot read {}: {e}", archive_path.display())))?;
    unpack(&bytes, out_dir)?;
    read_archive(&bytes)
}

/// A minimal reader so `Read` isn't needed elsewhere: read into Vec.
pub fn read_all<R: Read>(mut r: R) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Hex digest helper delegating to the shared compiler SHA-256.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hs_compiler::sha256::hex(bytes)
}

/// Return the on-disk path a package archive would use in the cache.
pub fn archive_rel(name: &str, version: &str) -> PathBuf {
    PathBuf::from(format!("packages/{name}/{version}/{name}-{version}.hspkg"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_read_roundtrip() {
        let files = vec![
            FileRecord {
                rel_path: "hard.toml".to_string(),
                data: b"name = \"demo\"\n".to_vec(),
            },
            FileRecord {
                rel_path: "src/main.hard".to_string(),
                data: b"bring http\napp @3000\n".to_vec(),
            },
        ];
        let archive = pack(&files).unwrap();
        assert_eq!(&archive[..7], MAGIC);
        let extracted = read_archive(&archive).unwrap();
        let got: Vec<(String, Vec<u8>)> = extracted
            .iter()
            .map(|f| (f.rel_path.clone(), f.data.clone()))
            .collect();
        let want: Vec<(String, Vec<u8>)> = files
            .iter()
            .map(|f| (f.rel_path.clone(), f.data.clone()))
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn unpack_roundtrip_disk() {
        let files = vec![FileRecord {
            rel_path: "nested/hello.hard".to_string(),
            data: b"<- { ok: true }".to_vec(),
        }];
        let archive = pack(&files).unwrap();
        let tmp = std::env::temp_dir().join(format!("hs-pkgfmt-test-{}", std::process::id()));
        let out = tmp.join("out");
        unpack(&archive, &out).unwrap();
        assert_eq!(
            std::fs::read_to_string(out.join("nested/hello.hard")).unwrap(),
            "<- { ok: true }"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn rejects_path_traversal() {
        let archive = pack(&[FileRecord {
            rel_path: "../evil.txt".to_string(),
            data: b"evil".to_vec(),
        }])
        .unwrap();
        let err = read_archive(&archive).unwrap_err();
        assert!(
            err.message.contains("'..'"),
            "unexpected message: {}",
            err.message
        );

        // unpack must not write outside the destination either
        let tmp = std::env::temp_dir().join(format!("hs-pkgfmt-x-{}", std::process::id()));
        let out = tmp.join("out");
        let err = unpack(&archive, &out).unwrap_err();
        assert!(err.message.contains("'..'"), "unexpected message: {}", err.message);
        assert!(!tmp.join("evil.txt").exists());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn rejects_bad_magic() {
        let err = read_archive(b"NOPE\x00\x01abc").unwrap_err();
        assert!(err.message.contains("bad magic"));
    }
}