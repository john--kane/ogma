//! Permission-gated, streaming GGUF ingestion and pinned Hugging Face resolution.
use crate::{
    cache::{digest, unique_id},
    error::RuntimeError,
    generated::{Request, Status},
    permissions::{PermissionSet, validate_net_url_text},
};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::Duration,
};

pub(crate) const MAX_MODEL: u64 = 64 * 1024 * 1024 * 1024;
const MAX_METADATA: usize = 8 * 1024 * 1024;
fn invalid(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(Status::InvalidArgument, message)
}
pub(crate) fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Reject symlinks in all store path components, including pre-existing parents.
pub(crate) fn private_dir(path: &Path) -> Result<(), RuntimeError> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        if component == Component::ParentDir {
            return Err(invalid("store paths cannot contain '..'"));
        }
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err(invalid("model store must contain only real directories"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut builder = fs::DirBuilder::new();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                match builder.create(&current) {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e.into()),
                }
                let meta = fs::symlink_metadata(&current)?;
                if meta.file_type().is_symlink() || !meta.is_dir() {
                    return Err(invalid("invalid model store directory"));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&current, fs::Permissions::from_mode(0o700))?;
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
pub(crate) fn open_regular(path: &Path, max: u64) -> Result<File, RuntimeError> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(invalid("model/context source cannot be a symlink"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(invalid(
            "model/context source must be a regular file (no symlinks)",
        ));
    }
    if meta.len() > max {
        return Err(RuntimeError::new(
            Status::ResourceLimit,
            "model/context file exceeds size limit",
        ));
    }
    Ok(file)
}
pub(crate) struct Pending {
    pub path: PathBuf,
    pub file: File,
}
impl Pending {
    pub(crate) fn new(dir: &Path) -> Result<Self, RuntimeError> {
        let path = dir.join(format!(".tmp-{}-{}", std::process::id(), unique_id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&path)?;
        Ok(Self { path, file })
    }
    pub(crate) fn publish(&self, target: &Path) -> Result<(), RuntimeError> {
        if let Ok(meta) = fs::symlink_metadata(target)
            && (meta.file_type().is_symlink() || !meta.is_file())
        {
            return Err(invalid("store entry must be a regular file"));
        }
        self.file.sync_data()?;
        fs::rename(&self.path, target)?;
        File::open(
            target
                .parent()
                .ok_or_else(|| invalid("invalid store target"))?,
        )?
        .sync_all()?;
        Ok(())
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
pub(crate) fn verified_bytes(path: &Path, hash: &str, max: usize) -> Result<Vec<u8>, RuntimeError> {
    if !valid_hash(hash) {
        return Err(invalid("invalid SHA256 digest"));
    }
    let mut bytes = Vec::new();
    open_regular(path, max as u64)?
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max || digest(&bytes) != hash {
        return Err(invalid("stored content SHA256 mismatch"));
    }
    Ok(bytes)
}
fn verified_model(path: &Path, expected: &str) -> Result<(), RuntimeError> {
    if !valid_hash(expected) {
        return Err(invalid("invalid model SHA256"));
    }
    let mut file = open_regular(path, MAX_MODEL)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        if size == 0 && (n < 4 || &buffer[..4] != b"GGUF") {
            return Err(invalid("model is not a GGUF file"));
        }
        size += n as u64;
        if size > MAX_MODEL {
            return Err(RuntimeError::new(
                Status::ResourceLimit,
                "model exceeds 64 GiB",
            ));
        }
        hash.update(&buffer[..n]);
    }
    if size < 4 || format!("{:x}", hash.finalize()) != expected {
        return Err(invalid("stored model SHA256 mismatch"));
    }
    Ok(())
}
pub fn store_root(request: &Request) -> Result<PathBuf, RuntimeError> {
    if request.model.store.is_empty() {
        Ok(std::env::current_dir()?.join(".ogma/models"))
    } else {
        Ok(PathBuf::from(&request.model.store))
    }
}
fn ingest_local(source: &Path, blobs: &Path) -> Result<PathBuf, RuntimeError> {
    let mut input = open_regular(source, MAX_MODEL)?;
    let mut pending = Pending::new(blobs)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    let mut size = 0u64;
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        if size == 0 && (n < 4 || &buffer[..4] != b"GGUF") {
            return Err(invalid("model is not a GGUF file"));
        }
        size += n as u64;
        if size > MAX_MODEL {
            return Err(RuntimeError::new(
                Status::ResourceLimit,
                "model exceeds 64 GiB",
            ));
        }
        hash.update(&buffer[..n]);
        pending.file.write_all(&buffer[..n])?;
    }
    if size < 4 {
        return Err(invalid("model is not a GGUF file"));
    }
    let hash = format!("{:x}", hash.finalize());
    let target = blobs.join(format!("{hash}.gguf"));
    if target.try_exists()? {
        verified_model(&target, &hash)?;
    } else {
        pending.publish(&target)?;
    }
    Ok(target)
}
#[derive(serde::Serialize, serde::Deserialize)]
struct Index {
    selector: String,
    repo: String,
    commit: String,
    filename: String,
    hash: String,
}
fn parse_hf(selector: &str) -> Result<(String, Option<String>), RuntimeError> {
    let selector = selector.strip_prefix("hf:").unwrap_or(selector);
    let (repo, filename) = selector
        .split_once("::")
        .map_or((selector, None), |(r, f)| (r, Some(f.to_owned())));
    let parts: Vec<_> = repo.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|p| {
            p.is_empty()
                || *p == "."
                || *p == ".."
                || !p
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
    {
        return Err(invalid(
            "expected hf:owner/repo::filename.gguf or owner/repo",
        ));
    }
    if let Some(file) = &filename {
        validate_filename(file)?;
    }
    Ok((repo.to_owned(), filename))
}
fn validate_filename(file: &str) -> Result<(), RuntimeError> {
    if !file.to_ascii_lowercase().ends_with(".gguf")
        || file.len() > 4096
        || file.split('/').any(|p| {
            p.is_empty()
                || p == "."
                || p == ".."
                || p.contains(['\\', ':', '?', '#', '%'])
                || p.chars().any(|c| c.is_control())
        })
    {
        return Err(invalid("invalid GGUF filename"));
    }
    if file.contains("-of-") {
        return Err(RuntimeError::new(
            Status::Unsupported,
            "split GGUF shards are unsupported; select a single-file model",
        ));
    }
    Ok(())
}
async fn get(
    client: &reqwest::Client,
    grants: &PermissionSet,
    text: &str,
    timeout: Duration,
) -> Result<reqwest::Response, RuntimeError> {
    let mut url = grants.net_url(text)?;
    for _ in 0..=10 {
        let response = client
            .get(url.as_str())
            .timeout(timeout)
            .send()
            .await
            .map_err(remote_error)?;
        if response.status().is_redirection() {
            let raw = response
                .headers()
                .get(reqwest::header::LOCATION)
                .ok_or_else(|| invalid("redirect has no Location"))?
                .to_str()
                .map_err(|_| invalid("invalid redirect Location"))?;
            validate_net_url_text(raw)?;
            let next = url.join(raw).map_err(|_| invalid("invalid redirect URL"))?;
            url = grants.net_url(next.as_str())?;
            if url.scheme() != "https" {
                return Err(invalid("Hugging Face redirects must use HTTPS"));
            }
        } else {
            if !response.status().is_success() {
                return Err(RuntimeError::new(
                    Status::Io,
                    format!("Hugging Face HTTP {}", response.status()),
                ));
            }
            return Ok(response);
        }
    }
    Err(invalid("too many Hugging Face redirects"))
}
fn remote_error(error: reqwest::Error) -> RuntimeError {
    RuntimeError::new(
        if error.is_timeout() {
            Status::Timeout
        } else {
            Status::Io
        },
        error.to_string(),
    )
}
pub async fn resolve_model(request: &Request) -> Result<PathBuf, RuntimeError> {
    if !request.permissions.model {
        return Err(RuntimeError::new(
            Status::PermissionDenied,
            "model operations require allow-model",
        ));
    }
    let selector = &request.model.model;
    if selector.is_empty() || selector.len() > 32768 || selector.contains('\0') {
        return Err(invalid(
            "a model path or Hugging Face identifier is required",
        ));
    }
    let root = store_root(request)?;
    private_dir(&root)?;
    let blobs = root.join("blobs");
    private_dir(&blobs)?;
    if !selector.starts_with("hf:")
        && (Path::new(selector).exists()
            || selector.starts_with(['/', '.'])
            || selector.contains('\\')
            || selector.to_ascii_lowercase().ends_with(".gguf"))
    {
        return ingest_local(Path::new(selector), &blobs);
    }
    let (repo, requested) = parse_hf(selector)?;
    let indexes = root.join("indexes");
    private_dir(&indexes)?;
    let index_path = indexes.join(format!("{}.json", digest(selector.as_bytes())));
    if fs::symlink_metadata(&index_path).is_ok() {
        let mut bytes = Vec::new();
        open_regular(&index_path, MAX_METADATA as u64)?
            .take(MAX_METADATA as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_METADATA {
            return Err(invalid("model index exceeds limit"));
        }
        let index: Index =
            serde_json::from_slice(&bytes).map_err(|_| invalid("invalid model index"))?;
        if index.selector != *selector
            || index.repo != repo
            || requested.as_ref().is_some_and(|f| *f != index.filename)
            || index.commit.len() != 40
            || !index.commit.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(invalid("model index does not match selector"));
        }
        validate_filename(&index.filename)?;
        let path = blobs.join(format!("{}.gguf", index.hash));
        verified_model(&path, &index.hash)?;
        return Ok(path);
    }
    let grants = PermissionSet::new(&request.permissions)?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(3600))
        .build()
        .map_err(remote_error)?;
    let mut metadata_response = get(
        &client,
        &grants,
        &format!("https://huggingface.co/api/models/{repo}?blobs=true"),
        Duration::from_secs(60),
    )
    .await?;
    if metadata_response
        .content_length()
        .is_some_and(|n| n > MAX_METADATA as u64)
    {
        return Err(RuntimeError::new(
            Status::ResourceLimit,
            "Hugging Face metadata exceeds 8 MiB",
        ));
    }
    let mut metadata = Vec::new();
    while let Some(chunk) = metadata_response.chunk().await.map_err(remote_error)? {
        if chunk.len() > MAX_METADATA - metadata.len() {
            return Err(RuntimeError::new(
                Status::ResourceLimit,
                "Hugging Face metadata exceeds 8 MiB",
            ));
        }
        metadata.extend_from_slice(&chunk);
    }
    let value: serde_json::Value =
        serde_json::from_slice(&metadata).map_err(|_| invalid("invalid Hugging Face metadata"))?;
    let commit = value["sha"]
        .as_str()
        .filter(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| invalid("Hugging Face metadata requires exact commit SHA"))?;
    let candidates: Vec<_> = value["siblings"]
        .as_array()
        .ok_or_else(|| invalid("missing Hugging Face file list"))?
        .iter()
        .filter(|s| {
            s["rfilename"]
                .as_str()
                .is_some_and(|f| f.to_ascii_lowercase().ends_with(".gguf"))
        })
        .collect();
    let selected = if let Some(filename) = requested {
        candidates
            .iter()
            .copied()
            .find(|s| s["rfilename"].as_str() == Some(&filename))
            .ok_or_else(|| invalid("requested GGUF filename is absent"))?
    } else if candidates.len() == 1 {
        candidates[0]
    } else {
        let q4: Vec<_> = candidates
            .iter()
            .copied()
            .filter(|s| {
                s["rfilename"]
                    .as_str()
                    .unwrap()
                    .to_ascii_uppercase()
                    .contains("Q4_K_M")
            })
            .collect();
        if q4.len() == 1 {
            q4[0]
        } else {
            return Err(invalid(format!(
                "select an exact GGUF filename with hf:owner/repo::filename.gguf; candidates: {}",
                candidates
                    .iter()
                    .filter_map(|s| s["rfilename"].as_str())
                    .take(30)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    };
    let filename = selected["rfilename"].as_str().unwrap();
    validate_filename(filename)?;
    let expected = selected["lfs"]["sha256"]
        .as_str()
        .or_else(|| selected["lfs"]["oid"].as_str())
        .map(|s| s.strip_prefix("sha256:").unwrap_or(s));
    if expected.is_some_and(|s| !valid_hash(s)) {
        return Err(invalid("invalid Hugging Face LFS SHA256"));
    }
    let mut url = url::Url::parse(&format!("https://huggingface.co/{repo}/resolve/{commit}/"))
        .map_err(|_| invalid("invalid model URL"))?;
    url.path_segments_mut()
        .map_err(|_| invalid("invalid model URL"))?
        .pop_if_empty()
        .extend(filename.split('/'));
    let mut response = get(&client, &grants, url.as_str(), Duration::from_secs(3600)).await?;
    if response.content_length().is_some_and(|n| n > MAX_MODEL) {
        return Err(RuntimeError::new(
            Status::ResourceLimit,
            "model exceeds 64 GiB",
        ));
    }
    let mut pending = Pending::new(&blobs)?;
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut magic = Vec::with_capacity(4);
    while let Some(chunk) = response.chunk().await.map_err(remote_error)? {
        size += chunk.len() as u64;
        if size > MAX_MODEL {
            return Err(RuntimeError::new(
                Status::ResourceLimit,
                "model exceeds 64 GiB",
            ));
        }
        if magic.len() < 4 {
            magic.extend_from_slice(&chunk[..chunk.len().min(4 - magic.len())]);
            if magic.len() == 4 && magic != b"GGUF" {
                return Err(invalid("download is not a GGUF file"));
            }
        }
        hash.update(&chunk);
        pending.file.write_all(&chunk)?;
    }
    if magic != b"GGUF" {
        return Err(invalid("download is not a GGUF file"));
    }
    let hash = format!("{:x}", hash.finalize());
    if expected.is_some_and(|expected| expected != hash) {
        return Err(invalid("downloaded model LFS SHA256 mismatch"));
    }
    let target = blobs.join(format!("{hash}.gguf"));
    if target.try_exists()? {
        verified_model(&target, &hash)?;
    } else {
        pending.publish(&target)?;
    }
    let index = Index {
        selector: selector.clone(),
        repo,
        commit: commit.to_owned(),
        filename: filename.to_owned(),
        hash,
    };
    let mut pending = Pending::new(&indexes)?;
    pending.file.write_all(
        &serde_json::to_vec(&index).map_err(|_| invalid("cannot encode model index"))?,
    )?;
    pending.publish(&index_path)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "ogma-model-store-{}-{}",
            std::process::id(),
            unique_id()
        ))
    }
    #[tokio::test]
    async fn local_ingestion_is_private_hashed_and_preserves_original() {
        let dir = dir();
        fs::create_dir(&dir).unwrap();
        let original = dir.join("source.gguf");
        let body = b"GGUFfixture";
        fs::write(&original, body).unwrap();
        let mut request = Request::default();
        request.permissions.model = true;
        request.model.model = original.to_string_lossy().into();
        request.model.store = dir.join("store").to_string_lossy().into();
        let path = resolve_model(&request).await.unwrap();
        assert_eq!(
            path.file_name().unwrap(),
            format!("{}.gguf", digest(body)).as_str()
        );
        assert_eq!(fs::read(&original).unwrap(), body);
        assert_eq!(resolve_model(&request).await.unwrap(), path);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        fs::write(&path, b"GGUFtampered").unwrap();
        assert!(resolve_model(&request).await.is_err());
        fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn model_capability_and_remote_network_are_explicit() {
        let dir = dir();
        let mut request = Request::default();
        request.model.store = dir.to_string_lossy().into();
        request.model.model = "hf:owner/repo".into();
        assert!(matches!(
            resolve_model(&request).await.unwrap_err().status,
            Status::PermissionDenied
        ));
        assert!(!dir.exists());
        request.permissions.model = true;
        assert!(matches!(
            resolve_model(&request).await.unwrap_err().status,
            Status::PermissionDenied
        ));
        fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn pinned_index_works_offline_and_detects_corruption() {
        let dir = dir();
        let blobs = dir.join("blobs");
        let indexes = dir.join("indexes");
        private_dir(&blobs).unwrap();
        private_dir(&indexes).unwrap();
        let selector = "hf:owner/repo::model.Q4_K_M.gguf";
        let hash = digest(b"GGUFfixture");
        let target = blobs.join(format!("{hash}.gguf"));
        fs::write(&target, b"GGUFfixture").unwrap();
        let index = Index {
            selector: selector.into(),
            repo: "owner/repo".into(),
            commit: "a".repeat(40),
            filename: "model.Q4_K_M.gguf".into(),
            hash,
        };
        fs::write(
            indexes.join(format!("{}.json", digest(selector.as_bytes()))),
            serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();
        let mut request = Request::default();
        request.permissions.model = true;
        request.model.store = dir.to_string_lossy().into();
        request.model.model = selector.into();
        assert_eq!(resolve_model(&request).await.unwrap(), target);
        fs::write(&target, b"GGUFtamper").unwrap();
        assert!(resolve_model(&request).await.is_err());
        fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn redirects_require_a_fresh_exact_host_grant() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).unwrap() > 0);
            socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: https://not-granted.invalid/model.gguf\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let mut permissions = crate::generated::Permissions::default();
        permissions.net.push(address.to_string());
        let grants = PermissionSet::new(&permissions).unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let error = get(
            &client,
            &grants,
            &format!("http://{address}/start"),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(matches!(error.status, Status::PermissionDenied));
        assert!(error.message.contains("not-granted.invalid"));
        server.join().unwrap();
    }
    #[test]
    fn bounded_regular_reads_reject_sparse_oversize_and_non_gguf() {
        let dir = dir();
        private_dir(&dir).unwrap();
        let source = dir.join("source");
        let file = File::create(&source).unwrap();
        file.set_len(MAX_MODEL + 1).unwrap();
        assert!(open_regular(&source, MAX_MODEL).is_err());
        fs::write(&source, b"HTML-not-a-model").unwrap();
        assert!(ingest_local(&source, &dir).is_err());
        assert!(fs::read_dir(&dir).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".tmp-")
        }));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn identifiers_reject_traversal_and_split_models() {
        for selector in [
            "hf:../repo",
            "hf:owner/repo::../model.gguf",
            "hf:owner/repo::x-00001-of-00003.gguf",
            "hf:owner/repo::x?foo.gguf",
        ] {
            assert!(parse_hf(selector).is_err());
        }
    }
    #[cfg(unix)]
    #[test]
    fn store_and_source_symlinks_are_rejected() {
        let dir = dir();
        fs::create_dir(&dir).unwrap();
        let source = dir.join("source");
        fs::write(&source, b"GGUFfixture").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&source, &link).unwrap();
        assert!(open_regular(&link, MAX_MODEL).is_err());
        let linkdir = dir.join("linkdir");
        std::os::unix::fs::symlink(&dir, &linkdir).unwrap();
        assert!(private_dir(&linkdir.join("store")).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
