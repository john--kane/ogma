use crate::{
    error::RuntimeError,
    generated::{Permissions, Status},
};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
};

/// Also used on raw redirect locations before URL normalization can discard userinfo.
pub(crate) fn validate_net_url_text(text: &str) -> Result<(), RuntimeError> {
    if text.len() > 32768
        || text.contains('\\')
        || text.chars().any(|c| c.is_control())
        || text.trim() != text
    {
        return Err(RuntimeError::new(
            Status::InvalidArgument,
            "invalid HTTP URL text",
        ));
    }
    if let Some(authority) = text
        .strip_prefix("//")
        .or_else(|| text.split_once("://").map(|(_, authority)| authority))
        && authority
            .split(['/', '?', '#'])
            .next()
            .is_some_and(|a| a.contains('@'))
    {
        return Err(RuntimeError::new(
            Status::InvalidArgument,
            "HTTP URL userinfo is unsupported",
        ));
    }
    Ok(())
}

const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
#[derive(Debug)]
struct NetGrant {
    host: url::Host<String>,
    port: Option<u16>,
}
impl NetGrant {
    fn parse(text: &str) -> Result<Self, RuntimeError> {
        let invalid = || {
            RuntimeError::new(
                Status::InvalidArgument,
                "allow-net must be an exact host or host:port; IPv6 requires [address]",
            )
        };
        if text.is_empty()
            || text.chars().any(|c| c.is_control() || c.is_whitespace())
            || text.contains(['/', '@', '*', '?', '#', '%'])
        {
            return Err(invalid());
        }
        let (host, port) = if text.starts_with('[') {
            let end = text.find(']').ok_or_else(invalid)?;
            let host = &text[..=end];
            let suffix = &text[end + 1..];
            (
                host,
                if suffix.is_empty() {
                    None
                } else {
                    Some(suffix.strip_prefix(':').ok_or_else(invalid)?)
                },
            )
        } else {
            if text.matches(':').count() > 1 {
                return Err(invalid());
            }
            text.split_once(':')
                .map_or((text, None), |(host, port)| (host, Some(port)))
        };
        let port = port
            .map(|p| {
                if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(invalid());
                }
                p.parse::<u16>()
                    .ok()
                    .filter(|p| *p != 0)
                    .ok_or_else(invalid)
            })
            .transpose()?;
        let host = url::Host::parse(host).map_err(|_| invalid())?;
        if let url::Host::Domain(domain) = &host
            && domain.split('.').any(|label| {
                label.is_empty()
                    || label.starts_with('-')
                    || label.ends_with('-')
                    || !label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
        {
            return Err(invalid());
        }
        Ok(Self { host, port })
    }
}
struct Root {
    path: PathBuf,
    alias: PathBuf,
    file: File,
    directory: bool,
}
pub struct PermissionSet {
    read: Vec<Root>,
    write: Vec<Root>,
    env: Vec<String>,
    net: Vec<NetGrant>,
    reserved: Vec<PathBuf>,
}
impl PermissionSet {
    pub fn new(grants: &Permissions) -> Result<Self, RuntimeError> {
        if [
            &grants.read,
            &grants.write,
            &grants.env,
            &grants.net,
            &grants.subprocess,
            &grants.native,
        ]
        .iter()
        .any(|list| {
            list.len() > 1024
                || list
                    .iter()
                    .any(|s| s.is_empty() || s.len() > 32768 || s.contains('\0'))
        }) {
            return Err(RuntimeError::new(
                Status::InvalidArgument,
                "invalid permission grant",
            ));
        }
        Ok(Self {
            read: Self::roots(&grants.read, false)?,
            write: Self::roots(&grants.write, true)?,
            env: grants.env.clone(),
            net: grants
                .net
                .iter()
                .map(|grant| NetGrant::parse(grant))
                .collect::<Result<_, _>>()?,
            reserved: Vec::new(),
        })
    }
    /// Check the normalized exact host and effective port before opening a socket.
    /// A host-only grant explicitly allows every port of that host, never subdomains.
    pub fn net_url(&self, text: &str) -> Result<url::Url, RuntimeError> {
        validate_net_url_text(text)?;
        let url = url::Url::parse(text)
            .map_err(|_| RuntimeError::new(Status::InvalidArgument, "invalid HTTP URL"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(RuntimeError::new(
                Status::Unsupported,
                "fetchText supports only HTTP(S) URLs",
            ));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(RuntimeError::new(
                Status::InvalidArgument,
                "HTTP URL userinfo is unsupported",
            ));
        }
        let host = url
            .host()
            .ok_or_else(|| RuntimeError::new(Status::InvalidArgument, "HTTP URL requires a host"))?
            .to_owned();
        let port = url.port_or_known_default().ok_or_else(|| {
            RuntimeError::new(Status::InvalidArgument, "HTTP URL requires a port")
        })?;
        if !self
            .net
            .iter()
            .any(|grant| grant.host == host && grant.port.is_none_or(|p| p == port))
        {
            return Err(RuntimeError::new(
                Status::PermissionDenied,
                format!("network access denied: {host}:{port}"),
            ));
        }
        Ok(url)
    }
    pub fn reserve_cache(&mut self, path: Option<&Path>) {
        if let Some(path) = path {
            self.reserved.push(path.to_owned());
        }
    }
    fn roots(paths: &[String], write: bool) -> Result<Vec<Root>, RuntimeError> {
        paths
            .iter()
            .map(|path| {
                let alias = Self::absolute(Path::new(path))?;
                let path = Path::new(path).canonicalize()?;
                let directory = path.is_dir();
                let file = OpenOptions::new()
                    .read(true)
                    .write(write && !directory)
                    .open(&path)?;
                if !directory && !file.metadata()?.is_file() {
                    return Err(RuntimeError::new(
                        Status::PermissionDenied,
                        "permission roots must be regular files or directories",
                    ));
                }
                Ok(Root {
                    path,
                    alias,
                    file,
                    directory,
                })
            })
            .collect()
    }
    fn absolute(path: &Path) -> Result<PathBuf, RuntimeError> {
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                Component::ParentDir => {
                    normalized.pop();
                }
                Component::CurDir => {}
                _ => normalized.push(component.as_os_str()),
            }
        }
        Ok(normalized)
    }
    fn open(&self, path: &Path, write: bool) -> Result<File, RuntimeError> {
        let path = Self::absolute(path)?;
        for reserved in &self.reserved {
            let canonical = path
                .canonicalize()
                .ok()
                .or_else(|| path.parent().and_then(|p| p.canonicalize().ok()));
            if path.starts_with(reserved)
                || canonical.as_ref().is_some_and(|p| p.starts_with(reserved))
            {
                return Err(RuntimeError::new(
                    Status::PermissionDenied,
                    "runtime cache is not accessible to guest code",
                ));
            }
        }
        let roots = if write { &self.write } else { &self.read };
        for root in roots {
            // Both names refer to the explicitly granted handle. Never resolve guest
            // child symlinks to decide containment. Prefer the explicit alias
            // when it is nested beneath the canonical root.
            if let Some(base) = [&root.alias, &root.path].into_iter().find(|base| {
                if root.directory {
                    path.starts_with(base)
                } else {
                    path == **base
                }
            }) {
                if !root.directory {
                    return Ok(root.file.try_clone()?);
                }
                let relative = path.strip_prefix(base).map_err(|_| {
                    RuntimeError::new(Status::PermissionDenied, "path escapes grant")
                })?;
                return open_beneath(root, relative, write);
            }
        }
        Err(RuntimeError::new(
            Status::PermissionDenied,
            format!(
                "{} access denied: {}",
                if write { "write" } else { "read" },
                path.display()
            ),
        ))
    }
    pub fn read_text(&self, path: &Path) -> Result<String, RuntimeError> {
        let file = self.open(path, false)?;
        if !file.metadata()?.is_file() {
            return Err(RuntimeError::new(
                Status::PermissionDenied,
                "only regular files are readable",
            ));
        }
        if file.metadata()?.len() > MAX_FILE_BYTES {
            return Err(RuntimeError::new(
                Status::ResourceLimit,
                "file exceeds 16 MiB",
            ));
        }
        let mut bytes = Vec::new();
        // Clone shares file position on Unix; positional reads keep file grants reusable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            let mut buffer = [0u8; 8192];
            loop {
                let count = file.read_at(&mut buffer, bytes.len() as u64)?;
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..count]);
                if bytes.len() as u64 > MAX_FILE_BYTES {
                    return Err(RuntimeError::new(
                        Status::ResourceLimit,
                        "file exceeds 16 MiB",
                    ));
                }
            }
        }
        #[cfg(not(unix))]
        {
            use std::io::{Read, Seek, SeekFrom};
            let mut file = file;
            file.seek(SeekFrom::Start(0))?;
            file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_FILE_BYTES {
                return Err(RuntimeError::new(
                    Status::ResourceLimit,
                    "file exceeds 16 MiB",
                ));
            }
        }
        String::from_utf8(bytes).map_err(|e| RuntimeError::new(Status::Io, e.to_string()))
    }
    pub fn write_text(&self, path: &Path, text: &str) -> Result<(), RuntimeError> {
        if text.len() as u64 > MAX_FILE_BYTES {
            return Err(RuntimeError::new(
                Status::ResourceLimit,
                "write exceeds 16 MiB",
            ));
        }
        let mut file = self.open(path, true)?;
        if !file.metadata()?.is_file() {
            return Err(RuntimeError::new(
                Status::PermissionDenied,
                "only regular files are writable",
            ));
        }
        file.set_len(0)?;
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(0))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }
    pub fn env(&self, name: &str) -> Result<Option<String>, RuntimeError> {
        if !self.env.iter().any(|grant| grant == name) {
            return Err(RuntimeError::new(
                Status::PermissionDenied,
                format!("environment access denied: {name}"),
            ));
        }
        Ok(std::env::var(name).ok())
    }
}

#[cfg(unix)]
fn open_beneath(root: &Root, relative: &Path, write: bool) -> Result<File, RuntimeError> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
    };
    let mut directory = root.file.try_clone()?;
    let parts = relative.components().collect::<Vec<_>>();
    if parts.is_empty() {
        return Err(RuntimeError::new(
            Status::PermissionDenied,
            "expected a file path",
        ));
    }
    for (index, part) in parts.iter().enumerate() {
        let Component::Normal(name) = part else {
            return Err(RuntimeError::new(
                Status::PermissionDenied,
                "invalid path component",
            ));
        };
        let name = CString::new(name.as_bytes())
            .map_err(|_| RuntimeError::new(Status::InvalidArgument, "NUL in path"))?;
        let last = index + 1 == parts.len();
        let flags = libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if last {
                if write {
                    libc::O_WRONLY | libc::O_CREAT
                } else {
                    libc::O_RDONLY
                }
            } else {
                libc::O_RDONLY | libc::O_DIRECTORY
            };
        // The directory descriptor anchors traversal even if paths are concurrently renamed.
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, 0o600) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            return Err(RuntimeError::new(
                if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) {
                    Status::PermissionDenied
                } else {
                    Status::Io
                },
                error.to_string(),
            ));
        }
        let file = unsafe { File::from_raw_fd(fd) };
        if last {
            return Ok(file);
        }
        directory = file;
    }
    Err(RuntimeError::new(Status::PermissionDenied, "invalid path"))
}

#[cfg(not(unix))]
fn open_beneath(root: &Root, relative: &Path, write: bool) -> Result<File, RuntimeError> {
    let path = root.path.join(relative);
    let checked = if path.exists() {
        path.canonicalize()?
    } else {
        path.parent()
            .ok_or_else(|| RuntimeError::new(Status::PermissionDenied, "missing parent"))?
            .canonicalize()?
            .join(path.file_name().unwrap())
    };
    if !checked.starts_with(&root.path) {
        return Err(RuntimeError::new(
            Status::PermissionDenied,
            "path escapes grant",
        ));
    }
    // Canonical containment is weaker than handle traversal: capabilities reports this distinction.
    Ok(OpenOptions::new()
        .read(!write)
        .write(write)
        .create(write)
        .open(checked)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn network_grants_are_exact_and_port_scoped() {
        let permissions = PermissionSet::new(&Permissions {
            net: vec![
                "example.com:443".into(),
                "[::1]:8080".into(),
                "localhost".into(),
            ],
            ..Default::default()
        })
        .unwrap();
        assert!(permissions.net_url("https://EXAMPLE.com/path").is_ok());
        assert!(permissions.net_url("http://example.com/path").is_err());
        assert!(
            permissions
                .net_url("https://child.example.com/path")
                .is_err()
        );
        assert!(
            permissions
                .net_url("https://example.com.evil/path")
                .is_err()
        );
        assert!(permissions.net_url("http://[::1]:8080/path").is_ok());
        assert!(permissions.net_url("http://[::1]:8081/path").is_err());
        assert!(permissions.net_url("http://localhost:19001/path").is_ok());
        for url in [
            "http://user@example.com",
            "http://@example.com",
            "http://example.com/\0",
            "http://example.com/\n",
        ] {
            assert!(permissions.net_url(url).is_err(), "{url:?}");
        }
        assert_eq!(
            permissions
                .net_url("file:///etc/passwd")
                .unwrap_err()
                .status,
            Status::Unsupported
        );
        for grant in [
            "*",
            "*.example.com",
            "example.com/",
            "example.com:0",
            "example.com:65536",
            "::1",
            "example.com:abc",
            "user@example.com",
            "example.com\0",
        ] {
            assert!(
                PermissionSet::new(&Permissions {
                    net: vec![grant.into()],
                    ..Default::default()
                })
                .is_err(),
                "{grant:?}"
            );
        }
    }
    fn fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "ogma-permission-{}-{}",
            std::process::id(),
            crate::cache::unique_id()
        ));
        std::fs::create_dir(&root).unwrap();
        root
    }
    #[test]
    fn deny_by_default_and_component_boundary() {
        let root = fixture();
        let file = root.join("a.ts");
        std::fs::write(&file, "hello").unwrap();
        let denied = PermissionSet::new(&Permissions::default()).unwrap();
        assert_eq!(
            denied.read_text(&file).unwrap_err().status,
            Status::PermissionDenied
        );
        let allowed = PermissionSet::new(&Permissions {
            read: vec![root.to_string_lossy().into()],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(allowed.read_text(&file).unwrap(), "hello");
        assert_eq!(
            allowed
                .read_text(&root.with_extension("other").join("a.ts"))
                .unwrap_err()
                .status,
            Status::PermissionDenied
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn single_file_handle_can_be_read_repeatedly() {
        let root = fixture();
        let file = root.join("a");
        std::fs::write(&file, "hello").unwrap();
        let allowed = PermissionSet::new(&Permissions {
            read: vec![file.to_string_lossy().into()],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(allowed.read_text(&file).unwrap(), "hello");
        assert_eq!(allowed.read_text(&file).unwrap(), "hello");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn explicit_symlink_root_alias_uses_the_granted_directory_handle() {
        let root = fixture();
        let real = root.join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = real.join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        std::fs::write(real.join("value"), "granted").unwrap();
        let grants = PermissionSet::new(&Permissions {
            read: vec![alias.to_string_lossy().into()],
            write: vec![alias.to_string_lossy().into()],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(grants.read_text(&alias.join("value")).unwrap(), "granted");
        grants.write_text(&alias.join("value"), "changed").unwrap();
        assert_eq!(
            std::fs::read_to_string(real.join("value")).unwrap(),
            "changed"
        );
        std::os::unix::fs::symlink(root.join("outside"), real.join("escape")).unwrap();
        assert_eq!(
            grants.read_text(&alias.join("escape")).unwrap_err().status,
            Status::PermissionDenied
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn directory_grant_stays_anchored_when_its_path_is_replaced() {
        let root = fixture();
        let granted = root.join("grant");
        std::fs::create_dir(&granted).unwrap();
        std::fs::write(granted.join("value"), "original").unwrap();
        let grants = PermissionSet::new(&Permissions {
            read: vec![granted.to_string_lossy().into()],
            ..Default::default()
        })
        .unwrap();
        std::fs::rename(&granted, root.join("moved")).unwrap();
        std::fs::create_dir(&granted).unwrap();
        std::fs::write(granted.join("value"), "replacement").unwrap();
        assert_eq!(
            grants.read_text(&granted.join("value")).unwrap(),
            "original"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_denied_for_read_and_write() {
        let root = fixture();
        let outside = fixture();
        std::fs::write(outside.join("secret"), "secret").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let allowed = PermissionSet::new(&Permissions {
            read: vec![root.to_string_lossy().into()],
            write: vec![root.to_string_lossy().into()],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            allowed
                .read_text(&root.join("link/secret"))
                .unwrap_err()
                .status,
            Status::PermissionDenied
        );
        assert_eq!(
            allowed
                .write_text(&root.join("link/secret"), "bad")
                .unwrap_err()
                .status,
            Status::PermissionDenied
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("secret")).unwrap(),
            "secret"
        );
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }
}
