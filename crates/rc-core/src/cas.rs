//! Content-addressed storage.
//!
//! blake3 is the only key. The same helper is used by the server (authoritative
//! store) and the worker (local read-through cache), so a blob written by one
//! is byte-identical for the other.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub const HASH_HEX_LEN: usize = 64;

/// Chunk size for streaming uploads/downloads. Kept under the default gRPC
/// 4 MiB message limit with room for framing.
pub const CHUNK_SIZE: usize = 1024 * 1024;

pub fn hash_bytes(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

pub fn hash_file(path: &Path) -> io::Result<String> {
    let mut f = fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

pub fn is_valid_hash(h: &str) -> bool {
    h.len() == HASH_HEX_LEN && h.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Filesystem CAS, sharded two levels deep to keep directory sizes sane.
#[derive(Clone, Debug)]
pub struct FsCas {
    root: PathBuf,
}

impl FsCas {
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("tmp"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path_of(&self, hash: &str) -> PathBuf {
        self.root
            .join("blobs")
            .join(&hash[0..2])
            .join(&hash[2..4])
            .join(hash)
    }

    pub fn exists(&self, hash: &str) -> bool {
        is_valid_hash(hash) && self.path_of(hash).is_file()
    }

    pub fn size_of(&self, hash: &str) -> Option<u64> {
        fs::metadata(self.path_of(hash)).ok().map(|m| m.len())
    }

    pub fn get(&self, hash: &str) -> io::Result<Vec<u8>> {
        if !is_valid_hash(hash) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "malformed hash"));
        }
        fs::read(self.path_of(hash))
    }

    pub fn open_read(&self, hash: &str) -> io::Result<fs::File> {
        fs::File::open(self.path_of(hash))
    }

    /// Write a blob, verifying that its content really hashes to `hash`.
    /// A peer that lies about a hash would poison every future lookup.
    pub fn put_verified(&self, hash: &str, data: &[u8]) -> io::Result<()> {
        if !is_valid_hash(hash) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "malformed hash"));
        }
        let actual = hash_bytes(data);
        if actual != hash {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("blob hash mismatch: declared {hash}, actual {actual}"),
            ));
        }
        self.put_trusted(hash, data)
    }

    /// Write without re-hashing (caller already verified while streaming).
    pub fn put_trusted(&self, hash: &str, data: &[u8]) -> io::Result<()> {
        let dest = self.path_of(hash);
        if dest.is_file() {
            return Ok(());
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.root.join("tmp").join(format!("{hash}.{}", std::process::id()));
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(data)?;
            f.sync_all()?;
        }
        // rename is atomic within a filesystem; a concurrent writer producing
        // the same content is harmless because the content is the key.
        match fs::rename(&tmp, &dest) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                if dest.is_file() {
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Convenience: hash and store in one step, returning the key.
    pub fn put(&self, data: &[u8]) -> io::Result<String> {
        let hash = hash_bytes(data);
        self.put_trusted(&hash, data)?;
        Ok(hash)
    }

    pub fn remove(&self, hash: &str) -> io::Result<()> {
        match fs::remove_file(self.path_of(hash)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Total bytes on disk and blob count. Walks the shard tree; only used by
    /// the Storage page and GC, never on a hot path.
    pub fn usage(&self) -> (u64, u64) {
        fn walk(dir: &Path, bytes: &mut u64, count: &mut u64) {
            let Ok(rd) = fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let Ok(ft) = e.file_type() else { continue };
                if ft.is_dir() {
                    walk(&e.path(), bytes, count);
                } else if let Ok(m) = e.metadata() {
                    *bytes += m.len();
                    *count += 1;
                }
            }
        }
        let mut bytes = 0;
        let mut count = 0;
        walk(&self.root.join("blobs"), &mut bytes, &mut count);
        (bytes, count)
    }
}

/// Build logs are stored compressed (§9): they are large, highly compressible
/// and read rarely.
pub fn compress_log(text: &str) -> io::Result<Vec<u8>> {
    zstd::encode_all(text.as_bytes(), 3)
}

pub fn decompress_log(data: &[u8]) -> io::Result<String> {
    let raw = zstd::decode_all(data)?;
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("rc-cas-test-{tag}-{}", ulid::Ulid::generate()));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn roundtrip() {
        let cas = FsCas::open(tmpdir("rt")).unwrap();
        let h = cas.put(b"hello").unwrap();
        assert!(cas.exists(&h));
        assert_eq!(cas.get(&h).unwrap(), b"hello");
        assert_eq!(cas.size_of(&h), Some(5));
    }

    #[test]
    fn identical_content_stores_once() {
        let cas = FsCas::open(tmpdir("dedupe")).unwrap();
        let a = cas.put(b"same bytes").unwrap();
        let b = cas.put(b"same bytes").unwrap();
        assert_eq!(a, b);
        assert_eq!(cas.usage().1, 1);
    }

    #[test]
    fn a_lying_hash_is_rejected() {
        let cas = FsCas::open(tmpdir("liar")).unwrap();
        let wrong = "0".repeat(64);
        let err = cas.put_verified(&wrong, b"payload").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(!cas.exists(&wrong));
    }

    #[test]
    fn malformed_hashes_never_touch_the_filesystem() {
        let cas = FsCas::open(tmpdir("malformed")).unwrap();
        assert!(!cas.exists("../../etc/passwd"));
        assert!(cas.get("nope").is_err());
    }

    #[test]
    fn log_compression_roundtrips() {
        let text = "error[E0308]: mismatched types\n".repeat(500);
        let packed = compress_log(&text).unwrap();
        assert!(packed.len() < text.len() / 10);
        assert_eq!(decompress_log(&packed).unwrap(), text);
    }
}
