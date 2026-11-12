//! Bounded first-audio decode evidence, cached by source content and decoder.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
pub const MAX_DECODE_SECONDS: f64 = 21_600.0;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioTiming {
    pub version: u32,
    pub source_sha256: String,
    pub decoder: String,
    pub stream_index: u32,
    pub samples: u64,
    pub sample_rate: u32,
    pub decoded_duration_seconds: f64,
    pub presentation_end_seconds: f64,
    /// None means negative infinity (digital silence).
    pub peak_dbfs: Option<f64>,
    pub rms_dbfs: Option<f64>,
    pub cache_hit: bool,
}
pub fn decoder_identity() -> Result<String> {
    let out = crate::process::run(
        "ffmpeg",
        &crate::process::args(&["-version"]),
        Duration::from_secs(10),
        64 * 1024,
    )?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
fn source_hash(info: &crate::media::MediaInfo) -> Result<String> {
    crate::media::file_sha256_limited(&info.path, 16 * 1024 * 1024 * 1024)
        .context("exact timing source exceeds 16 GiB work limit or changed during hashing")
}
pub fn measure_audio(info: &crate::media::MediaInfo) -> Result<AudioTiming> {
    let info = crate::media::probe(&info.path)?;
    decode_audio(&info, source_hash(&info)?, decoder_identity()?)
}
fn valid(t: &AudioTiming, info: &crate::media::MediaInfo, hash: &str, decoder: &str) -> bool {
    t.version == 1
        && t.source_sha256 == hash
        && t.decoder == decoder
        && info
            .audio()
            .is_some_and(|a| a.index == t.stream_index && a.sample_rate == Some(t.sample_rate))
        && t.sample_rate > 0
        && t.samples > 0
        && t.decoded_duration_seconds.is_finite()
        && (t.decoded_duration_seconds - t.samples as f64 / t.sample_rate as f64).abs() < 1e-9
        && t.decoded_duration_seconds > 0.0
        && t.decoded_duration_seconds < MAX_DECODE_SECONDS
        && t.presentation_end_seconds.is_finite()
        && t.presentation_end_seconds > 0.0
        && t.presentation_end_seconds < MAX_DECODE_SECONDS
        && t.peak_dbfs.is_none_or(f64::is_finite)
        && t.rms_dbfs.is_none_or(f64::is_finite)
}
// Metadata supplements the content hashes: coarse filesystem timestamps can
// miss same-size rewrites, so a warm lookup must still recheck the content.
#[derive(PartialEq, Eq)]
struct SourceStamp {
    size: u64,
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}
impl SourceStamp {
    fn read(path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(path)?;
        Ok(Self {
            size: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            #[cfg(unix)]
            identity: {
                use std::os::unix::fs::MetadataExt;
                (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                )
            },
        })
    }
    fn check(&self, path: &Path) -> Result<()> {
        if *self != Self::read(path)? {
            bail!("source changed while reading timing evidence");
        }
        Ok(())
    }
}
pub fn exact_audio_timing(
    info: &crate::media::MediaInfo,
    cache_override: Option<&Path>,
) -> Result<AudioTiming> {
    let stamp = SourceStamp::read(&info.path)?;
    let refreshed = crate::media::probe(&info.path)?;
    let info = &refreshed;
    if !info.has_audio() {
        bail!("media has no audio stream");
    }
    let hash = source_hash(info)?;
    let decoder = decoder_identity()?;
    let key =
        crate::openrouter::cache_key(&[b"audio-timing-v1", hash.as_bytes(), decoder.as_bytes()]);
    let root = crate::cache::cache_root(cache_override)?.join("audio-timing-v1");
    std::fs::create_dir_all(&root)?;
    let root = root.canonicalize()?;
    let path = root.join(format!("{key}.json"));
    let lock_path = root.join(format!("{key}.lock"));
    crate::io::protect_inputs(&path, std::slice::from_ref(&info.path))?;
    crate::io::protect_inputs(&lock_path, std::slice::from_ref(&info.path))?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let lock = options.open(lock_path)?;
    if !lock.metadata()?.is_file() {
        bail!("timing cache lock must be a regular file");
    }
    // One nonnested asset lock; bounded wait for another process's decode.
    let deadline = std::time::Instant::now() + Duration::from_secs(610);
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25))
            }
            Err(error) => bail!("locking audio timing cache: {error}"),
        }
    }
    if let Ok(metadata) = std::fs::symlink_metadata(&path)
        && metadata.is_file()
        && metadata.len() <= 16 * 1024
        && let Ok(mut cached) = crate::io::read_json::<AudioTiming>(&path)
        && valid(&cached, info, &hash, &decoder)
    {
        stamp.check(&info.path)?;
        if source_hash(info)? != hash {
            bail!("source changed while reading timing evidence");
        }
        cached.cache_hit = true;
        return Ok(cached);
    }
    stamp.check(&info.path)?;
    let measured = decode_audio(info, hash, decoder)?;
    crate::io::write_json(&path, &measured, true)?;
    Ok(measured)
}
fn decode_audio(
    info: &crate::media::MediaInfo,
    hash: String,
    decoder: String,
) -> Result<AudioTiming> {
    let audio = info.audio().context("media has no audio stream")?;
    let sample_rate = audio
        .sample_rate
        .filter(|r| *r > 0)
        .context("audio has no sample rate")?;
    let mut args = crate::process::args(&[
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "info",
        "-xerror",
        "-protocol_whitelist",
        "file,pipe",
        "-i",
    ]);
    args.push(crate::media::canonical_file(&info.path)?.into_os_string());
    args.extend(crate::process::args(&[
        "-map", "0:a:0", "-vn", "-sn", "-dn", "-threads", "1", "-af",
        "astats=metadata=0:reset=0:measure_perchannel=none:measure_overall=Number_of_samples+Peak_level+RMS_level",
        "-t", "21601", "-f", "null", "-", "-progress", "pipe:1", "-stats_period", "60", "-nostats",
    ]));
    let output = crate::process::run("ffmpeg", &args, Duration::from_secs(600), 1024 * 1024)?;
    let progress = String::from_utf8_lossy(&output.stdout);
    if !progress.lines().any(|line| line == "progress=end") {
        bail!("audio decode did not complete");
    }
    let end = progress
        .lines()
        .filter_map(|line| line.strip_prefix("out_time_us="))
        .filter_map(|v| v.parse::<f64>().ok())
        .next_back()
        .context("decode has no end timestamp")?
        / 1_000_000.0;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stat = |name: &str| -> Result<f64> {
        stderr
            .lines()
            .filter_map(|line| line.split_once(name).map(|(_, v)| v.trim()))
            .filter_map(|v| v.parse::<f64>().ok())
            .next_back()
            .with_context(|| format!("audio decode missing {name}"))
    };
    let samples = stat("Number of samples:")?;
    if !samples.is_finite() || samples < 1.0 || samples.fract() != 0.0 || samples > u64::MAX as f64
    {
        bail!("invalid decoded sample count");
    }
    let peak = stat("Peak level dB:")?;
    let rms = stat("RMS level dB:")?;
    if peak.is_nan() || peak == f64::INFINITY || rms.is_nan() || rms == f64::INFINITY {
        bail!("nonfinite decoded audio levels");
    }
    let timing = AudioTiming {
        version: 1,
        source_sha256: hash,
        decoder,
        stream_index: audio.index,
        samples: samples as u64,
        sample_rate,
        decoded_duration_seconds: samples / sample_rate as f64,
        // FFmpeg 7+ reports the end of the last frame as out_time; 5.1-6.x
        // report its start (0 for a one-frame file), so never end before the
        // decoded samples do, counted from the stream's offset in the file.
        presentation_end_seconds: end.max(
            audio
                .start_time
                .map_or(0.0, |start| (start - info.start_time).max(0.0))
                + samples / sample_rate as f64,
        ),
        peak_dbfs: peak.is_finite().then_some(peak),
        rms_dbfs: rms.is_finite().then_some(rms),
        cache_hit: false,
    };
    if !valid(&timing, info, &timing.source_sha256, &timing.decoder) {
        bail!("audio timing is invalid or exceeds six-hour decode limit");
    }
    if source_hash(info)? != timing.source_sha256 {
        bail!("source changed during audio decode");
    }
    Ok(timing)
}
/// Audio-only consumers require decoded duration; video tails retain metadata duration.
pub fn resolve_audio_only_timing(
    mut info: crate::media::MediaInfo,
) -> Result<crate::media::MediaInfo> {
    if info.has_audio() && !info.has_video() {
        info.duration = exact_audio_timing(&info, None)?.presentation_end_seconds;
    }
    Ok(info)
}
pub fn probe_for_timing(path: &Path) -> Result<crate::media::MediaInfo> {
    resolve_audio_only_timing(crate::media::probe(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warm_timing_rechecks_content_and_matches_full_file_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.wav");
        crate::process::run(
            "ffmpeg",
            &crate::process::args(&[
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=sample_rate=8000:duration=0.1",
                source.to_str().unwrap(),
            ]),
            Duration::from_secs(10),
            1024 * 1024,
        )
        .unwrap();
        let info = crate::media::probe(&source).unwrap();
        exact_audio_timing(&info, Some(dir.path())).unwrap();
        let expected = crate::media::file_sha256(&source).unwrap();
        crate::media::hash_tests::READ_BYTES.with(|bytes| bytes.set(0));
        let cached = exact_audio_timing(&info, Some(dir.path())).unwrap();
        assert!(cached.cache_hit);
        assert_eq!(cached.source_sha256, expected);
        crate::media::hash_tests::READ_BYTES.with(|bytes| {
            assert_eq!(bytes.get(), 2 * std::fs::metadata(&source).unwrap().len());
        });
    }

    #[cfg(unix)]
    #[test]
    fn timing_source_stamp_rejects_same_length_replacement_with_same_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::write(&source, b"before").unwrap();
        let stamp = SourceStamp::read(&source).unwrap();
        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, b"after!").unwrap();
        std::fs::File::open(&replacement)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(stamp.modified.unwrap()))
            .unwrap();
        std::fs::rename(replacement, &source).unwrap();
        assert!(
            stamp
                .check(&source)
                .unwrap_err()
                .to_string()
                .contains("source changed")
        );
    }
}
