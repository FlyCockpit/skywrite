//! User-wide regeneratable data. Personal styles/skills/config live separately.
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// Explicit flag > app environment > XDG user cache > ~/.cache/skywrite.
/// Relative XDG paths are invalid under the XDG specification and are ignored.
pub fn cache_root(override_dir: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_dir {
        return nonempty(path.to_owned());
    }
    if let Some(path) = std::env::var_os("SKYWRITE_CACHE_DIR") {
        return nonempty(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME") {
        let path = PathBuf::from(path);
        if path.is_absolute() {
            return Ok(path.join("skywrite"));
        }
    }
    let home =
        std::env::var_os("HOME").context("set HOME or SKYWRITE_CACHE_DIR to locate the cache")?;
    Ok(nonempty(PathBuf::from(home))?.join(".cache/skywrite"))
}
fn nonempty(path: PathBuf) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        bail!("cache directory cannot be empty");
    }
    Ok(path)
}

/// Create a private cache directory, refusing symlinks at this path.
pub fn private_dir(path: &Path) -> Result<()> {
    make_private_dir(path).with_context(|| {
        format!(
            "cannot make cache directory private: {}; choose another directory with --cache-dir",
            path.display()
        )
    })
}

fn make_private_dir(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_dir() => {
            bail!(
                "cache directory must be a directory, not a symlink: {}",
                path.display()
            );
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(path)?;
        }
        Err(error) => return Err(error).context("checking cache directory"),
    }
    #[cfg(unix)]
    {
        use std::os::{
            fd::AsRawFd,
            unix::fs::{MetadataExt, OpenOptionsExt},
        };
        // Bind validation and chmod to the same directory, even if the path
        // is replaced after the initial metadata check.
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
            .context("opening cache directory without following symlinks")?;
        let metadata = directory.metadata()?;
        // SAFETY: getuid has no preconditions.
        if !metadata.is_dir() || metadata.uid() != unsafe { libc::getuid() } {
            bail!("cache directory must be a directory owned by the current user");
        }
        if metadata.mode() & 0o077 != 0 {
            // SAFETY: the owned File keeps this directory descriptor valid.
            if unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("making cache directory private");
            }
        }
    }
    #[cfg(not(unix))]
    if !std::fs::symlink_metadata(path)?.is_dir() {
        bail!("cache directory must not be a symlink: {}", path.display());
    }
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct FrameManifest {
    version: u32,
    source_sha256: String,
    parameters: serde_json::Value,
    frames: Vec<CachedFrame>,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedFrame {
    time: f64,
    sha256: String,
}

/// Serialize extraction per asset, publish complete immutable directories, and
/// verify hashes on reuse. Filenames come from code, never cached paths.
pub fn sampled_frames(
    info: &crate::media::MediaInfo,
    root: &Path,
    source_sha256: &str,
    count: usize,
    start: f64,
    duration: f64,
) -> Result<(Vec<crate::media::SampleFrame>, bool)> {
    if source_sha256.len() != 64 || !source_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid source SHA-256");
    }
    if !info.has_video() {
        return Ok((Vec::new(), false));
    }
    if !(1..=32).contains(&count) {
        bail!("frame count must be between 1 and 32");
    }
    let parameters = serde_json::json!({"version":1,"frames":count,"start":start,"duration":duration,"scale":"768:768:force_original_aspect_ratio=decrease","jpeg_quality":4});
    let encoded = serde_json::to_vec(&parameters)?;
    let key = crate::openrouter::cache_key(&[b"sample-frames-v1", &encoded]);
    let parent = root.join(source_sha256).join("frames");
    std::fs::create_dir_all(&parent)?;
    let parent = parent.canonicalize()?;
    let mut open = std::fs::OpenOptions::new();
    open.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let lock = open.open(parent.join(".extract.lock"))?;
    if !lock.metadata()?.is_file() {
        bail!("frame cache lock must be a regular file");
    }
    lock.lock().context("locking shared frame cache")?;
    let destination = parent.join(key);
    if destination.exists() {
        let path = destination.join("manifest.json");
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.len() > 64 * 1024 {
            bail!("invalid frame manifest; remove the affected cache entry");
        }
        let manifest: FrameManifest = crate::io::read_json(&path)?;
        if manifest.version != 1
            || manifest.source_sha256 != source_sha256
            || manifest.parameters != parameters
            || manifest.frames.len() != count
        {
            bail!("frame cache parameters do not match; remove the affected cache entry");
        }
        let frames = manifest
            .frames
            .iter()
            .enumerate()
            .map(|(i, frame)| {
                let path = destination.join(format!("frame-{i:03}.jpg"));
                let metadata = std::fs::symlink_metadata(&path)?;
                if !metadata.is_file()
                    || metadata.len() > 1024 * 1024
                    || !frame.time.is_finite()
                    || (frame.time
                        - (if info.duration == 0.0 {
                            0.0
                        } else {
                            start + duration * (i as f64 + 0.5) / count as f64
                        }))
                    .abs()
                        > 1e-8
                    || crate::media::file_sha256_limited(&path, 1024 * 1024)? != frame.sha256
                {
                    bail!("frame cache integrity check failed; remove the affected cache entry");
                }
                Ok(crate::media::SampleFrame {
                    time: frame.time,
                    path,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        return Ok((frames, true));
    }
    let run = tempfile::Builder::new()
        .prefix("extract-")
        .tempdir_in(&parent)?;
    let frames = crate::media::sample_frames(info, run.path(), count, start, duration)?;
    let records = frames
        .iter()
        .map(|f| {
            if std::fs::metadata(&f.path)?.len() > 1024 * 1024 {
                bail!("sample frame exceeds one MiB");
            }
            Ok(CachedFrame {
                time: f.time,
                sha256: crate::media::file_sha256_limited(&f.path, 1024 * 1024)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let manifest = FrameManifest {
        version: 1,
        source_sha256: source_sha256.into(),
        parameters,
        frames: records,
    };
    crate::io::write_json(&run.path().join("manifest.json"), &manifest, false)?;
    std::fs::rename(run.path(), &destination)?;
    Ok((
        frames
            .into_iter()
            .enumerate()
            .map(|(i, f)| crate::media::SampleFrame {
                time: f.time,
                path: destination.join(format!("frame-{i:03}.jpg")),
            })
            .collect(),
        false,
    ))
}

/// Cheap stat identity for a media file. Any change (including a same-size
/// rewrite, which updates ctime on Unix) forces a full re-hash.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct StatIdentity {
    path: PathBuf,
    size: u64,
    mtime_ns: i128,
    #[serde(default)]
    ctime_ns: Option<i128>,
    #[serde(default)]
    dev: Option<u64>,
    #[serde(default)]
    ino: Option<u64>,
}
fn stat_identity(path: &Path, metadata: &std::fs::Metadata) -> Result<StatIdentity> {
    let mtime = metadata.modified()?;
    let mtime_ns = match mtime.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    };
    #[cfg(unix)]
    let (ctime_ns, dev, ino) = {
        use std::os::unix::fs::MetadataExt;
        (
            Some(metadata.ctime() as i128 * 1_000_000_000 + metadata.ctime_nsec() as i128),
            Some(metadata.dev()),
            Some(metadata.ino()),
        )
    };
    #[cfg(not(unix))]
    let (ctime_ns, dev, ino) = (None, None, None);
    Ok(StatIdentity {
        path: path.to_owned(),
        size: metadata.len(),
        mtime_ns,
        ctime_ns,
        dev,
        ino,
    })
}
#[derive(serde::Serialize, serde::Deserialize)]
struct MediaHashEntry {
    version: u32,
    identity: StatIdentity,
    sha256: String,
}
/// Files modified (mtime or ctime) this recently are hashed but not cached: on coarse-timestamp
/// file systems a later write could keep the same mtime (the "racy" case).
const MEDIA_HASH_SETTLE: std::time::Duration = std::time::Duration::from_secs(2);

/// Content SHA-256 of a media file, cached under the user cache
/// (`media-sha256-v1/`) by canonical path and stat identity. A changed size,
/// mtime, ctime, device or inode re-hashes the file. Cache problems never
/// change the result: an unreadable or corrupt entry is a miss, and an
/// unwritable cache only costs the re-hash next time.
pub fn media_sha256(path: &Path, cache_override: Option<&Path>) -> Result<String> {
    Ok(media_sha256_with_status(path, cache_override)?.0)
}
/// As [`media_sha256`], also reporting whether the cache answered.
pub fn media_sha256_with_status(
    path: &Path,
    cache_override: Option<&Path>,
) -> Result<(String, bool)> {
    media_sha256_settled(path, cache_override, MEDIA_HASH_SETTLE)
}
fn media_sha256_settled(
    path: &Path,
    cache_override: Option<&Path>,
    settle: std::time::Duration,
) -> Result<(String, bool)> {
    let path = crate::media::canonical_file(path)?;
    let before = stat_identity(&path, &std::fs::metadata(&path)?)?;
    let entry_path = cache_root(cache_override).ok().and_then(|root| {
        let key = crate::openrouter::cache_key(&[
            b"media-sha256-v1",
            path.as_os_str().as_encoded_bytes(),
        ]);
        let directory = root.join("media-sha256-v1");
        private_dir(&directory).ok()?;
        Some(directory.join(format!("{key}.json")))
    });
    if let Some(entry_path) = &entry_path
        && let Ok(metadata) = std::fs::symlink_metadata(entry_path)
        && metadata.is_file()
        && metadata.len() <= 16 * 1024
        && let Ok(entry) = crate::io::read_json::<MediaHashEntry>(entry_path)
        && entry.version == 1
        && entry.identity == before
        && entry.sha256.len() == 64
        && entry.sha256.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Ok((entry.sha256.to_ascii_lowercase(), true));
    }
    let sha256 = crate::media::file_sha256(&path)?;
    let after = stat_identity(&path, &std::fs::metadata(&path)?)?;
    if after != before {
        bail!(
            "media {} changed while it was being hashed; try again once it is no longer being written",
            path.display()
        );
    }
    // Tools such as `cp -p` or `tar x` restore an old mtime on new bytes; the
    // later of mtime and ctime is when the content last could have changed.
    let changed_ns = after.mtime_ns.max(after.ctime_ns.unwrap_or(i128::MIN));
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i128);
    let settled = now_ns - changed_ns >= settle.as_nanos() as i128;
    if settled && let Some(entry_path) = &entry_path {
        let entry = MediaHashEntry {
            version: 1,
            identity: before,
            sha256: sha256.clone(),
        };
        let _ = crate::io::protect_inputs(entry_path, std::slice::from_ref(&path))
            .and_then(|()| crate::io::write_json(entry_path, &entry, true));
    }
    Ok((sha256, false))
}

#[cfg(test)]
mod media_hash_tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[cfg(unix)]
    #[test]
    fn already_private_directory_is_untouched() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        // Both modes are private; the read-only mode also proves that we do
        // not unnecessarily grant the owner write access.
        for mode in [0o700, 0o500] {
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode)).unwrap();
            let before = std::fs::metadata(dir.path()).unwrap();
            private_dir(dir.path()).unwrap();
            let after = std::fs::metadata(dir.path()).unwrap();
            assert_eq!(after.mode(), before.mode());
            assert_eq!(
                (after.ctime(), after.ctime_nsec()),
                (before.ctime(), before.ctime_nsec())
            );
        }
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn private_directory_refuses_symlink_without_changing_target() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = dir.path().join("link");
        symlink(&target, &link).unwrap();
        let error = format!("{:#}", private_dir(&link).unwrap_err());
        assert!(error.contains("symlink"), "{error}");
        assert!(error.contains(link.to_str().unwrap()), "{error}");
        assert!(error.contains("--cache-dir"), "{error}");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    /// ctime cannot be backdated, so these tests waive the settle window.
    fn lookup(media: &Path, cache: &Path) -> (String, bool) {
        media_sha256_settled(media, Some(cache), Duration::ZERO).unwrap()
    }

    fn aged(path: &Path) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
    }
    fn entry(cache: &Path, media: &Path) -> PathBuf {
        let media = media.canonicalize().unwrap();
        let key = crate::openrouter::cache_key(&[
            b"media-sha256-v1",
            media.as_os_str().as_encoded_bytes(),
        ]);
        cache.join("media-sha256-v1").join(format!("{key}.json"))
    }

    #[cfg(unix)]
    #[test]
    fn media_hash_cache_directories_and_entries_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let media = dir.path().join("a.wav");
        std::fs::write(&media, b"first").unwrap();
        lookup(&media, &cache);
        let path = entry(&cache, &media);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let parent = path.parent().unwrap();
        assert_eq!(
            std::fs::metadata(parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(lookup(&media, &cache).1);
        assert_eq!(
            std::fs::metadata(parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_media_hash_directory_is_ignored() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("a.wav");
        std::fs::write(&media, b"first").unwrap();
        let cache = dir.path().join("cache");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&cache).unwrap();
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, cache.join("media-sha256-v1")).unwrap();
        assert_eq!(
            lookup(&media, &cache),
            (crate::media::file_sha256(&media).unwrap(), false)
        );
        assert_eq!(std::fs::read_dir(outside).unwrap().count(), 0);
    }

    #[test]
    fn cache_hits_for_an_unchanged_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let media = dir.path().join("a.wav");
        std::fs::write(&media, b"first").unwrap();
        aged(&media);
        let expected = crate::media::file_sha256(&media).unwrap();
        let (hash, hit) = lookup(&media, &cache);
        assert_eq!((hash.as_str(), hit), (expected.as_str(), false));
        // Prove the second answer comes from the entry, not a re-hash.
        let path = entry(&cache, &media);
        let mut stored: serde_json::Value = crate::io::read_json(&path).unwrap();
        stored["sha256"] = serde_json::json!("ab".repeat(32));
        crate::io::write_json(&path, &stored, true).unwrap();
        let (hash, hit) = lookup(&media, &cache);
        assert_eq!((hash, hit), ("ab".repeat(32), true));
    }

    #[test]
    fn stat_change_invalidates_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let media = dir.path().join("a.wav");
        std::fs::write(&media, b"first").unwrap();
        aged(&media);
        lookup(&media, &cache);
        // Same size, restored mtime: only the content (and ctime) differ.
        #[cfg(unix)]
        {
            let mtime = std::fs::metadata(&media).unwrap().modified().unwrap();
            std::thread::sleep(Duration::from_millis(20));
            std::fs::write(&media, b"other").unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(&media)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
            let (hash, hit) = lookup(&media, &cache);
            assert!(!hit);
            assert_eq!(hash, crate::media::file_sha256(&media).unwrap());
        }
        // A size change is a miss too.
        std::fs::write(&media, b"longer content").unwrap();
        aged(&media);
        let (hash, hit) = lookup(&media, &cache);
        assert!(!hit);
        assert_eq!(hash, crate::media::file_sha256(&media).unwrap());
        assert!(lookup(&media, &cache).1);
    }

    #[test]
    fn corrupt_or_unwritable_cache_never_changes_the_hash() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let media = dir.path().join("a.wav");
        std::fs::write(&media, b"first").unwrap();
        aged(&media);
        let expected = crate::media::file_sha256(&media).unwrap();
        let path = entry(&cache, &media);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(lookup(&media, &cache), (expected.clone(), false));
        // A file where the cache directory should be: hashing still works.
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, b"").unwrap();
        assert_eq!(lookup(&media, &blocked), (expected, false));
    }

    #[test]
    fn fresh_files_are_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let media = dir.path().join("a.wav");
        std::fs::write(&media, b"first").unwrap();
        // An old mtime alone is not enough: ctime records the recent write.
        aged(&media);
        media_sha256_with_status(&media, Some(&cache)).unwrap();
        assert!(!entry(&cache, &media).exists());
    }
}
