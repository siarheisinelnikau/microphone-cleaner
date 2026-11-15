use crate::config::replace_file;
use reqwest::{
    Url,
    blocking::{Client, Response},
    redirect::Policy,
};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const REPOSITORY: &str = "siarheisinelnikau/microphone-cleaner";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_EXE: u64 = 128 * 1024 * 1024;
const MAX_JSON: u64 = 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    pub browser_download_url: String,
    pub digest: Option<String>,
}
#[derive(Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub draft: bool,
    pub prerelease: bool,
    pub assets: Vec<Asset>,
}
#[derive(Clone, Debug)]
pub struct Available {
    pub version: Version,
    pub executable: Asset,
    pub checksum: Option<Asset>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Network,
    NoRelease,
    InvalidRelease,
    Incompatible,
    Integrity,
    Storage,
    Cancelled,
    Install,
}
#[derive(Clone, Debug)]
pub struct UpdateError {
    pub kind: ErrorKind,
    pub detail: String,
}
impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.detail)
    }
}
impl std::error::Error for UpdateError {}
fn failure(kind: ErrorKind, detail: impl ToString) -> UpdateError {
    UpdateError {
        kind,
        detail: detail.to_string(),
    }
}

#[derive(Clone, Debug)]
pub enum Check {
    Current,
    Available(Available),
}

fn http_client() -> Result<Client, UpdateError> {
    Client::builder()
        .user_agent(concat!("MicrophoneCleaner/", env!("CARGO_PKG_VERSION")))
        .https_only(true)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(180))
        .redirect(Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.error("Too many redirects");
            }
            let host = attempt.url().host_str().unwrap_or("");
            if attempt.url().scheme() == "https"
                && [
                    "github.com",
                    "api.github.com",
                    "release-assets.githubusercontent.com",
                    "objects.githubusercontent.com",
                ]
                .contains(&host)
            {
                attempt.follow()
            } else {
                attempt.error("Untrusted update redirect")
            }
        }))
        .build()
        .map_err(|e| failure(ErrorKind::Network, e))
}

fn bounded_body(response: Response, limit: u64) -> Result<Vec<u8>, UpdateError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        return Err(failure(ErrorKind::InvalidRelease, "Response is too large"));
    }
    let mut bytes = Vec::new();
    response
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| failure(ErrorKind::Network, e))?;
    if bytes.len() as u64 > limit {
        return Err(failure(ErrorKind::InvalidRelease, "Response is too large"));
    }
    Ok(bytes)
}

pub fn check() -> Result<Check, UpdateError> {
    let response = http_client()?
        .get(format!(
            "https://api.github.com/repos/{REPOSITORY}/releases/latest"
        ))
        .header("Accept", "application/vnd.github+json")
        .send()
        .map_err(|e| failure(ErrorKind::Network, e))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(failure(
            ErrorKind::NoRelease,
            "Repository or stable release is unavailable (HTTP 404)",
        ));
    }
    let response = response
        .error_for_status()
        .map_err(|e| failure(ErrorKind::Network, e))?;
    let release: Release = serde_json::from_slice(&bounded_body(response, MAX_JSON)?)
        .map_err(|e| failure(ErrorKind::InvalidRelease, e))?;
    select_release(
        release,
        &Version::parse(VERSION).unwrap(),
        std::env::consts::ARCH,
    )
}

pub fn select_release(
    release: Release,
    installed: &Version,
    architecture: &str,
) -> Result<Check, UpdateError> {
    let version = Version::parse(
        release
            .tag_name
            .strip_prefix('v')
            .unwrap_or(&release.tag_name),
    )
    .map_err(|e| failure(ErrorKind::InvalidRelease, e))?;
    if release.draft || release.prerelease || !version.pre.is_empty() {
        return Err(failure(
            ErrorKind::InvalidRelease,
            "Stable channel received a draft or prerelease",
        ));
    }
    if version <= *installed {
        return Ok(Check::Current);
    }
    let suffix = match architecture {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "x86",
        _ => {
            return Err(failure(
                ErrorKind::Incompatible,
                "Unsupported processor architecture",
            ));
        }
    };
    let plain = format!("MicrophoneCleaner-v{version}.exe");
    let specific = format!("MicrophoneCleaner-v{version}-{suffix}.exe");
    // The project's unsuffixed portable distribution is Windows x64. Multiple architectures
    // require explicit suffixes; never install a same-name binary on an incompatible processor.
    let executable = release
        .assets
        .iter()
        .find(|a| a.name == specific)
        .or_else(|| {
            if architecture == "x86_64" {
                release.assets.iter().find(|a| a.name == plain)
            } else {
                None
            }
        })
        .ok_or_else(|| {
            failure(
                ErrorKind::Incompatible,
                "Release has no compatible portable executable",
            )
        })?
        .clone();
    validate_asset(&executable, &release.tag_name)?;
    if executable.size < 4096 || executable.size > MAX_EXE {
        return Err(failure(
            ErrorKind::InvalidRelease,
            "Invalid executable size",
        ));
    }
    let checksum = release
        .assets
        .iter()
        .find(|a| a.name == format!("{}.sha256", executable.name))
        .cloned();
    if let Some(asset) = &checksum {
        validate_asset(asset, &release.tag_name)?;
    }
    if checksum.is_none()
        && executable
            .digest
            .as_deref()
            .and_then(parse_digest)
            .is_none()
    {
        return Err(failure(
            ErrorKind::Integrity,
            "Release must supply a SHA-256 digest or matching .sha256 asset",
        ));
    }
    Ok(Check::Available(Available {
        version,
        executable,
        checksum,
    }))
}

fn validate_asset(asset: &Asset, tag: &str) -> Result<(), UpdateError> {
    let url = Url::parse(&asset.browser_download_url)
        .map_err(|e| failure(ErrorKind::InvalidRelease, e))?;
    let expected = format!("/{REPOSITORY}/releases/download/{tag}/{}", asset.name);
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || url.path() != expected
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(failure(
            ErrorKind::InvalidRelease,
            "Asset URL does not belong to the canonical release",
        ));
    }
    Ok(())
}

fn parse_digest(value: &str) -> Option<String> {
    let hex = value.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| hex.to_ascii_lowercase())
}

pub fn checksum_text(text: &str, filename: &str) -> Option<String> {
    let mut parts = text.split_whitespace();
    let digest = parts.next()?;
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    if let Some(name) = parts.next()
        && (name.trim_start_matches('*') != filename || parts.next().is_some())
    {
        return None;
    }
    Some(digest.to_ascii_lowercase())
}

#[derive(Default)]
pub struct Progress {
    pub bytes: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstallPlan {
    pub target: PathBuf,
    pub staged: PathBuf,
    pub helper: PathBuf,
    pub backup: PathBuf,
    pub acknowledgement: PathBuf,
    pub waiting: PathBuf,
    pub parent_pid: u32,
    pub sha256: String,
    pub nonce: String,
}

impl InstallPlan {
    pub fn path(&self) -> PathBuf {
        self.target
            .parent()
            .unwrap()
            .join(format!(".microphone-cleaner-{}.json", self.nonce))
    }

    pub fn validate(&self, helper: &Path) -> Result<(), UpdateError> {
        let parent = self
            .target
            .parent()
            .ok_or_else(|| failure(ErrorKind::Install, "Target has no parent"))?;
        let parent = fs::canonicalize(parent).map_err(|e| failure(ErrorKind::Storage, e))?;
        if self.nonce.len() > 64
            || self.nonce.is_empty()
            || !self
                .nonce
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b == b'-')
        {
            return Err(failure(ErrorKind::Install, "Invalid installation nonce"));
        }
        let stem = format!(".microphone-cleaner-{}", self.nonce);
        let paths = [
            (&self.staged, format!("{stem}.new")),
            (&self.helper, format!("{stem}.helper.exe")),
            (&self.backup, format!("{stem}.backup.exe")),
            (&self.acknowledgement, format!("{stem}.ready")),
            (&self.waiting, format!("{stem}.waiting")),
        ];
        for (path, name) in paths {
            if path.parent() != Some(parent.as_path())
                || path.file_name().and_then(|s| s.to_str()) != Some(name.as_str())
            {
                return Err(failure(
                    ErrorKind::Install,
                    "Installation paths must stay in the portable executable directory",
                ));
            }
        }
        if fs::canonicalize(helper).map_err(|e| failure(ErrorKind::Storage, e))? != self.helper
            || !self.target.is_absolute()
            || self.target.parent() != Some(parent.as_path())
            || self.target.extension().is_none_or(|ext| ext != "exe")
            || self.target == self.helper
            || self.target == self.backup
            || self.target == self.staged
        {
            return Err(failure(
                ErrorKind::Install,
                "Invalid portable installation target",
            ));
        }
        if self.sha256.len() != 64 || !self.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(failure(ErrorKind::Integrity, "Invalid installation digest"));
        }
        Ok(())
    }
}

pub fn stage(
    available: &Available,
    executable: &Path,
    progress: &Progress,
) -> Result<InstallPlan, UpdateError> {
    let target = fs::canonicalize(executable).map_err(|e| failure(ErrorKind::Storage, e))?;
    let parent = target
        .parent()
        .ok_or_else(|| failure(ErrorKind::Storage, "Executable directory is missing"))?;
    let nonce = format!(
        "{:x}-{:x}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let stem = format!(".microphone-cleaner-{nonce}");
    let plan = InstallPlan {
        target: target.clone(),
        staged: parent.join(format!("{stem}.new")),
        helper: parent.join(format!("{stem}.helper.exe")),
        backup: parent.join(format!("{stem}.backup.exe")),
        acknowledgement: parent.join(format!("{stem}.ready")),
        waiting: parent.join(format!("{stem}.waiting")),
        parent_pid: std::process::id(),
        sha256: String::new(),
        nonce,
    };
    let client = http_client()?;
    let digest = match available
        .executable
        .digest
        .as_deref()
        .and_then(parse_digest)
    {
        Some(digest) => digest,
        None => {
            let asset = available
                .checksum
                .as_ref()
                .ok_or_else(|| failure(ErrorKind::Integrity, "Missing SHA-256"))?;
            let response = client
                .get(&asset.browser_download_url)
                .send()
                .and_then(|r| r.error_for_status())
                .map_err(|e| failure(ErrorKind::Network, e))?;
            let bytes = bounded_body(response, 4096)?;
            checksum_text(
                std::str::from_utf8(&bytes).map_err(|e| failure(ErrorKind::Integrity, e))?,
                &available.executable.name,
            )
            .ok_or_else(|| failure(ErrorKind::Integrity, "Invalid SHA-256 file"))?
        }
    };
    let plan = InstallPlan {
        sha256: digest.clone(),
        ..plan
    };
    let outcome = (|| {
        let mut response = client
            .get(&available.executable.browser_download_url)
            .send()
            .and_then(|r| r.error_for_status())
            .map_err(|e| failure(ErrorKind::Network, e))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&plan.staged)
            .map_err(|e| failure(ErrorKind::Storage, e))?;
        progress
            .total
            .store(available.executable.size, Ordering::Relaxed);
        copy_verified(
            &mut response,
            &mut file,
            available.executable.size,
            &digest,
            progress,
        )?;
        file.sync_all()
            .map_err(|e| failure(ErrorKind::Storage, e))?;
        drop(file);
        verify_pe(&plan.staged, std::env::consts::ARCH)?;
        if progress.cancel.load(Ordering::Acquire) {
            return Err(failure(
                ErrorKind::Cancelled,
                "Update cancelled before installation",
            ));
        }
        // The helper is the same signed/build-identical executable, copied only after verification.
        let mut source = File::open(&plan.target).map_err(|e| failure(ErrorKind::Storage, e))?;
        let mut helper = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&plan.helper)
            .map_err(|e| failure(ErrorKind::Storage, e))?;
        std::io::copy(&mut source, &mut helper).map_err(|e| failure(ErrorKind::Storage, e))?;
        helper
            .sync_all()
            .map_err(|e| failure(ErrorKind::Storage, e))?;
        let bytes = serde_json::to_vec(&plan).map_err(|e| failure(ErrorKind::Storage, e))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(plan.path())
            .map_err(|e| failure(ErrorKind::Storage, e))?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| failure(ErrorKind::Storage, e))?;
        Ok(plan.clone())
    })();
    if outcome.is_err() {
        cleanup_staging(&plan);
    }
    outcome
}

pub fn copy_verified(
    reader: &mut impl Read,
    writer: &mut impl Write,
    size: u64,
    expected: &str,
    progress: &Progress,
) -> Result<(), UpdateError> {
    if !(4096..=MAX_EXE).contains(&size) {
        return Err(failure(ErrorKind::InvalidRelease, "Invalid download size"));
    }
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        if progress.cancel.load(Ordering::Acquire) {
            return Err(failure(ErrorKind::Cancelled, "Update cancelled"));
        }
        let count = reader
            .read(&mut buffer)
            .map_err(|e| failure(ErrorKind::Network, e))?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > size {
            return Err(failure(
                ErrorKind::Integrity,
                "Download exceeds declared size",
            ));
        }
        hasher.update(&buffer[..count]);
        writer
            .write_all(&buffer[..count])
            .map_err(|e| failure(ErrorKind::Storage, e))?;
        progress.bytes.store(total, Ordering::Relaxed);
    }
    if total != size || format!("{:x}", hasher.finalize()) != expected.to_ascii_lowercase() {
        return Err(failure(
            ErrorKind::Integrity,
            "Downloaded file size or SHA-256 mismatch",
        ));
    }
    Ok(())
}

pub fn hash_file(path: &Path) -> Result<String, UpdateError> {
    let file = File::open(path).map_err(|e| failure(ErrorKind::Storage, e))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut file = file.take(MAX_EXE + 1);
    let mut total = 0;
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|e| failure(ErrorKind::Storage, e))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        hasher.update(&buffer[..n]);
    }
    if total > MAX_EXE {
        return Err(failure(
            ErrorKind::Integrity,
            "Executable exceeds size limit",
        ));
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn verify_pe(path: &Path, architecture: &str) -> Result<(), UpdateError> {
    use std::io::{Seek, SeekFrom};
    let mut file = File::open(path).map_err(|e| failure(ErrorKind::Storage, e))?;
    let mut header = [0; 64];
    file.read_exact(&mut header)
        .map_err(|e| failure(ErrorKind::Integrity, e))?;
    if &header[..2] != b"MZ" {
        return Err(failure(
            ErrorKind::Integrity,
            "Update is not a Windows executable",
        ));
    }
    let offset = u32::from_le_bytes(header[60..64].try_into().unwrap());
    if offset > 1024 * 1024 {
        return Err(failure(ErrorKind::Integrity, "Invalid PE offset"));
    }
    file.seek(SeekFrom::Start(offset as u64))
        .and_then(|_| file.read_exact(&mut header[..6]))
        .map_err(|e| failure(ErrorKind::Integrity, e))?;
    let machine = u16::from_le_bytes([header[4], header[5]]);
    let expected = match architecture {
        "x86_64" => 0x8664,
        "x86" => 0x14c,
        "aarch64" => 0xaa64,
        _ => 0,
    };
    if &header[..4] != b"PE\0\0" || machine != expected {
        return Err(failure(
            ErrorKind::Incompatible,
            "Update PE architecture does not match",
        ));
    }
    Ok(())
}

pub fn cleanup_staging(plan: &InstallPlan) {
    for path in [&plan.staged, &plan.helper, &plan.waiting, &plan.path()] {
        let _ = fs::remove_file(path);
    }
}

// The transaction is separate from process handling so rollback is tested with fake restarts.
pub fn install_transaction(
    plan: &InstallPlan,
    restart_and_wait: impl FnOnce(&Path, &Path) -> Result<(), String>,
) -> Result<(), UpdateError> {
    if hash_file(&plan.staged)? != plan.sha256 {
        return Err(failure(
            ErrorKind::Integrity,
            "Staged update changed before installation",
        ));
    }
    verify_pe(&plan.staged, std::env::consts::ARCH)?;
    let mut original = File::open(&plan.target).map_err(|e| failure(ErrorKind::Storage, e))?;
    let mut backup = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&plan.backup)
        .map_err(|e| failure(ErrorKind::Storage, e))?;
    std::io::copy(&mut original, &mut backup).map_err(|e| failure(ErrorKind::Storage, e))?;
    backup
        .sync_all()
        .map_err(|e| failure(ErrorKind::Storage, e))?;
    drop(backup);
    drop(original);
    replace_file(&plan.staged, &plan.target).map_err(|e| failure(ErrorKind::Storage, e))?;
    if let Err(error) = restart_and_wait(&plan.target, &plan.path()) {
        replace_file(&plan.backup, &plan.target).map_err(|e| {
            failure(
                ErrorKind::Install,
                format!(
                    "Restart failed: {error}; restore backup failed: {e}. Backup: {}",
                    plan.backup.display()
                ),
            )
        })?;
        return Err(failure(
            ErrorKind::Install,
            format!("Restart failed; previous executable restored: {error}"),
        ));
    }
    let _ = fs::remove_file(&plan.backup);
    let _ = fs::remove_file(&plan.acknowledgement);
    let _ = fs::remove_file(&plan.waiting);
    let _ = fs::remove_file(plan.path());
    Ok(())
}

#[cfg(windows)]
pub fn spawn_helper(plan: &InstallPlan) -> Result<(), UpdateError> {
    use std::os::windows::process::CommandExt;
    let mut child = std::process::Command::new(&plan.helper)
        .arg("--apply-update")
        .arg(plan.path())
        .creation_flags(0x08000000)
        .spawn()
        .map_err(|e| failure(ErrorKind::Install, e))?;
    // Called on the update worker. Keep the original alive until the helper has opened its
    // process handle; otherwise an unusually quick exit can race helper initialization.
    for _ in 0..60 {
        if plan.waiting.exists() {
            return Ok(());
        }
        if child
            .try_wait()
            .map_err(|e| failure(ErrorKind::Install, e))?
            .is_some()
        {
            return Err(failure(
                ErrorKind::Install,
                "Update helper exited before becoming ready",
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    Err(failure(
        ErrorKind::Install,
        "Update helper did not become ready",
    ))
}

#[cfg(windows)]
pub fn run_helper(path: &Path) -> Result<(), UpdateError> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            QueryFullProcessImageNameW, WaitForSingleObject,
        },
    };
    let bytes = fs::read(path).map_err(|e| failure(ErrorKind::Storage, e))?;
    if bytes.len() > 16 * 1024 {
        return Err(failure(ErrorKind::Install, "Invalid update plan size"));
    }
    let plan: InstallPlan =
        serde_json::from_slice(&bytes).map_err(|e| failure(ErrorKind::Install, e))?;
    plan.validate(&std::env::current_exe().map_err(|e| failure(ErrorKind::Install, e))?)?;
    if path != plan.path() {
        return Err(failure(
            ErrorKind::Install,
            "Update plan path does not match",
        ));
    }
    // SAFETY: Open only the recorded parent for querying/waiting, without termination privileges.
    let parent = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            plan.parent_pid,
        )
    };
    if parent.is_null() {
        return Err(failure(
            ErrorKind::Install,
            "Cannot verify the updating process",
        ));
    }
    let mut image = [0u16; 32768];
    let mut length = image.len() as u32;
    // SAFETY: The handle is live and image has length elements of writable storage.
    let queried = unsafe { QueryFullProcessImageNameW(parent, 0, image.as_mut_ptr(), &mut length) };
    let matches = queried != 0
        && fs::canonicalize(PathBuf::from(String::from_utf16_lossy(
            &image[..length as usize],
        )))
        .ok()
            == Some(plan.target.clone());
    if matches && let Err(error) = fs::write(&plan.waiting, b"waiting") {
        // SAFETY: Close the process handle before returning the storage error.
        unsafe {
            CloseHandle(parent);
        }
        return Err(failure(ErrorKind::Storage, error));
    }
    // SAFETY: Wait on the verified process handle for a finite duration, then close exactly once.
    let waited = if matches {
        // SAFETY: The previously opened process handle remains live until the following close.
        unsafe { WaitForSingleObject(parent, 60_000) }
    } else {
        u32::MAX
    };
    // SAFETY: This function owns the handle returned by OpenProcess.
    unsafe {
        CloseHandle(parent);
    }
    if waited != 0 {
        return Err(failure(
            ErrorKind::Install,
            "Original process did not exit; no file was replaced",
        ));
    }
    let result = install_transaction(&plan, |target, plan_path| {
        let mut child = std::process::Command::new(target)
            .arg("--update-ack")
            .arg(plan_path)
            .creation_flags(0x08000000)
            .spawn()
            .map_err(|e| e.to_string())?;
        for _ in 0..100 {
            if plan.acknowledgement.exists() {
                return Ok(());
            }
            if child.try_wait().map_err(|e| e.to_string())?.is_some() {
                return Err("Updated application exited before acknowledgement".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Only the just-created, unacknowledged replacement is stopped to release its file lock.
        child.kill().map_err(|e| e.to_string())?;
        let _ = child.wait();
        Err("Updated application did not acknowledge startup within 10 seconds".into())
    });
    if result.is_err() {
        let _ = std::process::Command::new(&plan.target)
            .creation_flags(0x08000000)
            .spawn();
    }
    result
}

pub fn acknowledge(path: &Path) -> Result<(), UpdateError> {
    let bytes = fs::read(path).map_err(|e| failure(ErrorKind::Storage, e))?;
    if bytes.len() > 16 * 1024 {
        return Err(failure(ErrorKind::Install, "Invalid update plan size"));
    }
    let plan: InstallPlan =
        serde_json::from_slice(&bytes).map_err(|e| failure(ErrorKind::Install, e))?;
    plan.validate(&plan.helper)?;
    if path != plan.path()
        || fs::canonicalize(std::env::current_exe().map_err(|e| failure(ErrorKind::Install, e))?)
            .ok()
            != Some(plan.target.clone())
    {
        return Err(failure(
            ErrorKind::Install,
            "Restart acknowledgement does not match the installed executable",
        ));
    }
    fs::write(&plan.acknowledgement, b"ready").map_err(|e| failure(ErrorKind::Storage, e))?;
    // The replacement may clean the temporary helper only after Windows releases its image.
    let helper = plan.helper;
    let _ = std::thread::Builder::new()
        .name("update-cleanup".into())
        .spawn(move || {
            for _ in 0..60 {
                match fs::remove_file(&helper) {
                    Ok(()) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                    Err(_) => std::thread::sleep(Duration::from_millis(100)),
                }
            }
        });
    Ok(())
}
