//! Deterministic tool/context excerpts with recoverable, content-addressed originals.
use crate::{
    cache::digest,
    error::RuntimeError,
    generated::Status,
    model_store::{Pending, private_dir, valid_hash, verified_bytes},
};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};
const PASSTHROUGH: usize = 16 * 1024;
const MAX_CONTEXT: usize = 8 * 1024 * 1024;
const EXCERPT: usize = 8 * 1024;
pub fn root(store: &Path) -> PathBuf {
    store.join("contexts")
}
pub fn recover(store: &Path, hash: &str) -> Result<Vec<u8>, RuntimeError> {
    if !valid_hash(hash) {
        return Err(RuntimeError::new(
            Status::InvalidArgument,
            "invalid context SHA256",
        ));
    }
    private_dir(&root(store))?;
    verified_bytes(&root(store).join(hash), hash, MAX_CONTEXT)
}
/// Call only for explicit context and tool results, never ordinary user prompts.
pub fn compress(store: &Path, text: &str) -> Result<String, RuntimeError> {
    if text.len() > MAX_CONTEXT {
        return Err(RuntimeError::new(
            Status::ResourceLimit,
            "context exceeds 8 MiB",
        ));
    }
    if text.len() <= PASSTHROUGH {
        return Ok(text.to_owned());
    }
    let dir = root(store);
    private_dir(&dir)?;
    let hash = digest(text.as_bytes());
    let path = dir.join(&hash);
    if path.try_exists()? {
        verified_bytes(&path, &hash, MAX_CONTEXT)?;
    } else {
        let mut pending = Pending::new(&dir)?;
        pending.file.write_all(text.as_bytes())?;
        pending.publish(&path)?;
    }
    // Three bounded selections: complete head lines, complete tail lines, then
    // error-bearing lines in input order. Selected text remains byte-for-byte exact.
    let line_count = text.split_inclusive('\n').count();
    let mut selected = BTreeMap::new();
    let mut budget = 2048;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        if line.len() > budget {
            break;
        }
        selected.insert(index, line);
        budget -= line.len();
    }
    budget = 2048;
    for (offset, line) in text.split_inclusive('\n').rev().enumerate() {
        if line.len() > budget {
            break;
        }
        selected.insert(line_count - offset - 1, line);
        budget -= line.len();
    }
    budget = 2048;
    let mut errors = 0;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        if errors == 16 {
            break;
        }
        // ASCII matching does not allocate a lowercase copy of large lines.
        let is_error = [
            b"error".as_slice(),
            b"exception",
            b"panic",
            b"failed",
            b"fatal",
        ]
        .iter()
        .any(|word| {
            line.as_bytes()
                .windows(word.len())
                .any(|part| part.eq_ignore_ascii_case(word))
        });
        if is_error && !selected.contains_key(&index) && line.len() <= budget {
            selected.insert(index, line);
            budget -= line.len();
            errors += 1;
        }
    }
    let mut output = format!(
        "[ogma context excerpt; original {} bytes, {} lines; SHA256 {}; file contexts/{}; recover /ogma/context/{}]\n",
        text.len(),
        line_count,
        hash,
        hash,
        hash
    );
    let mut next = 0;
    for (index, line) in selected {
        if index > next {
            output.push_str(&format!("[ogma omitted {} complete lines]\n", index - next));
        }
        output.push_str(line);
        next = index + 1;
    }
    if next < line_count {
        if !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&format!(
            "[ogma omitted {} complete lines]\n",
            line_count - next
        ));
    }
    // Maximum selected content is 6144 bytes; at most 18 omission markers.
    if output.len() > EXCERPT {
        return Err(RuntimeError::new(
            Status::ResourceLimit,
            "context excerpt exceeds 8 KiB",
        ));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "ogma-context-{}-{}",
            std::process::id(),
            crate::cache::unique_id()
        ))
    }
    #[test]
    fn bounded_deterministic_and_exact_recovery() {
        let store = dir();
        let mut original = "first exact code line\n".to_owned();
        for i in 0..1200 {
            if i == 600 {
                original.push_str("panic: exact middle error with code();\n");
            }
            original.push_str(&format!("line {i:04} = some exact code content;\n"));
        }
        original.push_str("error: exact failure detail\nlast exact line\n");
        let excerpt = compress(&store, &original).unwrap();
        assert_eq!(excerpt, compress(&store, &original).unwrap());
        assert!(excerpt.len() <= EXCERPT);
        assert!(excerpt.contains("first exact code line\n"));
        assert!(excerpt.contains("panic: exact middle error with code();\n"));
        assert!(excerpt.contains("error: exact failure detail\n"));
        assert!(excerpt.contains("last exact line\n"));
        let hash = digest(original.as_bytes());
        assert_eq!(recover(&store, &hash).unwrap(), original.as_bytes());
        std::fs::write(root(&store).join(&hash), b"tamper").unwrap();
        assert!(recover(&store, &hash).is_err());
        std::fs::remove_dir_all(store).unwrap();
    }
    #[test]
    fn oversized_lines_have_no_partial_code() {
        let store = dir();
        let text = format!("{}\nerror: preserved\n", "x".repeat(20000));
        let excerpt = compress(&store, &text).unwrap();
        assert!(!excerpt.contains("xxxx"));
        assert!(excerpt.contains("error: preserved\n"));
        assert_eq!(
            recover(&store, &digest(text.as_bytes())).unwrap(),
            text.as_bytes()
        );
        std::fs::remove_dir_all(store).unwrap();
    }
    #[test]
    fn small_input_is_unchanged_and_hash_traversal_rejected() {
        let store = dir();
        assert_eq!(compress(&store, "hello\n").unwrap(), "hello\n");
        assert!(!store.exists());
        assert!(recover(&store, "../../etc/passwd").is_err());
    }
    #[cfg(unix)]
    #[test]
    fn recovery_rejects_symlink() {
        let store = dir();
        private_dir(&root(&store)).unwrap();
        let hash = digest(b"content");
        let source = store.join("source");
        std::fs::write(&source, b"content").unwrap();
        std::os::unix::fs::symlink(&source, root(&store).join(&hash)).unwrap();
        assert!(recover(&store, &hash).is_err());
        std::fs::remove_dir_all(store).unwrap();
    }
}
