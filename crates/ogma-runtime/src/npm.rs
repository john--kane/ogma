//! Deliberately small, pure-ESM npm subset. Installation never executes package code.
use crate::{
    error::RuntimeError,
    generated::{OutputEvent, Request, Status},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::read::GzDecoder;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha512};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::Duration,
};

const MAX_DOWNLOAD: u64 = 64 * 1024 * 1024;
const MAX_EXTRACTED: u64 = 64 * 1024 * 1024;
const MAX_PACKAGES: usize = 512;
const MAX_GRAPH_BYTES: u64 = 512 * 1024 * 1024;
const LOCK: &str = "ogma.lock.json";
const JOURNAL: &str = ".ogma-install-journal.json";

type Dependencies = BTreeMap<String, String>;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LockedPackage {
    name: String,
    version: String,
    integrity: String,
    tarball: String,
    dependencies: Dependencies,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Lock {
    lockfile_version: u32,
    dependencies: Dependencies,
    packages: BTreeMap<String, LockedPackage>,
}
fn bad(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(Status::InvalidArgument, message)
}
fn unsupported(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(Status::Unsupported, message)
}
fn json(bytes: &[u8]) -> Result<Value, RuntimeError> {
    serde_json::from_slice(bytes).map_err(|e| bad(format!("Invalid package JSON: {e}")))
}
fn read_json(path: &Path) -> Result<Value, RuntimeError> {
    let bytes = bounded(fs::File::open(path)?, 1024 * 1024)?;
    json(&bytes)
}
fn bounded(mut reader: impl Read, limit: u64) -> Result<Vec<u8>, RuntimeError> {
    let mut bytes = Vec::new();
    reader.by_ref().take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(RuntimeError::new(
            Status::ResourceLimit,
            "Package input exceeds size limit",
        ));
    }
    Ok(bytes)
}
fn package_name(name: &str) -> Result<(), RuntimeError> {
    let parts: Vec<_> = name.split('/').collect();
    let valid = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    };
    if (parts.len() == 1 && valid(parts[0]))
        || (parts.len() == 2
            && parts[0].starts_with('@')
            && valid(&parts[0][1..])
            && valid(parts[1]))
    {
        Ok(())
    } else {
        Err(unsupported(format!("Unsupported npm package name: {name}")))
    }
}
fn requirement(range: &str) -> Result<VersionReq, RuntimeError> {
    let (prefix, text) = if let Some(s) = range.strip_prefix('^') {
        ("^", s)
    } else if let Some(s) = range.strip_prefix('~') {
        ("~", s)
    } else {
        ("=", range)
    };
    Version::parse(text).map_err(|_| {
        unsupported(format!(
            "Only exact, caret and tilde full versions are supported: {range}"
        ))
    })?;
    VersionReq::parse(&format!("{prefix}{text}")).map_err(|e| unsupported(e.to_string()))
}
fn dependencies(manifest: &Value) -> Result<Dependencies, RuntimeError> {
    for field in [
        "optionalDependencies",
        "peerDependencies",
        "bundledDependencies",
        "bundleDependencies",
        "workspaces",
    ] {
        if let Some(value) = manifest.get(field)
            && !(value.is_null()
                || value.as_object().is_some_and(|m| m.is_empty())
                || value.as_array().is_some_and(|a| a.is_empty()))
        {
            return Err(unsupported(format!(
                "{field} is outside the pure-JS npm subset"
            )));
        }
    }
    let Some(value) = manifest.get("dependencies") else {
        return Ok(BTreeMap::new());
    };
    let object = value
        .as_object()
        .ok_or_else(|| bad("dependencies must be an object"))?;
    let mut deps = BTreeMap::new();
    for (name, range) in object {
        package_name(name)?;
        let range = range
            .as_str()
            .ok_or_else(|| bad("Dependency range must be a string"))?;
        requirement(range)?;
        deps.insert(name.clone(), range.to_owned());
    }
    Ok(deps)
}
fn validate_manifest(manifest: &Value, package: &LockedPackage) -> Result<(), RuntimeError> {
    if manifest.get("name").and_then(Value::as_str) != Some(&package.name)
        || manifest.get("version").and_then(Value::as_str) != Some(&package.version)
    {
        return Err(bad("Tarball name/version does not match registry and lock"));
    }
    if manifest
        .get("scripts")
        .is_some_and(|v| !v.is_null() && !v.as_object().is_some_and(|o| o.is_empty()))
    {
        return Err(unsupported(
            "Package scripts are unsupported; no lifecycle scripts are executed",
        ));
    }
    if manifest.get("gypfile").and_then(Value::as_bool) == Some(true) {
        return Err(unsupported("Native addons are unsupported"));
    }
    if manifest.get("type").and_then(Value::as_str) != Some("module")
        && !manifest
            .get("main")
            .and_then(Value::as_str)
            .is_some_and(|s| s.ends_with(".mjs"))
    {
        return Err(unsupported(
            "CommonJS packages are unsupported; require type=module or an .mjs main",
        ));
    }
    if dependencies(manifest)? != package.dependencies {
        return Err(bad("Tarball dependencies do not match registry and lock"));
    }
    Ok(())
}
fn digest(integrity: &str) -> Result<Vec<u8>, RuntimeError> {
    let value = integrity
        .strip_prefix("sha512-")
        .ok_or_else(|| unsupported("Package integrity must be a single SHA-512 SRI digest"))?;
    let bytes = STANDARD
        .decode(value)
        .map_err(|_| bad("Malformed SHA-512 integrity"))?;
    if bytes.len() != 64 {
        return Err(bad("Malformed SHA-512 integrity"));
    }
    Ok(bytes)
}
fn hash_key(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn verify(bytes: &[u8], expected: &[u8]) -> Result<(), RuntimeError> {
    if Sha512::digest(bytes).as_slice() != expected {
        return Err(bad("Package tarball SHA-512 integrity mismatch"));
    }
    Ok(())
}
fn remote(url: &str) -> Result<reqwest::Url, RuntimeError> {
    let url = reqwest::Url::parse(url).map_err(|_| bad("Invalid registry/tarball URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(unsupported(
            "Registry/tarball must use HTTP(S) without URL credentials",
        ));
    }
    Ok(url)
}
fn fetch(
    client: &reqwest::blocking::Client,
    url: &str,
    limit: u64,
) -> Result<Vec<u8>, RuntimeError> {
    let response = client
        .get(remote(url)?)
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| RuntimeError::new(Status::Io, format!("Registry request failed: {e}")))?;
    bounded(response, limit)
}
fn blob(
    package: &LockedPackage,
    store: &Path,
    client: &reqwest::blocking::Client,
    locked: bool,
) -> Result<Vec<u8>, RuntimeError> {
    let expected = digest(&package.integrity)?;
    let target = store.join(format!("{}.tgz", hash_key(&expected)));
    if target.exists() {
        if fs::symlink_metadata(&target)?.file_type().is_symlink() {
            return Err(bad("Package store entry must not be a symlink"));
        }
        let bytes = bounded(fs::File::open(&target)?, MAX_DOWNLOAD)?;
        verify(&bytes, &expected)?;
        return Ok(bytes);
    }
    if locked {
        return Err(bad(
            "Locked offline install needs a verified tarball in the package store",
        ));
    }
    let bytes = fetch(client, &package.tarball, MAX_DOWNLOAD)?;
    verify(&bytes, &expected)?;
    let temp = store.join(format!(
        ".{}-{}.tmp",
        hash_key(&expected),
        std::process::id()
    ));
    let result = (|| {
        let mut f = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        fs::rename(&temp, &target)?;
        sync_directory(store)?;
        Ok::<_, RuntimeError>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result?;
    Ok(bytes)
}
fn extract(bytes: &[u8], destination: &Path, package: &LockedPackage) -> Result<u64, RuntimeError> {
    fs::create_dir_all(destination)?;
    // Bound the entire decompressed stream, including GNU/PAX extension records
    // consumed internally by tar before an entry is returned to this loop.
    let decoder = GzDecoder::new(bytes).take(MAX_EXTRACTED + 16 * 1024 * 1024);
    let mut archive = tar::Archive::new(decoder);
    let mut total = 0u64;
    let mut count = 0usize;
    let mut seen = BTreeSet::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        count += 1;
        if count > 20_000 {
            return Err(RuntimeError::new(
                Status::ResourceLimit,
                "Too many tarball entries",
            ));
        }
        let path = entry.path()?.into_owned();
        let components: Vec<_> = path.components().collect();
        if components.first() != Some(&Component::Normal(std::ffi::OsStr::new("package")))
            || components
                .iter()
                .any(|c| !matches!(c, Component::Normal(_)))
            || path.to_string_lossy().contains('\\')
        {
            return Err(bad("Unsafe tarball path; expected package/relative path"));
        }
        let relative: PathBuf = components.iter().skip(1).collect();
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            return Err(unsupported(
                "Tarball links and special files are unsupported",
            ));
        }
        if relative.as_os_str().is_empty() {
            if kind.is_dir() {
                continue;
            }
            return Err(bad("Invalid tarball root"));
        }
        if !seen.insert(relative.clone()) {
            return Err(bad("Duplicate tarball path"));
        }
        if relative
            .components()
            .any(|c| c == Component::Normal(std::ffi::OsStr::new("node_modules")))
        {
            return Err(unsupported("Bundled node_modules is unsupported"));
        }
        let ext = relative.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ["node", "so", "dll", "dylib", "a", "o", "cjs", "gyp"].contains(&ext)
            || relative.file_name().is_some_and(|n| n == "binding.gyp")
        {
            return Err(unsupported(
                "Native addons and CommonJS files are unsupported",
            ));
        }
        let target = destination.join(&relative);
        if kind.is_dir() {
            fs::create_dir_all(target)?;
            continue;
        }
        total = total
            .checked_add(entry.size())
            .ok_or_else(|| bad("Tarball size overflow"))?;
        if total > MAX_EXTRACTED {
            return Err(RuntimeError::new(
                Status::ResourceLimit,
                "Expanded tarball exceeds size limit",
            ));
        }
        fs::create_dir_all(target.parent().unwrap())?;
        let mut output = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(target)?;
        std::io::copy(&mut entry, &mut output)?;
        output.sync_all()?;
    }
    validate_manifest(&read_json(&destination.join("package.json"))?, package)?;
    Ok(total)
}
struct Installer<'a> {
    client: reqwest::blocking::Client,
    registry: String,
    store: &'a Path,
    stage: &'a Path,
    packages: BTreeMap<String, LockedPackage>,
    extracted_bytes: u64,
}
impl Installer<'_> {
    fn add(
        &mut self,
        name: &str,
        range: &str,
        path: &str,
        ancestry: &mut Vec<(String, String)>,
    ) -> Result<(), RuntimeError> {
        if self.packages.len() >= MAX_PACKAGES || ancestry.len() >= 32 {
            return Err(RuntimeError::new(
                Status::ResourceLimit,
                "Dependency graph exceeds supported size/depth",
            ));
        }
        let req = requirement(range)?;
        let encoded = name.replace('/', "%2f");
        let metadata = json(&fetch(
            &self.client,
            &format!("{}/{encoded}", self.registry.trim_end_matches('/')),
            8 * 1024 * 1024,
        )?)?;
        let versions = metadata
            .get("versions")
            .and_then(Value::as_object)
            .ok_or_else(|| bad("Registry has no versions object"))?;
        let selected = versions
            .iter()
            .filter_map(|(v, info)| {
                Version::parse(v)
                    .ok()
                    .filter(|v| req.matches(v))
                    .map(|v| (v, info))
            })
            .max_by(|a, b| a.0.cmp(&b.0))
            .ok_or_else(|| bad(format!("No version of {name} matches {range}")))?;
        let version = selected.0.to_string();
        let info = selected.1;
        if ancestry.contains(&(name.to_owned(), version.clone())) {
            return Err(unsupported(
                "Cyclic npm dependencies are outside this preview subset",
            ));
        }
        let dist = info
            .get("dist")
            .ok_or_else(|| bad("Registry package has no dist metadata"))?;
        let package = LockedPackage {
            name: name.into(),
            version: version.clone(),
            integrity: dist
                .get("integrity")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("Registry package needs SHA-512 integrity"))?
                .into(),
            tarball: dist
                .get("tarball")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("Registry package needs a tarball URL"))?
                .into(),
            dependencies: dependencies(info)?,
        };
        drop(metadata);
        let bytes = blob(&package, self.store, &self.client, false)?;
        self.extracted_bytes += extract(&bytes, &self.stage.join(path), &package)?;
        drop(bytes);
        if self.extracted_bytes > MAX_GRAPH_BYTES {
            return Err(RuntimeError::new(
                Status::ResourceLimit,
                "Expanded dependency graph exceeds size limit",
            ));
        }
        self.packages.insert(path.into(), package.clone());
        ancestry.push((name.into(), version));
        for (dep, range) in &package.dependencies {
            self.add(dep, range, &format!("{path}/node_modules/{dep}"), ancestry)?;
        }
        ancestry.pop();
        Ok(())
    }
}
fn validate_lock(lock: &Lock, deps: &Dependencies) -> Result<(), RuntimeError> {
    if lock.lockfile_version != 1 || &lock.dependencies != deps {
        return Err(bad("Lockfile does not match package.json dependencies"));
    }
    if lock.packages.len() > MAX_PACKAGES {
        return Err(bad("Lockfile dependency graph is too large"));
    }
    let mut expected = BTreeMap::new();
    for (name, range) in deps {
        expected.insert(name.clone(), (name.clone(), range.clone()));
    }
    for (path, package) in &lock.packages {
        for (name, range) in &package.dependencies {
            package_name(name)?;
            expected.insert(
                format!("{path}/node_modules/{name}"),
                (name.clone(), range.clone()),
            );
        }
    }
    if expected.len() != lock.packages.len() {
        return Err(bad("Lockfile contains missing or extraneous packages"));
    }
    for (path, (name, range)) in expected {
        let p = lock
            .packages
            .get(&path)
            .ok_or_else(|| bad("Lockfile contains missing package"))?;
        if path
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
            || Path::new(&path).is_absolute()
            || path.contains('\\')
            || p.name != name
            || !requirement(&range)?
                .matches(&Version::parse(&p.version).map_err(|_| bad("Invalid locked version"))?)
        {
            return Err(bad("Invalid lockfile package path/version"));
        }
        digest(&p.integrity)?;
        remote(&p.tarball)?;
    }
    // Every node must be reachable from the root rather than a disconnected cycle.
    let mut reachable: BTreeSet<String> = deps.keys().cloned().collect();
    let mut queue: Vec<String> = reachable.iter().cloned().collect();
    while let Some(path) = queue.pop() {
        let package = lock
            .packages
            .get(&path)
            .ok_or_else(|| bad("Missing locked dependency"))?;
        for name in package.dependencies.keys() {
            let child = format!("{path}/node_modules/{name}");
            if reachable.insert(child.clone()) {
                queue.push(child);
            }
        }
    }
    if reachable.len() != lock.packages.len() {
        return Err(bad("Disconnected lockfile dependency graph"));
    }
    Ok(())
}
struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[cfg(unix)]
struct InstallGuard(fs::File);
#[cfg(unix)]
impl Drop for InstallGuard {
    fn drop(&mut self) {
        unsafe {
            libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.0), libc::LOCK_UN);
        }
    }
}
#[cfg(unix)]
fn guard(project: &Path) -> Result<InstallGuard, RuntimeError> {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let f = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(project.join(".ogma-install.lock"))?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(bad(
            "Another npm install is already running in this project",
        ));
    }
    Ok(InstallGuard(f))
}
/// Exchange complete directory trees without exposing a missing/partial node_modules.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn exchange(left: &Path, right: &Path) -> Result<(), RuntimeError> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let left =
        CString::new(left.as_os_str().as_bytes()).map_err(|_| bad("Invalid installation path"))?;
    let right =
        CString::new(right.as_os_str().as_bytes()).map_err(|_| bad("Invalid installation path"))?;
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            left.as_ptr(),
            libc::AT_FDCWD,
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            left.as_ptr(),
            libc::AT_FDCWD,
            right.as_ptr(),
            libc::RENAME_SWAP,
        )
    };
    if result != 0 {
        return Err(RuntimeError::new(
            Status::Io,
            format!(
                "Filesystem cannot atomically exchange node_modules: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    Ok(())
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn exchange(_: &Path, _: &Path) -> Result<(), RuntimeError> {
    Err(unsupported(
        "Atomic package publication requires Linux or macOS",
    ))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicationJournal {
    version: u32,
    stage: String,
    locked: bool,
    had_modules: bool,
    previous_lock_present: bool,
    previous_lock_sha512: Option<String>,
    new_lock_sha512: String,
}
fn interrupted(project: &Path) -> RuntimeError {
    RuntimeError::new(
        Status::Io,
        format!(
            "Interrupted npm installation: {} exists. Preserve the staging tree named in this journal and its previous-ogma.lock.json backup; restore a matching node_modules/ogma.lock.json pair, then remove the journal and retry --locked. No installation is replayed automatically.",
            project.join(JOURNAL).display()
        ),
    )
}
fn sync_directory(path: &Path) -> Result<(), RuntimeError> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}
fn sync_tree_directories(path: &Path) -> Result<(), RuntimeError> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_tree_directories(&entry.path())?;
        }
    }
    sync_directory(path)
}
fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), RuntimeError> {
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn clear_journal(project: &Path, stage_root: &Path) -> Result<(), RuntimeError> {
    // First persist both publication destinations (or the completed rollback).
    // Only then remove the durable indication that publication may be incomplete.
    sync_directory(stage_root)?;
    sync_directory(project)?;
    fs::remove_file(project.join(JOURNAL))?;
    sync_directory(project)
}

/// Install is an explicit tool operation, authorized by its command rather than guest grants.
pub fn install(request: &Request) -> Result<Vec<OutputEvent>, RuntimeError> {
    let project = fs::canonicalize(&request.entry)?;
    if !project.is_dir() {
        return Err(bad("Install entry must be a project directory"));
    }
    #[cfg(unix)]
    let _guard = guard(&project)?;
    if fs::symlink_metadata(project.join(JOURNAL)).is_ok() {
        return Err(interrupted(&project));
    }
    let deps = dependencies(&read_json(&project.join("package.json"))?)?;
    let store = if request.tool.store.is_empty() {
        project.join(".ogma/store")
    } else {
        PathBuf::from(&request.tool.store)
    };
    fs::create_dir_all(&store)?;
    if fs::symlink_metadata(&store)?.file_type().is_symlink() {
        return Err(bad("Package store must not be a symlink"));
    }
    let stage_root = project.join(format!(".ogma-install-{}", std::process::id()));
    fs::create_dir(&stage_root)?;
    let _cleanup = Cleanup(stage_root.clone());
    let stage = stage_root.join("node_modules");
    fs::create_dir(&stage)?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| bad(e.to_string()))?;
    let lock_path = project.join(LOCK);
    let lock = if request.tool.locked {
        let lock: Lock =
            serde_json::from_slice(&bounded(fs::File::open(&lock_path)?, 8 * 1024 * 1024)?)
                .map_err(|e| bad(format!("Invalid lockfile: {e}")))?;
        validate_lock(&lock, &deps)?;
        let mut extracted_bytes = 0;
        for (path, package) in &lock.packages {
            let bytes = blob(package, &store, &client, true)?;
            extracted_bytes += extract(&bytes, &stage.join(path), package)?;
            if extracted_bytes > MAX_GRAPH_BYTES {
                return Err(RuntimeError::new(
                    Status::ResourceLimit,
                    "Expanded dependency graph exceeds size limit",
                ));
            }
        }
        lock
    } else {
        let registry = if request.tool.registry.is_empty() {
            "https://registry.npmjs.org".into()
        } else {
            request.tool.registry.clone()
        };
        remote(&registry)?;
        let mut installer = Installer {
            client,
            registry,
            store: &store,
            stage: &stage,
            packages: BTreeMap::new(),
            extracted_bytes: 0,
        };
        for (name, range) in &deps {
            installer.add(name, range, name, &mut Vec::new())?;
        }
        Lock {
            lockfile_version: 1,
            dependencies: deps,
            packages: installer.packages,
        }
    };
    let mut encoded = serde_json::to_vec_pretty(&lock).map_err(|e| bad(e.to_string()))?;
    encoded.push(b'\n');
    if encoded.len() > 8 * 1024 * 1024 {
        return Err(RuntimeError::new(
            Status::ResourceLimit,
            "Dependency lockfile exceeds size limit",
        ));
    }
    let new_lock = stage_root.join(LOCK);
    write_synced(&new_lock, &encoded)?;
    let modules = project.join("node_modules");
    let has_modules = fs::symlink_metadata(&modules).is_ok();
    if has_modules && !fs::symlink_metadata(&modules)?.is_dir() {
        return Err(bad(
            "Existing node_modules must be a directory, not a symlink",
        ));
    }
    let previous_lock_present = lock_path.is_file();
    let mut previous_lock_sha512 = None;
    if previous_lock_present {
        let previous = bounded(fs::File::open(&lock_path)?, 8 * 1024 * 1024)?;
        previous_lock_sha512 = Some(hash_key(&Sha512::digest(&previous)));
        write_synced(&stage_root.join("previous-ogma.lock.json"), &previous)?;
    }
    let record = PublicationJournal {
        version: 1,
        stage: stage_root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        locked: request.tool.locked,
        had_modules: has_modules,
        previous_lock_present,
        new_lock_sha512: if request.tool.locked {
            previous_lock_sha512.clone().unwrap()
        } else {
            hash_key(&Sha512::digest(&encoded))
        },
        previous_lock_sha512,
    };
    sync_tree_directories(&stage_root)?;
    let journal = project.join(JOURNAL);
    let prepared = (|| {
        write_synced(
            &journal,
            &serde_json::to_vec(&record).map_err(|e| bad(e.to_string()))?,
        )?;
        sync_directory(&project)
    })();
    if let Err(error) = prepared {
        if fs::symlink_metadata(&journal).is_ok() {
            std::mem::forget(_cleanup);
        }
        return Err(error);
    }
    let published = if has_modules {
        exchange(&stage, &modules)
    } else {
        fs::rename(&stage, &modules).map_err(RuntimeError::from)
    };
    if let Err(error) = published {
        if let Err(clear_error) = clear_journal(&project, &stage_root) {
            std::mem::forget(_cleanup);
            return Err(RuntimeError::new(
                Status::Io,
                format!("Publication failed: {error}; journal recovery required: {clear_error}"),
            ));
        }
        return Err(error);
    }
    if !request.tool.locked
        && let Err(error) = fs::rename(&new_lock, &lock_path)
    {
        let rollback = if has_modules {
            exchange(&stage, &modules)
        } else {
            fs::remove_dir_all(&modules).map_err(RuntimeError::from)
        };
        if let Err(rollback_error) = rollback {
            // Preserve the old directory in staging if filesystem failure prevents recovery.
            std::mem::forget(_cleanup);
            return Err(RuntimeError::new(
                Status::Io,
                format!(
                    "Lock publication failed: {error}; rollback failed: {rollback_error}; recovery tree: {}",
                    stage_root.display()
                ),
            ));
        }
        if let Err(clear_error) = clear_journal(&project, &stage_root) {
            std::mem::forget(_cleanup);
            return Err(RuntimeError::new(
                Status::Io,
                format!(
                    "Lock publication failed: {error}; journal recovery required: {clear_error}"
                ),
            ));
        }
        return Err(error.into());
    }
    if let Err(error) = clear_journal(&project, &stage_root) {
        std::mem::forget(_cleanup);
        return Err(error);
    }
    Ok(vec![OutputEvent {
        stream: "stdout".into(),
        text: format!(
            "Installed {} pure-ESM packages{}",
            lock.packages.len(),
            if request.tool.locked {
                " from verified offline store"
            } else {
                ""
            }
        ),
    }])
}

fn export_target(value: &Value) -> Result<String, RuntimeError> {
    if let Some(s) = value.as_str() {
        if !s.starts_with("./") || s.split('/').any(|part| part == "node_modules") {
            return Err(bad(
                "Package exports must be relative ./ paths without node_modules",
            ));
        }
        return Ok(s.into());
    }
    if let Some(object) = value.as_object() {
        for condition in ["import", "default"] {
            if let Some(v) = object.get(condition) {
                return export_target(v);
            }
        }
    }
    Err(unsupported(
        "Only string exports and import/default conditional exports are supported",
    ))
}
/// Resolve nearest installed ESM package. Actual source reads remain loader permission checked.
pub fn resolve(
    permissions: &crate::permissions::PermissionSet,
    specifier: &str,
    referrer: &str,
) -> Result<Option<String>, RuntimeError> {
    if specifier.starts_with('.') || specifier.starts_with('/') || specifier.starts_with("file:") {
        return Ok(None);
    }
    if specifier.starts_with("node:") || specifier.contains(':') || specifier.starts_with('#') {
        return Err(unsupported(format!(
            "Unsupported module specifier: {specifier}"
        )));
    }
    let parts: Vec<_> = specifier.split('/').collect();
    let n = if specifier.starts_with('@') { 2 } else { 1 };
    if parts.len() < n {
        return Err(bad("Invalid scoped package import"));
    }
    let name = parts[..n].join("/");
    package_name(&name)?;
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == "." || *p == ".." || p.contains('\\'))
    {
        return Err(bad("Invalid package subpath"));
    }
    let subpath = parts[n..].join("/");
    let referrer = if referrer.starts_with("file:") {
        url::Url::parse(referrer)
            .ok()
            .and_then(|u| u.to_file_path().ok())
            .ok_or_else(|| bad("Invalid module referrer"))?
    } else {
        PathBuf::from(referrer)
    };
    for parent in referrer.ancestors().skip(1) {
        if fs::symlink_metadata(parent.join(JOURNAL)).is_ok() {
            return Err(interrupted(parent));
        }
    }
    let mut directory = referrer.parent();
    while let Some(parent) = directory {
        let package = parent.join("node_modules").join(&name);
        if package.join("package.json").exists() {
            let manifest = json(
                permissions
                    .read_text(&package.join("package.json"))?
                    .as_bytes(),
            )?;
            let target =
                if let Some(exports) = manifest.get("exports") {
                    let key = if subpath.is_empty() {
                        ".".to_owned()
                    } else {
                        format!("./{subpath}")
                    };
                    if let Some(object) = exports
                        .as_object()
                        .filter(|o| o.keys().any(|k| k.starts_with('.')))
                    {
                        export_target(object.get(&key).ok_or_else(|| {
                            unsupported(format!("Package does not export {key}"))
                        })?)?
                    } else if subpath.is_empty() {
                        export_target(exports)?
                    } else {
                        return Err(unsupported("Package subpath is not exported"));
                    }
                } else if !subpath.is_empty() {
                    format!("./{subpath}")
                } else {
                    manifest
                        .get("main")
                        .and_then(Value::as_str)
                        .unwrap_or("./index.js")
                        .to_owned()
                };
            let target_path = Path::new(&target);
            if target_path.is_absolute()
                || target.contains('\\')
                || target_path
                    .components()
                    .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
            {
                return Err(bad("Package entry escapes its package directory"));
            }
            let path = package.join(target_path);
            let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !matches!(extension, "js" | "mjs" | "ts" | "mts") {
                return Err(unsupported(
                    "Only explicit ESM JS/TS package entries are supported",
                ));
            }
            if extension == "js" && manifest.get("type").and_then(Value::as_str) != Some("module") {
                return Err(unsupported("CommonJS package entry is unsupported"));
            }
            if !path.is_file() {
                return Err(bad(format!(
                    "Package entry does not exist: {}",
                    path.display()
                )));
            }
            return Ok(Some(
                url::Url::from_file_path(&path)
                    .map_err(|_| bad("Invalid package entry path"))?
                    .to_string(),
            ));
        }
        directory = parent.parent();
    }
    Err(unsupported(format!(
        "Package is not installed: {name}; run ogma install"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{generated::Permissions, permissions::PermissionSet};

    #[test]
    fn supported_ranges_keep_npm_major_zero_and_tilde_rules() {
        assert!(
            requirement("^0.2.3")
                .unwrap()
                .matches(&Version::parse("0.2.9").unwrap())
        );
        assert!(
            !requirement("^0.2.3")
                .unwrap()
                .matches(&Version::parse("0.3.0").unwrap())
        );
        assert!(
            !requirement("~1.0.0")
                .unwrap()
                .matches(&Version::parse("1.1.0").unwrap())
        );
        assert!(
            !requirement("1.0.0")
                .unwrap()
                .matches(&Version::parse("1.0.1").unwrap())
        );
        for range in [
            "latest",
            "*",
            ">=1.0.0",
            "1.0",
            "file:../pkg",
            "https://example.com/pkg",
        ] {
            assert!(requirement(range).is_err(), "{range}");
        }
    }

    #[test]
    fn package_manifest_is_permission_checked_and_nested_exports_resolve() {
        let root = std::env::temp_dir().join(format!("ogma-npm-resolve-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let _cleanup = Cleanup(root.clone());
        let package = root.join("node_modules/@demo/value");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("package.json"),
            r#"{"type":"module","exports":{".":{"import":"./index.js"},"./other":"./index.js"}}"#,
        )
        .unwrap();
        fs::write(package.join("index.js"), "export const value = 1;").unwrap();
        let main = root.join("main.ts");
        fs::write(&main, "").unwrap();
        let denied = PermissionSet::new(&Permissions {
            read: vec![main.to_string_lossy().into_owned()],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            resolve(&denied, "@demo/value", main.to_str().unwrap())
                .unwrap_err()
                .status,
            Status::PermissionDenied
        );
        let allowed = PermissionSet::new(&Permissions {
            read: vec![root.to_string_lossy().into_owned()],
            ..Default::default()
        })
        .unwrap();
        let expected = url::Url::from_file_path(package.join("index.js"))
            .unwrap()
            .to_string();
        assert_eq!(
            resolve(&allowed, "@demo/value/other", main.to_str().unwrap()).unwrap(),
            Some(expected)
        );
        assert!(resolve(&allowed, "@demo/value/../value", main.to_str().unwrap()).is_err());
        assert!(resolve(&allowed, "node:fs", main.to_str().unwrap()).is_err());
    }
}
