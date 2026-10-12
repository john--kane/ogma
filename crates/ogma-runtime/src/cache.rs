use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
pub fn unique_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}
pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Cache is an optimization: invalid/inaccessible entries are misses, never errors.
#[derive(Clone)]
pub struct Cache {
    root: Option<PathBuf>,
}
impl Cache {
    pub fn new(path: &str) -> Self {
        if path.is_empty() {
            return Self { root: None };
        }
        let root = PathBuf::from(path).join("transforms-v1");
        if fs::create_dir_all(&root).is_err() {
            return Self { root: None };
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Only touch the dedicated cache directory requested by the frontend.
            if fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).is_err() {
                return Self { root: None };
            }
        }
        Self {
            root: root.canonicalize().ok(),
        }
    }
    pub fn root(&self) -> Option<&std::path::Path> {
        self.root.as_deref()
    }
    fn path(&self, key: &str) -> Option<PathBuf> {
        if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        Some(self.root.as_ref()?.join(key))
    }
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options.open(self.path(key)?).ok()?;
        if !file.metadata().ok()?.is_file() || file.metadata().ok()?.len() > 32 * 1024 * 1024 {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(32 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() < 32 || bytes.len() > 32 * 1024 * 1024 {
            return None;
        }
        let expected = Sha256::digest(&bytes[32..]);
        if bytes[..32] != expected[..] {
            return None;
        }
        Some(bytes.split_off(32))
    }
    pub fn put(&self, key: &str, bytes: &[u8]) {
        if bytes.len() > 32 * 1024 * 1024 - 32 {
            return;
        }
        let Some(path) = self.path(key) else {
            return;
        };
        let temp = path.with_extension(format!("tmp-{}-{}", std::process::id(), unique_id()));
        let result = (|| -> std::io::Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            file.write_all(&Sha256::digest(bytes))?;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            // Replacing a destination symlink replaces the link, never follows it.
            fs::rename(&temp, &path)?;
            #[cfg(unix)]
            if let Some(parent) = path.parent() {
                fs::File::open(parent)?.sync_all()?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn corruption_and_partial_writes_are_misses() {
        let path =
            std::env::temp_dir().join(format!("ogma-cache-{}-{}", std::process::id(), unique_id()));
        let cache = Cache::new(path.to_str().unwrap());
        let key = digest(b"source");
        cache.put(&key, b"code");
        assert_eq!(cache.get(&key), Some(b"code".to_vec()));
        fs::write(cache.root().unwrap().join(&key), b"partial").unwrap();
        assert!(cache.get(&key).is_none());
        let mut corrupt = vec![0u8; 32];
        corrupt.extend_from_slice(b"code");
        fs::write(cache.root().unwrap().join(&key), corrupt).unwrap();
        assert!(cache.get(&key).is_none());
        assert!(cache.get("../escape").is_none());
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn concurrent_atomic_writers_produce_complete_content() {
        let path =
            std::env::temp_dir().join(format!("ogma-cache-{}-{}", std::process::id(), unique_id()));
        let cache = Cache::new(path.to_str().unwrap());
        let key = digest(b"same");
        let handles = (0..8)
            .map(|index| {
                let cache = cache.clone();
                let key = key.clone();
                std::thread::spawn(move || cache.put(&key, &vec![index; 8192]))
            })
            .collect::<Vec<_>>();
        for handle in handles {
            handle.join().unwrap();
        }
        let bytes = cache.get(&key).unwrap();
        assert_eq!(bytes.len(), 8192);
        assert!(bytes.iter().all(|byte| *byte == bytes[0]));
        fs::remove_dir_all(path).unwrap();
    }
}
