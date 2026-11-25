//! The built-in `ondevice` transcription provider: NVIDIA Parakeet TDT run
//! in-process by transcribe.cpp (ggml), so audio never leaves the machine and
//! no key or account is involved. Model files are downloaded only by an
//! explicit `skywrite models download` (or `transcribe --download`) from a
//! pinned URL, checked against a pinned size and SHA-256, and kept in the
//! shared cache until `skywrite models remove`.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const PROVIDER: &str = "ondevice";

/// One downloadable on-device model. Everything that decides the bytes is pinned.
#[derive(Debug)]
pub struct Spec {
    pub id: &'static str,
    pub name: &'static str,
    pub file: &'static str,
    pub url: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
    pub source: &'static str,
    pub license: &'static str,
    pub attribution: &'static str,
    pub languages: &'static [&'static str],
}

pub const MODELS: &[Spec] = &[Spec {
    id: "parakeet-tdt-0.6b-v3",
    name: "NVIDIA Parakeet TDT 0.6B v3 (Q8_0 GGUF)",
    file: "parakeet-tdt-0.6b-v3-Q8_0.gguf",
    url: "https://huggingface.co/handy-computer/parakeet-tdt-0.6b-v3-gguf/resolve/85ac09ea12fc4b1112fa76810059364bc6adc9de/parakeet-tdt-0.6b-v3-Q8_0.gguf",
    bytes: 739_508_576,
    sha256: "5859f77944efcd8eafa23a6350731960b2b55b2203df51f319665c807d802cc7",
    source: "https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3",
    license: "CC-BY-4.0",
    attribution: "Parakeet TDT 0.6B v3 by NVIDIA (https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3), licensed CC BY 4.0 (https://creativecommons.org/licenses/by/4.0/). GGUF Q8_0 conversion by handy-computer for transcribe.cpp; quantized from the original weights.",
    languages: &[
        "bg", "cs", "da", "de", "el", "en", "es", "et", "fi", "fr", "hr", "hu", "it", "lt", "lv",
        "mt", "nl", "pl", "pt", "ro", "ru", "sk", "sl", "sv", "uk",
    ],
}];

/// Stalls longer than this abort a download; a resumed run continues it.
const READ_STALL: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound for one download run, however slowly bytes arrive.
const DOWNLOAD_DEADLINE: Duration = Duration::from_secs(6 * 3600);
const MAX_REDIRECTS: usize = 10;
/// How long a held model lock is retried before "another process" is reported.
const LOCK_WAIT: Duration = Duration::from_secs(1);

/// An in-process engine: one 16 kHz mono chunk in, a verbose transcription
/// body (`text`, `words` with chunk-relative times, optional `usage`) out.
pub trait ChunkEngine {
    fn transcribe_chunk(&mut self, samples: &[i16], language: Option<&str>) -> Result<Value>;
}

pub fn spec(id: &str) -> Result<&'static Spec> {
    MODELS.iter().find(|spec| spec.id == id).with_context(|| {
        format!(
            "unknown on-device model {id:?}; available: {}",
            MODELS
                .iter()
                .map(|spec| format!("{PROVIDER}:{}", spec.id))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

pub fn reference(spec: &Spec) -> String {
    format!("{PROVIDER}:{}", spec.id)
}

fn ondevice_root(cache_dir: Option<&Path>) -> Result<PathBuf> {
    Ok(crate::cache::cache_root(cache_dir)?.join(PROVIDER))
}

fn model_dir(spec: &Spec, cache_dir: Option<&Path>) -> Result<PathBuf> {
    Ok(ondevice_root(cache_dir)?.join(spec.id))
}

fn c_name(name: &str) -> Result<std::ffi::CString> {
    std::ffi::CString::new(name).context("file name contains a NUL byte")
}

fn last_os_error(what: &str, path: &Path) -> anyhow::Error {
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ELOOP) || error.raw_os_error() == Some(libc::ENOTDIR) {
        anyhow::anyhow!(
            "{} is a symlink or not a directory/regular file; remove it by hand",
            path.display()
        )
    } else {
        anyhow::Error::new(error).context(format!("{what} {}", path.display()))
    }
}

/// An open, verified directory of the model cache. Every later open, rename
/// and unlink is relative to this descriptor, so replacing a path component
/// with a symlink after the check cannot redirect them outside the cache.
struct Dir {
    fd: std::os::fd::OwnedFd,
    path: PathBuf,
}
impl Dir {
    /// `<cache>/ondevice`, created private (0700) and opened without
    /// following a symlink at its own name.
    fn root(cache_dir: Option<&Path>) -> Result<Self> {
        let path = ondevice_root(cache_dir)?;
        crate::cache::private_dir(&path)?;
        Self::open_at(libc::AT_FDCWD, &path, &path)?
            .with_context(|| format!("{} disappeared", path.display()))
    }
    /// `<cache>/ondevice` if it exists; never creates anything.
    fn existing_root(cache_dir: Option<&Path>) -> Result<Option<Self>> {
        let path = ondevice_root(cache_dir)?;
        Self::open_at(libc::AT_FDCWD, &path, &path)
    }
    fn open_at(parent: libc::c_int, name: &Path, path: &Path) -> Result<Option<Self>> {
        use std::os::fd::FromRawFd;
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(name.as_os_str().as_bytes())?;
        // SAFETY: a valid NUL-terminated name; the descriptor is owned below.
        let fd = unsafe {
            libc::openat(
                parent,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(last_os_error("opening directory", path));
        }
        // SAFETY: openat returned a fresh descriptor that nothing else owns.
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        Ok(Some(Self {
            fd,
            path: path.to_owned(),
        }))
    }
    /// Where `name` in this directory is, for messages; every operation goes
    /// through the descriptor, never this path.
    fn path_of(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
    fn raw(&self) -> libc::c_int {
        use std::os::fd::AsRawFd;
        self.fd.as_raw_fd()
    }
    /// A model's own directory inside the root; with `create`, made 0700.
    fn child(&self, name: &str, create: bool) -> Result<Option<Self>> {
        let path = self.path_of(name);
        if create {
            // SAFETY: valid descriptor and NUL-terminated name.
            if unsafe { libc::mkdirat(self.raw(), c_name(name)?.as_ptr(), 0o700) } != 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(last_os_error("creating", &path));
            }
        }
        Self::open_at(self.raw(), Path::new(name), &path)
    }
    /// Open a file here without following symlinks or blocking on a FIFO,
    /// refusing anything but a regular file with a single link, so writing,
    /// hashing or loading never reaches a file outside the cache. `None` when
    /// it does not exist and `write` is off.
    fn open_file(&self, name: &str, write: bool) -> Result<Option<std::fs::File>> {
        use std::os::fd::FromRawFd;
        use std::os::unix::fs::MetadataExt;
        let mut flags = libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
        flags |= if write {
            libc::O_RDWR | libc::O_CREAT
        } else {
            libc::O_RDONLY
        };
        let path = self.path_of(name);
        // SAFETY: valid descriptor and NUL-terminated name; mode for O_CREAT.
        let fd = unsafe {
            libc::openat(
                self.raw(),
                c_name(name)?.as_ptr(),
                flags,
                0o600 as libc::c_uint,
            )
        };
        if fd < 0 {
            if !write && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(last_os_error("opening", &path));
        }
        // SAFETY: openat returned a fresh descriptor that nothing else owns.
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            bail!(
                "{} is not a regular file with a single link; remove it by hand",
                path.display()
            );
        }
        // Regular files ignore O_NONBLOCK, but clear it for plain blocking I/O.
        // SAFETY: fcntl on the descriptor `file` owns.
        unsafe {
            use std::os::fd::AsRawFd;
            let current = libc::fcntl(file.as_raw_fd(), libc::F_GETFL);
            if current >= 0 {
                libc::fcntl(file.as_raw_fd(), libc::F_SETFL, current & !libc::O_NONBLOCK);
            }
        }
        Ok(Some(file))
    }
    /// Size of a regular file here, without following a symlink.
    fn file_len(&self, name: &str) -> Result<Option<u64>> {
        // SAFETY: zeroed stat buffer filled by fstatat.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: valid descriptor, NUL-terminated name and buffer.
        let rc = unsafe {
            libc::fstatat(
                self.raw(),
                c_name(name)?.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(last_os_error("checking", &self.path_of(name)));
        }
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
            bail!("{} is not a regular file", self.path_of(name).display());
        }
        Ok(Some(stat.st_size as u64))
    }
    /// Remove a name here (never what a symlink points to); its size if it was a file.
    fn unlink(&self, name: &str) -> Result<u64> {
        let length = self.file_len(name).unwrap_or(None).unwrap_or(0);
        // SAFETY: valid descriptor and NUL-terminated name.
        if unsafe { libc::unlinkat(self.raw(), c_name(name)?.as_ptr(), 0) } != 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
                return Ok(0);
            }
            return Err(last_os_error("removing", &self.path_of(name)));
        }
        Ok(length)
    }
    fn rename(&self, from: &str, to: &str) -> Result<()> {
        // SAFETY: valid descriptors and NUL-terminated names.
        if unsafe {
            libc::renameat(
                self.raw(),
                c_name(from)?.as_ptr(),
                self.raw(),
                c_name(to)?.as_ptr(),
            )
        } != 0
        {
            return Err(last_os_error("publishing", &self.path_of(to)));
        }
        Ok(())
    }
    fn rmdir(&self, name: &str) {
        // Only an empty directory goes; anything else in it is kept.
        if let Ok(name) = c_name(name) {
            // SAFETY: valid descriptor and NUL-terminated name.
            unsafe { libc::unlinkat(self.raw(), name.as_ptr(), libc::AT_REMOVEDIR) };
        }
    }
    fn sync(&self) -> Result<()> {
        // SAFETY: fsync on the descriptor this struct owns.
        if unsafe { libc::fsync(self.raw()) } != 0 {
            return Err(last_os_error("syncing", &self.path));
        }
        Ok(())
    }
}

/// Same file (device and inode) as an open handle.
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

pub fn model_path(spec: &Spec, cache_dir: Option<&Path>) -> Result<PathBuf> {
    Ok(model_dir(spec, cache_dir)?.join(spec.file))
}

fn regular_len(path: &Path) -> Result<Option<u64>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata.len())),
        Ok(_) => bail!("{} is not a regular file", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("checking {}", path.display())),
    }
}

fn not_downloaded(spec: &Spec) -> String {
    format!(
        "on-device model {} is not downloaded ({} MB, {} licence). Download it once with `skywrite models download {}`, or add --download to this transcribe command; after that transcription works offline",
        reference(spec),
        spec.bytes / 1_000_000,
        spec.license,
        reference(spec)
    )
}

/// Catalog entry with local status; reads only file metadata.
pub fn describe(spec: &Spec, cache_dir: Option<&Path>) -> Result<Value> {
    let path = model_path(spec, cache_dir)?;
    let present = regular_len(&path)?;
    let partial = regular_len(&part_path(&path))?;
    Ok(json!({
        "id": spec.id,
        "reference": reference(spec),
        "provider": PROVIDER,
        "name": spec.name,
        "task": "transcription",
        "downloaded": present == Some(spec.bytes),
        "path": path,
        "bytes": spec.bytes,
        "partial_bytes": partial,
        "sha256": spec.sha256,
        "url": spec.url,
        "source": spec.source,
        "license": spec.license,
        "attribution": spec.attribution,
        "languages": spec.languages,
        "word_timestamps": "model token timings (80 ms encoder frames)",
        "engine": engine_summary(),
        "pricing_note": "Free: runs on this computer. Audio never leaves it; the only network use is the explicit model download.",
    }))
}

pub fn list(cache_dir: Option<&Path>, search: Option<&str>) -> Result<Value> {
    let mut models = Vec::new();
    for spec in MODELS {
        if search.is_some_and(|s| !spec.id.contains(&s.to_lowercase())) {
            continue;
        }
        models.push(describe(spec, cache_dir)?);
    }
    Ok(json!({
        "version": 1,
        "provider": provider_summary(),
        "task": "transcription",
        "models": models,
        "network_requests": false,
        "paid_inference_performed": false,
        "note": "On-device models are fixed and pinned; download one with skywrite models download REFERENCE and remove it with skywrite models remove REFERENCE.",
    }))
}

pub fn provider_summary() -> Value {
    json!({
        "name": PROVIDER,
        "builtin": true,
        "base_url": null,
        "key_env": null,
        "key_present": false,
        "tasks": ["transcription"],
        "models": MODELS.iter().map(reference).collect::<Vec<_>>(),
        "engine": engine_summary(),
        "note": "Runs in this process; audio never leaves the computer. Models are downloaded only with skywrite models download.",
        "warnings": engine_unavailable().into_iter().collect::<Vec<_>>(),
    })
}

fn engine_summary() -> Value {
    json!({
        "name": "transcribe.cpp (ggml)",
        "compiled": cfg!(feature = "ondevice"),
        "acceleration": acceleration(),
    })
}

fn acceleration() -> &'static str {
    if cfg!(all(
        feature = "ondevice",
        target_os = "macos",
        target_arch = "aarch64"
    )) {
        "Metal (Apple GPU), CPU fallback"
    } else {
        "CPU"
    }
}

/// Why the engine cannot run in this binary or on this CPU, if it cannot.
pub fn engine_unavailable() -> Option<String> {
    if !cfg!(feature = "ondevice") {
        return Some("this skywrite was built without the on-device engine (cargo feature `ondevice`); release binaries include it".into());
    }
    cpu_unsupported()
}

/// Release builds pin a CPU floor for the C/C++ engine; without these
/// instructions inference would crash with SIGILL, so it is refused first.
#[cfg(target_arch = "x86_64")]
fn cpu_unsupported() -> Option<String> {
    let mut missing = Vec::new();
    #[cfg(target_os = "macos")]
    {
        if !std::arch::is_x86_feature_detected!("sse4.2") {
            missing.push("SSE4.2");
        }
        if !std::arch::is_x86_feature_detected!("avx") {
            missing.push("AVX");
        }
        if !std::arch::is_x86_feature_detected!("f16c") {
            missing.push("F16C");
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        for (present, name) in [
            (std::arch::is_x86_feature_detected!("avx2"), "AVX2"),
            (std::arch::is_x86_feature_detected!("fma"), "FMA"),
            (std::arch::is_x86_feature_detected!("f16c"), "F16C"),
            (std::arch::is_x86_feature_detected!("bmi2"), "BMI2"),
        ] {
            if !present {
                missing.push(name);
            }
        }
    }
    (!missing.is_empty()).then(|| {
        format!(
            "this CPU lacks {} needed by the on-device engine (x86-64 from about 2013 on, Intel Haswell or AMD Excavator); use a hosted transcription model instead",
            missing.join(", ")
        )
    })
}
#[cfg(not(target_arch = "x86_64"))]
fn cpu_unsupported() -> Option<String> {
    None
}

fn part_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

fn sha256_open(file: &mut std::fs::File) -> Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// An exclusive lock for one model, held for a whole download or removal.
/// The lock file lives beside the model directory and is never deleted, so
/// every process always locks the same inode.
struct ModelLock {
    _file: std::fs::File,
}
impl ModelLock {
    fn acquire(spec: &Spec, root: &Dir) -> Result<Self> {
        use std::os::fd::AsRawFd;
        let file = root
            .open_file(&format!(".{}.lock", spec.id), true)?
            .context("creating the model lock file")?;
        // A just-released lock can stay held for a moment: a process forked
        // elsewhere in this program shares the descriptor until it execs. So
        // a held lock is retried briefly before it is reported.
        let deadline = std::time::Instant::now() + LOCK_WAIT;
        loop {
            // SAFETY: flock on a descriptor this struct owns for the lock's lifetime.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self { _file: file });
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            if error.kind() != std::io::ErrorKind::WouldBlock {
                return Err(error).context("locking the model directory");
            }
            if std::time::Instant::now() >= deadline {
                bail!(
                    "another skywrite process is downloading or removing {}; wait for it to finish",
                    reference(spec)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Download (or resume) a pinned model, then verify its full SHA-256 before
/// publishing it under its final name. Idempotent: a verified file is kept.
pub fn download(spec: &Spec, cache_dir: Option<&Path>) -> Result<Value> {
    download_from(spec, spec.url, cache_dir)
}

fn download_from(spec: &Spec, url: &str, cache_dir: Option<&Path>) -> Result<Value> {
    let root = Dir::root(cache_dir)?;
    let _lock = ModelLock::acquire(spec, &root)?;
    let dir = root
        .child(spec.id, true)?
        .context("creating the model directory")?;
    let part_name = format!("{}.part", spec.file);
    let part = dir.path_of(&part_name);
    let warnings: Vec<String> = engine_unavailable().into_iter().collect();
    if let Some(mut file) = dir.open_file(spec.file, false)? {
        if file.metadata()?.len() == spec.bytes && sha256_open(&mut file)? == spec.sha256 {
            return Ok(
                json!({"downloaded": describe(spec, cache_dir)?, "already_present": true, "bytes_transferred": 0, "verified_sha256": spec.sha256, "warnings": warnings}),
            );
        }
        drop(file);
        // A damaged final file is never trusted or resumed.
        dir.unlink(spec.file)?;
        eprintln!(
            "Removed {}: it did not match the pinned size and SHA-256",
            dir.path_of(spec.file).display()
        );
    }
    // One validated handle appends to and hashes the partial file.
    let mut file = dir
        .open_file(&part_name, true)?
        .context("creating the partial download")?;
    let mut offset = file.metadata()?.len();
    if offset > spec.bytes {
        file.set_len(0)?;
        offset = 0;
    }
    let transferred = if offset < spec.bytes {
        fetch(url, &mut file, &part, offset, spec.bytes)?
    } else {
        0
    };
    eprintln!("Verifying SHA-256 of {}", part.display());
    let actual = sha256_open(&mut file)?;
    if actual != spec.sha256 {
        drop(file);
        dir.unlink(&part_name)?;
        bail!(
            "downloaded {} has SHA-256 {actual}, not the pinned {}; the partial file was deleted, run the download again",
            spec.file,
            spec.sha256
        );
    }
    file.sync_all()?;
    dir.rename(&part_name, spec.file)?;
    dir.sync()?;
    // Publish exactly the bytes that were hashed: the name must still lead
    // to the same inode as the verified handle.
    let verified = file.metadata()?;
    let published = dir
        .open_file(spec.file, false)?
        .map(|published| published.metadata())
        .transpose()?;
    if !published.is_some_and(|published| same_file(&published, &verified)) {
        dir.unlink(spec.file)?;
        bail!(
            "{} changed while it was being published; it was removed, run the download again",
            dir.path_of(spec.file).display()
        );
    }
    Ok(
        json!({"downloaded": describe(spec, cache_dir)?, "already_present": false, "resumed_from_bytes": offset, "bytes_transferred": transferred, "verified_sha256": actual, "warnings": warnings}),
    )
}

/// Append bytes `offset..total` of `url` to the open partial file. The server
/// must honour the range or restart from zero; more bytes than pinned are
/// never written.
fn fetch(url: &str, file: &mut std::fs::File, part: &Path, offset: u64, total: u64) -> Result<u64> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        tokio::time::timeout(
            DOWNLOAD_DEADLINE,
            fetch_async(url, file, part, offset, total),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "download exceeded {} hours; run it again to resume",
                DOWNLOAD_DEADLINE.as_secs() / 3600
            )
        })?
    })
}

async fn fetch_async(
    url: &str,
    file: &mut std::fs::File,
    part: &Path,
    offset: u64,
    total: u64,
) -> Result<u64> {
    let initial = url::Url::parse(url)?;
    let allow_http =
        initial.scheme() == "http" && matches!(initial.host_str(), Some("127.0.0.1" | "localhost"));
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_STALL)
        .user_agent(concat!("skywrite/", env!("CARGO_PKG_VERSION")))
        // The bytes are pinned by size and SHA-256, so the CDN a mirror
        // redirects to does not need to be trusted; plain http is refused.
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else if attempt.url().scheme() != "https" && !allow_http {
                attempt.error("refusing a redirect to plain http")
            } else {
                attempt.follow()
            }
        }))
        .build()?;
    if initial.scheme() != "https" && !allow_http {
        bail!("model URL must be https");
    }
    let mut request = client.get(url);
    if offset > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
    }
    let mut response = request.send().await.map_err(|error| {
        anyhow::anyhow!(
            "model download request failed: {error}; run the same command again to resume"
        )
    })?;
    let status = response.status();
    let start = match status.as_u16() {
        206 => {
            let range = response
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .context("partial response without Content-Range")?;
            if range != format!("bytes {offset}-{}/{total}", total - 1) {
                bail!(
                    "server answered range {range:?} for bytes {offset}- of a {total}-byte file; delete the .part file and retry"
                );
            }
            offset
        }
        200 => {
            if offset > 0 {
                eprintln!("Server ignored the resume request; starting the download again");
            }
            0
        }
        416 if offset > 0 => bail!(
            "server rejected resuming at byte {offset}; delete {} and retry",
            part.display()
        ),
        _ => bail!("model download failed with HTTP {status}"),
    };
    if let Some(length) = response.content_length()
        && length != total - start
    {
        bail!(
            "server announced {length} bytes, but {} remain of the pinned {total}-byte file",
            total - start
        );
    }
    file.set_len(start)?;
    file.seek(SeekFrom::Start(start))?;
    let mut written = start;
    let mut reported = written * 20 / total;
    eprintln!(
        "Downloading {} MB to {}{}",
        total / 1_000_000,
        part.display(),
        if start > 0 {
            format!(" (resuming at {} MB)", start / 1_000_000)
        } else {
            String::new()
        }
    );
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        anyhow::anyhow!("model download interrupted: {error}; run the same command again to resume")
    })? {
        if chunk.len() as u64 > total - written {
            bail!("server sent more than the pinned {total} bytes");
        }
        file.write_all(&chunk)?;
        written += chunk.len() as u64;
        if written * 20 / total > reported {
            reported = written * 20 / total;
            eprintln!("Downloaded {}%", reported * 5);
        }
    }
    file.sync_all()?;
    if written != total {
        bail!("download ended at {written} of {total} bytes; run the same command again to resume");
    }
    Ok(written - start)
}

/// Refuse media that is this model's cache file (or partial download):
/// `transcribe --download` may delete a damaged model file, and inputs are
/// never modified.
pub fn check_input_is_not_model(spec: &Spec, cache_dir: Option<&Path>, input: &Path) -> Result<()> {
    let Ok(media) = std::fs::metadata(input) else {
        return Ok(());
    };
    let path = model_path(spec, cache_dir)?;
    for candidate in [path.clone(), part_path(&path)] {
        if let Ok(cached) = std::fs::symlink_metadata(&candidate)
            && same_file(&media, &cached)
        {
            bail!(
                "input {} is the on-device model cache file {}; transcribe a media file",
                input.display(),
                candidate.display()
            );
        }
    }
    Ok(())
}

/// Delete a downloaded model (and any partial download) from the cache.
pub fn remove(spec: &Spec, cache_dir: Option<&Path>) -> Result<Value> {
    let path = model_path(spec, cache_dir)?;
    let Some(root) = Dir::existing_root(cache_dir)? else {
        return Ok(
            json!({"removed": reference(spec), "path": path, "freed_bytes": 0, "was_present": false}),
        );
    };
    let _lock = ModelLock::acquire(spec, &root)?;
    let mut freed = 0;
    if let Some(dir) = root.child(spec.id, false)? {
        freed += dir.unlink(spec.file)?;
        freed += dir.unlink(&format!("{}.part", spec.file))?;
        root.rmdir(spec.id);
    }
    Ok(
        json!({"removed": reference(spec), "path": path, "freed_bytes": freed, "was_present": freed > 0}),
    )
}

/// The model file, open, at its exact pinned size and SHA-256. Loading goes
/// through this verified handle, so the bytes checked are the bytes used.
fn verified_model(spec: &Spec, cache_dir: Option<&Path>) -> Result<(PathBuf, std::fs::File)> {
    let path = model_path(spec, cache_dir)?;
    let Some(dir) = Dir::existing_root(cache_dir)?
        .map(|root| root.child(spec.id, false))
        .transpose()?
        .flatten()
    else {
        bail!("{}", not_downloaded(spec));
    };
    let Some(mut file) = dir.open_file(spec.file, false)? else {
        bail!("{}", not_downloaded(spec));
    };
    if file.metadata()?.len() != spec.bytes {
        bail!(
            "{} does not have the pinned size; remove it with `skywrite models remove {}` and download it again",
            path.display(),
            reference(spec)
        );
    }
    if sha256_open(&mut file)? != spec.sha256 {
        bail!(
            "{} does not match the pinned SHA-256; remove it with `skywrite models remove {}` and download it again",
            path.display(),
            reference(spec)
        );
    }
    file.seek(SeekFrom::Start(0))?;
    Ok((path, file))
}

/// Load the verified model so that the bytes checked are the bytes used.
///
/// Linux: through `/proc/self/fd/N`, which opens the verified inode itself
/// with a fresh file offset (the native loader opens its path twice, for a
/// format sniff and for parsing, and reads from offset 0 each time).
/// macOS has no such path: `/dev/fd/N` duplicates the descriptor and shares
/// its offset, which breaks that second read. There the validated path is
/// loaded and must name the verified inode both before and after loading;
/// only another process of the same user swapping files at that instant
/// could defeat this, and it could already change the cache or the binary.
#[cfg(feature = "ondevice")]
fn load_verified_inode(path: &Path, file: &std::fs::File) -> Result<transcribe_cpp::Model> {
    #[cfg(target_os = "linux")]
    {
        let _ = path;
        let source = verified_descriptor_path(file)?;
        Ok(transcribe_cpp::Model::load(&source)?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let verified = file.metadata()?;
        let unchanged =
            || std::fs::symlink_metadata(path).is_ok_and(|current| same_file(&current, &verified));
        if !unchanged() {
            bail!(
                "{} changed after it was verified; run the command again",
                path.display()
            );
        }
        let model = transcribe_cpp::Model::load(path)?;
        if !unchanged() {
            bail!(
                "{} changed while it was loading; run the command again",
                path.display()
            );
        }
        Ok(model)
    }
}

/// `/proc/self/fd/N` for a verified handle, checked to name the same inode.
#[cfg(all(target_os = "linux", any(feature = "ondevice", test)))]
fn verified_descriptor_path(file: &std::fs::File) -> Result<PathBuf> {
    use std::os::fd::AsRawFd;
    let path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    let reopened = std::fs::metadata(&path).context(
        "the on-device engine loads the verified model through /proc/self/fd; mount /proc",
    )?;
    if !same_file(&reopened, &file.metadata()?) {
        bail!("/proc/self/fd does not refer to this process's descriptors");
    }
    Ok(path)
}

#[cfg(feature = "ondevice")]
pub struct Engine {
    spec: &'static Spec,
    session: transcribe_cpp::Session,
    backend: String,
    threads: i32,
}

/// Half the logical CPUs, 1..=8. The library default (every CPU) is never
/// fastest: TDT decoding runs many tiny steps whose thread barriers cost more
/// than extra cores gain, worst on a busy machine. Measured on 60 s of speech:
/// Apple M4 (10 CPUs) Metal 4.34 s with all CPUs, 1.76 s with 4; an 8-CPU
/// x86-64 under load 38.5 s with all, 16.4 s with 4 (docs/model-evaluation.md).
#[cfg(feature = "ondevice")]
fn session_threads() -> i32 {
    let logical = std::thread::available_parallelism().map_or(2, usize::from);
    (logical / 2).clamp(1, 8) as i32
}

#[cfg(not(feature = "ondevice"))]
pub struct Engine {
    never: std::convert::Infallible,
}

impl Engine {
    /// Verify the model file and load it once for every chunk of a command.
    pub fn load(spec: &'static Spec, cache_dir: Option<&Path>) -> Result<Self> {
        if let Some(reason) = engine_unavailable() {
            bail!("{reason}");
        }
        let (path, file) = verified_model(spec, cache_dir)?;
        Self::load_verified(spec, &path, &file)
    }

    #[cfg(feature = "ondevice")]
    fn load_verified(spec: &'static Spec, path: &Path, file: &std::fs::File) -> Result<Self> {
        transcribe_cpp::disable_logging();
        let model = load_verified_inode(path, file)
            .with_context(|| format!("loading on-device model {}", path.display()))?;
        let capabilities = model.capabilities()?;
        if model.arch() != "parakeet"
            || capabilities.native_sample_rate != 16_000
            || !matches!(
                capabilities.max_timestamp_kind,
                transcribe_cpp::TimestampKind::Word | transcribe_cpp::TimestampKind::Token
            )
        {
            bail!(
                "on-device model {} is not a 16 kHz Parakeet model with word timestamps",
                path.display()
            );
        }
        let backend = model.backend();
        let threads = session_threads();
        let session = model.session_with(&transcribe_cpp::SessionOptions {
            n_threads: threads,
            ..Default::default()
        })?;
        Ok(Self {
            spec,
            session,
            backend,
            threads,
        })
    }
    #[cfg(not(feature = "ondevice"))]
    fn load_verified(_: &'static Spec, _: &Path, _: &std::fs::File) -> Result<Self> {
        unreachable!("engine_unavailable() refuses builds without the engine")
    }
}

impl ChunkEngine for Engine {
    /// Transcribe one 16 kHz mono chunk in the shape of an
    /// `/audio/transcriptions` verbose response, so it passes the same
    /// validation as a provider's. Word times are the model's own token
    /// timings; a result without them is rejected, never estimated.
    #[cfg(feature = "ondevice")]
    fn transcribe_chunk(&mut self, samples: &[i16], language: Option<&str>) -> Result<Value> {
        use transcribe_cpp::TimestampKind;
        if let Some(language) = language
            && !self.spec.languages.contains(&language)
        {
            bail!(
                "{} does not support --language {language}; it transcribes {} and detects the language itself, so omit --language",
                reference(self.spec),
                self.spec.languages.join(", ")
            );
        }
        let pcm: Vec<f32> = samples.iter().map(|&s| f32::from(s) / 32768.0).collect();
        let result = self.session.run(
            &pcm,
            &transcribe_cpp::RunOptions {
                timestamps: TimestampKind::Word,
                language: language.map(str::to_owned),
                ..Default::default()
            },
        )?;
        let text = result.text.trim();
        if !text.is_empty()
            && (!matches!(
                result.timestamp_kind,
                TimestampKind::Word | TimestampKind::Token
            ) || result.words.is_empty())
        {
            bail!("on-device engine returned text without word timestamps");
        }
        let words: Vec<Value> = result
            .words
            .iter()
            .map(|word| {
                json!({
                    "word": word.text.trim(),
                    "start": word.t0_ms as f64 / 1000.0,
                    "end": word.t1_ms as f64 / 1000.0,
                })
            })
            .collect();
        let mut usage = json!({
            "engine": format!("transcribe.cpp {}", transcribe_cpp::version()),
            "backend": self.backend,
            "threads": self.threads,
            "model_sha256": self.spec.sha256,
            "encode_ms": result.timings.encode_ms.round(),
            "decode_ms": result.timings.decode_ms.round(),
            "cost": 0,
        });
        if let Some(language) = &result.language {
            usage["detected_language"] = json!(language);
        }
        Ok(json!({"text": text, "words": words, "usage": usage}))
    }
    #[cfg(not(feature = "ondevice"))]
    fn transcribe_chunk(&mut self, _: &[i16], _: Option<&str>) -> Result<Value> {
        match self.never {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    fn fixture_spec(bytes: &'static [u8]) -> Spec {
        let hash: String = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Spec {
            id: "fixture",
            name: "fixture",
            file: "fixture.gguf",
            url: "unused",
            bytes: bytes.len() as u64,
            sha256: Box::leak(hash.into_boxed_str()),
            source: "",
            license: "CC-BY-4.0",
            attribution: "",
            languages: &["en"],
        }
    }

    /// A loopback server answering each connection with `respond(range_start)`.
    fn serve(
        connections: usize,
        respond: impl Fn(Option<u64>) -> Vec<u8> + Send + 'static,
    ) -> (String, std::thread::JoinHandle<Vec<Option<u64>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/model.gguf", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut ranges = Vec::new();
            for _ in 0..connections {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut range = None;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        range = Some(value.trim().trim_end_matches('-').parse().unwrap());
                    }
                }
                ranges.push(range);
                let mut stream = stream;
                let _ = stream.write_all(&respond(range));
            }
            ranges
        });
        (url, handle)
    }

    fn full(body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    const BODY: &[u8] = b"pinned on-device model bytes for the download fixture";

    #[test]
    fn download_verifies_publishes_and_is_idempotent_then_removable() {
        let cache = tempfile::tempdir().unwrap();
        let spec = fixture_spec(BODY);
        let (url, server) = serve(1, |_| full(BODY));
        let result = download_from(&spec, &url, Some(cache.path())).unwrap();
        assert_eq!(server.join().unwrap(), vec![None]);
        assert_eq!(result["already_present"], false);
        assert_eq!(result["bytes_transferred"], BODY.len());
        let path = model_path(&spec, Some(cache.path())).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), BODY);
        assert!(!part_path(&path).exists());
        assert_eq!(
            describe(&spec, Some(cache.path())).unwrap()["downloaded"],
            true
        );
        // Present and verified: no network at all (nothing listens on this URL).
        let again = download_from(&spec, "http://127.0.0.1:9/none", Some(cache.path())).unwrap();
        assert_eq!(again["already_present"], true);
        assert_eq!(verified_model(&spec, Some(cache.path())).unwrap().0, path);
        let removed = remove(&spec, Some(cache.path())).unwrap();
        assert_eq!(removed["freed_bytes"], BODY.len());
        assert!(!path.exists() && !path.parent().unwrap().exists());
        assert!(
            verified_model(&spec, Some(cache.path()))
                .unwrap_err()
                .to_string()
                .contains("models download ondevice:fixture")
        );
        assert_eq!(
            remove(&spec, Some(cache.path())).unwrap()["was_present"],
            false
        );
    }

    #[test]
    fn interrupted_download_resumes_with_a_range_request() {
        let cache = tempfile::tempdir().unwrap();
        let spec = fixture_spec(BODY);
        let cut = 20;
        let (url, server) = serve(2, move |range| match range {
            None => {
                // Announce the whole file, then drop the connection early.
                let mut out = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    BODY.len()
                )
                .into_bytes();
                out.extend_from_slice(&BODY[..cut]);
                out
            }
            Some(start) => {
                let rest = &BODY[start as usize..];
                let mut out = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    BODY.len() - 1,
                    BODY.len(),
                    rest.len()
                )
                .into_bytes();
                out.extend_from_slice(rest);
                out
            }
        });
        let error = download_from(&spec, &url, Some(cache.path())).unwrap_err();
        assert!(format!("{error:#}").contains("resume"), "{error:#}");
        let path = model_path(&spec, Some(cache.path())).unwrap();
        assert_eq!(std::fs::read(part_path(&path)).unwrap(), &BODY[..cut]);
        let result = download_from(&spec, &url, Some(cache.path())).unwrap();
        assert_eq!(server.join().unwrap(), vec![None, Some(cut as u64)]);
        assert_eq!(result["resumed_from_bytes"], cut);
        assert_eq!(std::fs::read(&path).unwrap(), BODY);
    }

    #[test]
    fn wrong_hash_size_or_oversized_body_is_never_published() {
        let cache = tempfile::tempdir().unwrap();
        let spec = fixture_spec(BODY);
        let path = model_path(&spec, Some(cache.path())).unwrap();
        // Same size, different bytes: rejected by SHA-256 and the part deleted.
        let mut wrong = BODY.to_vec();
        wrong[0] ^= 1;
        let (url, server) = serve(1, move |_| full(&wrong));
        let error = download_from(&spec, &url, Some(cache.path())).unwrap_err();
        server.join().unwrap();
        assert!(error.to_string().contains("not the pinned"), "{error:#}");
        assert!(!path.exists() && !part_path(&path).exists());
        // Announced size differs from the pin: refused before writing.
        let (url, server) = serve(1, |_| full(b"short"));
        let error = download_from(&spec, &url, Some(cache.path())).unwrap_err();
        server.join().unwrap();
        assert!(error.to_string().contains("pinned"), "{error:#}");
        assert!(!path.exists());
        // No Content-Length and more bytes than pinned: stopped at the bound.
        let (url, server) = serve(1, |_| {
            let mut out = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
            out.extend_from_slice(&[b'x'; 4096]);
            out
        });
        let error = download_from(&spec, &url, Some(cache.path())).unwrap_err();
        server.join().unwrap();
        assert!(
            error.to_string().contains("more than the pinned"),
            "{error:#}"
        );
        assert!(!path.exists());
        assert!(regular_len(&part_path(&path)).unwrap().unwrap() <= spec.bytes);
        // A damaged final file is detected and never loaded.
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &BODY[1..]).unwrap();
        assert!(
            verified_model(&spec, Some(cache.path()))
                .unwrap_err()
                .to_string()
                .contains("pinned size")
        );
        let mut tampered = BODY.to_vec();
        tampered[3] ^= 1;
        std::fs::write(&path, tampered).unwrap();
        assert!(
            verified_model(&spec, Some(cache.path()))
                .unwrap_err()
                .to_string()
                .contains("SHA-256")
        );
    }

    #[test]
    fn symlinks_and_hard_links_never_expose_files_outside_the_cache() {
        let cache = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let spec = fixture_spec(BODY);
        let original = outside.path().join("fixture.gguf");
        std::fs::write(&original, BODY).unwrap();
        // A symlinked model directory is refused by remove, download and load.
        let dir = model_dir(&spec, Some(cache.path())).unwrap();
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(outside.path(), &dir).unwrap();
        for error in [
            remove(&spec, Some(cache.path())).unwrap_err(),
            download_from(&spec, "http://127.0.0.1:9/none", Some(cache.path())).unwrap_err(),
            verified_model(&spec, Some(cache.path())).unwrap_err(),
        ] {
            assert!(error.to_string().contains("is a symlink"), "{error:#}");
        }
        assert_eq!(std::fs::read(&original).unwrap(), BODY);
        std::fs::remove_file(&dir).unwrap();
        // A partial download that is a symlink or a hard link is never written.
        std::fs::create_dir(&dir).unwrap();
        let part = part_path(&dir.join(spec.file));
        std::os::unix::fs::symlink(&original, &part).unwrap();
        let error =
            download_from(&spec, "http://127.0.0.1:9/none", Some(cache.path())).unwrap_err();
        assert!(error.to_string().contains("is a symlink"), "{error:#}");
        std::fs::remove_file(&part).unwrap();
        std::fs::hard_link(&original, &part).unwrap();
        let error =
            download_from(&spec, "http://127.0.0.1:9/none", Some(cache.path())).unwrap_err();
        assert!(error.to_string().contains("single link"), "{error:#}");
        assert_eq!(std::fs::read(&original).unwrap(), BODY);
        // The same holds for a linked final model file at load time.
        std::fs::remove_file(&part).unwrap();
        std::fs::hard_link(&original, dir.join(spec.file)).unwrap();
        let error = verified_model(&spec, Some(cache.path())).unwrap_err();
        assert!(error.to_string().contains("single link"), "{error:#}");
        assert_eq!(std::fs::read(&original).unwrap(), BODY);
    }

    #[test]
    fn a_fifo_in_place_of_a_model_file_is_refused_without_blocking() {
        let cache = tempfile::tempdir().unwrap();
        let root = cache.path().to_owned();
        let spec: &'static Spec = Box::leak(Box::new(fixture_spec(BODY)));
        let path = model_path(spec, Some(&root)).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for name in [path.clone(), part_path(&path)] {
            let c = std::ffi::CString::new(name.to_str().unwrap()).unwrap();
            // SAFETY: valid NUL-terminated path.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        }
        // Opening a FIFO for reading would wait forever for a writer; bound it.
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let load = verified_model(spec, Some(&root)).map(|_| ()).unwrap_err();
            let download = download_from(spec, "http://127.0.0.1:9/none", Some(&root)).unwrap_err();
            send.send((load.to_string(), download.to_string())).unwrap();
        });
        let (load, download) = receive
            .recv_timeout(Duration::from_secs(20))
            .expect("a FIFO blocked the model cache");
        assert!(load.contains("regular file"), "{load}");
        assert!(download.contains("regular file"), "{download}");
    }

    #[test]
    fn a_partial_file_swapped_during_the_download_is_never_published() {
        let cache = tempfile::tempdir().unwrap();
        let spec = fixture_spec(BODY);
        let path = model_path(&spec, Some(cache.path())).unwrap();
        let part = part_path(&path);
        let aside = path.with_file_name("aside");
        let (swap_part, swap_aside) = (part.clone(), aside.clone());
        // While the request is in flight, the partial file is replaced by
        // same-sized garbage; the bytes being hashed stay in the original.
        let (url, server) = serve(1, move |_| {
            std::fs::rename(&swap_part, &swap_aside).unwrap();
            std::fs::write(&swap_part, vec![b'x'; BODY.len()]).unwrap();
            full(BODY)
        });
        let error = download_from(&spec, &url, Some(cache.path())).unwrap_err();
        server.join().unwrap();
        assert!(
            error
                .to_string()
                .contains("changed while it was being published"),
            "{error:#}"
        );
        assert!(!path.exists(), "substituted bytes were published");
        assert_eq!(std::fs::read(&aside).unwrap(), BODY);
    }

    #[test]
    fn a_model_directory_swapped_for_a_symlink_mid_download_leaves_its_target_alone() {
        let cache = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let spec = fixture_spec(BODY);
        let dir = model_dir(&spec, Some(cache.path())).unwrap();
        let outside_files = [
            (
                outside.path().join("fixture.gguf"),
                b"external model".to_vec(),
            ),
            (
                outside.path().join("fixture.gguf.part"),
                b"external part".to_vec(),
            ),
        ];
        for (file, bytes) in &outside_files {
            std::fs::write(file, bytes).unwrap();
        }
        let (swap_dir, target) = (dir.clone(), outside.path().to_owned());
        let (url, server) = serve(1, move |_| {
            std::fs::rename(&swap_dir, swap_dir.with_file_name("moved")).unwrap();
            std::os::unix::fs::symlink(&target, &swap_dir).unwrap();
            full(BODY)
        });
        // Every write, rename and check stays in the directory that was opened.
        download_from(&spec, &url, Some(cache.path())).unwrap();
        server.join().unwrap();
        for (file, bytes) in &outside_files {
            assert_eq!(&std::fs::read(file).unwrap(), bytes);
        }
        assert_eq!(
            std::fs::read(dir.with_file_name("moved").join("fixture.gguf")).unwrap(),
            BODY
        );
        // The symlink is then refused, and removal never follows it.
        assert!(verified_model(&spec, Some(cache.path())).is_err());
        assert!(remove(&spec, Some(cache.path())).is_err());
        for (file, bytes) in &outside_files {
            assert_eq!(&std::fs::read(file).unwrap(), bytes);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_loader_path_names_the_verified_bytes_even_after_a_swap() {
        let cache = tempfile::tempdir().unwrap();
        let spec = fixture_spec(BODY);
        let (url, server) = serve(1, |_| full(BODY));
        download_from(&spec, &url, Some(cache.path())).unwrap();
        server.join().unwrap();
        let (path, file) = verified_model(&spec, Some(cache.path())).unwrap();
        // Replace the cache file after verification.
        let swapped = path.with_file_name("swapped");
        std::fs::write(&swapped, vec![b'x'; BODY.len()]).unwrap();
        std::fs::rename(&swapped, &path).unwrap();
        let source = verified_descriptor_path(&file).unwrap();
        // Two independent opens both read the verified bytes from the start.
        assert_eq!(std::fs::read(&source).unwrap(), BODY);
        assert_eq!(std::fs::read(&source).unwrap(), BODY);
    }

    #[test]
    fn one_lock_inode_serialises_download_and_removal() {
        let cache = tempfile::tempdir().unwrap();
        let spec = fixture_spec(BODY);
        let held = ModelLock::acquire(&spec, &Dir::root(Some(cache.path())).unwrap()).unwrap();
        for error in [
            download_from(&spec, "http://127.0.0.1:9/none", Some(cache.path())).unwrap_err(),
            remove(&spec, Some(cache.path())).unwrap_err(),
        ] {
            assert!(
                error.to_string().contains("another skywrite process"),
                "{error:#}"
            );
        }
        drop(held);
        let (url, server) = serve(1, |_| full(BODY));
        download_from(&spec, &url, Some(cache.path())).unwrap();
        server.join().unwrap();
        remove(&spec, Some(cache.path())).unwrap();
        // The lock file outlives removal, so later processes lock the same inode.
        let lock = ondevice_root(Some(cache.path()))
            .unwrap()
            .join(".fixture.lock");
        assert!(lock.is_file());
        let _again = ModelLock::acquire(&spec, &Dir::root(Some(cache.path())).unwrap()).unwrap();
        assert!(ModelLock::acquire(&spec, &Dir::root(Some(cache.path())).unwrap()).is_err());
    }

    #[test]
    fn pinned_models_are_https_hashed_and_attributed() {
        for spec in MODELS {
            assert!(spec.url.starts_with("https://"));
            assert_eq!(spec.sha256.len(), 64);
            assert!(spec.attribution.contains("CC BY 4.0"));
        }
    }
}
