pub use crate::timing::{
    AudioTiming, exact_audio_timing, probe_for_timing, resolve_audio_only_timing,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// These demuxers can follow playlists, manifests, scripts, session descriptions,
/// or sidecar files. The file protocol whitelist alone cannot confine those
/// references to the selected input, so they are not accepted as media sources.
const REJECTED_DEMUXERS: &[&str] = &[
    "hls",
    "dash",
    "imf",
    "concat",
    "webm_dash_manifest",
    "mlv",
    "sdp",
    "rtsp",
    "rtp",
    "sap", // SAP announcements supply SDP descriptions of external RTP streams.
    "avisynth",
    "vapoursynth",
    "dvdvideo",
    "bluray",
    "vobsub", // An IDX subtitle index automatically opens its SUB sidecar.
];

/// libavdevice inputs; FFmpeg before 7 lists them as ordinary demuxers.
const INPUT_DEVICES: &[&str] = &[
    "alsa",
    "android_camera",
    "avfoundation",
    "bktr",
    "decklink",
    "dshow",
    "fbdev",
    "gdigrab",
    "iec61883",
    "jack",
    "kmsgrab",
    "lavfi",
    "libcdio",
    "libdc1394",
    "openal",
    "oss",
    "pulse",
    "sndio",
    "video4linux2",
    "v4l2",
    "vfwcap",
    "x11grab",
    "xcbgrab",
];

fn rejected_container(name: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "unsupported media container '{name}': playlist and manifest formats are not accepted as sources; use the underlying media files"
    )
}

fn reject_external_reference_demuxer(format_name: &str) -> Result<()> {
    for name in format_name.split(',') {
        if REJECTED_DEMUXERS.contains(&name) {
            return Err(rejected_container(name));
        }
    }
    Ok(())
}

/// Parse `ffprobe -demuxers` into a `-format_whitelist` value: every file
/// demuxer this FFmpeg build has, except devices and [`REJECTED_DEMUXERS`].
/// FFmpeg enforces the whitelist after identifying the input and before its
/// demuxer reads the header, and passes it on to nested demuxers, so a rejected
/// playlist never opens the files it names.
///
/// FFmpeg 7+ prints a three-column flag legend ending in `---` and marks
/// devices with `d`; FFmpeg 5.1–6.x prints two columns ending in `--` and does
/// not mark devices, so the known input devices are also dropped by name.
fn demuxer_whitelist(listing: &str) -> Option<String> {
    let mut names = Vec::new();
    for line in listing
        .lines()
        .skip_while(|line| {
            let line = line.trim();
            !(line.len() >= 2 && line.bytes().all(|b| b == b'-'))
        })
        .skip(1)
    {
        let mut tokens = line.split_whitespace();
        let Some(flags) = tokens.next() else {
            continue;
        };
        if !flags.contains('D') || !flags.chars().all(|c| matches!(c, 'D' | 'E' | 'd' | '.')) {
            continue;
        }
        let mut device = flags.contains('d');
        let mut name = tokens.next();
        if name == Some("d") {
            device = true;
            name = tokens.next();
        }
        let Some(name) = name else {
            continue;
        };
        if device {
            continue;
        }
        names.extend(
            name.split(',')
                .filter(|part| {
                    !part.is_empty()
                        && !REJECTED_DEMUXERS.contains(part)
                        && !INPUT_DEVICES.contains(part)
                })
                .map(str::to_owned),
        );
    }
    (!names.is_empty()).then(|| names.join(","))
}

fn format_whitelist() -> Result<&'static str> {
    static WHITELIST: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if let Some(list) = WHITELIST.get() {
        return Ok(list);
    }
    let out = crate::process::run(
        "ffprobe",
        &crate::process::args(&["-hide_banner", "-demuxers"]),
        Duration::from_secs(10),
        1024 * 1024,
    )?;
    let list = demuxer_whitelist(&String::from_utf8_lossy(&out.stdout))
        .context("ffprobe -demuxers listed no demuxers")?;
    Ok(WHITELIST.get_or_init(|| list))
}

/// Name the demuxer FFmpeg refused because it is not on the whitelist.
fn whitelist_refusal(error: &anyhow::Error) -> Option<String> {
    let message = format!("{error:#}");
    let at = message.find("Format not on whitelist")?;
    let prefix = &message[..at];
    let start = prefix.rfind('[')? + 1;
    let name = prefix[start..].split(" @ ").next()?.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaInfo {
    pub path: PathBuf,
    pub duration: f64,
    pub start_time: f64,
    pub size_bytes: u64,
    pub format_name: String,
    pub streams: Vec<MediaStream>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaStream {
    pub index: u32,
    pub codec_type: String,
    pub codec_name: Option<String>,
    pub duration: Option<f64>,
    pub start_time: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
    pub avg_frame_rate: Option<String>,
    pub r_frame_rate: Option<String>,
    pub time_base: Option<String>,
}
impl MediaInfo {
    pub fn has_video(&self) -> bool {
        self.streams.iter().any(|s| s.codec_type == "video")
    }
    pub fn has_audio(&self) -> bool {
        self.streams.iter().any(|s| s.codec_type == "audio")
    }
    pub fn video(&self) -> Option<&MediaStream> {
        self.streams.iter().find(|s| s.codec_type == "video")
    }
    pub fn audio(&self) -> Option<&MediaStream> {
        self.streams.iter().find(|s| s.codec_type == "audio")
    }
}
/// Accept regular local files; canonical absolute paths cannot become flags or URLs.
pub fn canonical_file(path: &Path) -> Result<PathBuf> {
    let meta =
        std::fs::metadata(path).with_context(|| format!("reading media {}", path.display()))?;
    if !meta.is_file() {
        bail!(
            "media input must be a regular local file: {}",
            path.display()
        );
    }
    path.canonicalize()
        .with_context(|| format!("resolving {}", path.display()))
}
pub fn probe(path: &Path) -> Result<MediaInfo> {
    let path = canonical_file(path)?;
    let mut args = crate::process::args(&[
        "-v",
        "error",
        "-protocol_whitelist",
        "file,pipe",
        "-format_whitelist",
        format_whitelist()?,
        "-show_format",
        "-show_streams",
        "-of",
        "json",
    ]);
    args.push(path.clone().into_os_string());
    let out = crate::process::run("ffprobe", &args, Duration::from_secs(30), 8 * 1024 * 1024)
        .map_err(|error| match whitelist_refusal(&error) {
            Some(name) => rejected_container(&name),
            None => error,
        })?;
    let raw: Value =
        serde_json::from_slice(&out.stdout).context("ffprobe returned invalid JSON")?;
    let format = &raw["format"];
    let format_name = format["format_name"].as_str().unwrap_or("unknown");
    reject_external_reference_demuxer(format_name)?;
    // image2 expands printf-style patterns, so a file literally named
    // `frame%03d.png` would decode other files than the one fingerprinted.
    if format_name.split(',').any(|name| name == "image2")
        && path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().contains('%'))
    {
        bail!(
            "image file names containing '%' are not accepted, because FFmpeg reads them as numbered sequences; rename {}",
            path.display()
        );
    }
    let streams = raw["streams"]
        .as_array()
        .context("ffprobe returned no streams")?
        .iter()
        .map(|s| MediaStream {
            index: s["index"].as_u64().unwrap_or(0) as u32,
            codec_type: s["codec_type"].as_str().unwrap_or("unknown").to_owned(),
            codec_name: string(s, "codec_name"),
            duration: number(s, "duration"),
            start_time: number(s, "start_time"),
            width: s["width"].as_u64().map(|v| v as u32),
            height: s["height"].as_u64().map(|v| v as u32),
            sample_rate: s["sample_rate"].as_str().and_then(|v| v.parse().ok()),
            channels: s["channels"].as_u64().map(|v| v as u32),
            avg_frame_rate: string(s, "avg_frame_rate"),
            r_frame_rate: string(s, "r_frame_rate"),
            time_base: string(s, "time_base"),
        })
        .collect::<Vec<_>>();
    if streams.is_empty() {
        bail!("media contains no streams");
    }
    let start_time = number(format, "start_time").unwrap_or(0.0);
    // FFmpeg's Matroska/WebM demuxer exposes Segment Duration as an ending
    // presentation timestamp (the muxer includes a nonzero timestamp origin).
    // MOV/MP4 and MPEG-TS instead expose elapsed format duration. Never apply
    // the Matroska correction to those demuxers. This uses header metadata only;
    // inspection must not scan/decode the complete source.
    let matroska = format_name
        .split(',')
        .any(|name| name == "matroska" || name == "webm");
    let duration = number(format, "duration")
        .map(|duration| {
            if matroska {
                duration - start_time
            } else {
                duration
            }
        })
        .or_else(|| {
            streams
                .iter()
                .filter_map(|stream| {
                    stream.duration.map(|duration| {
                        stream.start_time.unwrap_or(start_time) + duration - start_time
                    })
                })
                .max_by(f64::total_cmp)
        })
        .unwrap_or(0.0);
    if !duration.is_finite() || duration < 0.0 {
        bail!("media has an invalid duration");
    }
    Ok(MediaInfo {
        path: path.clone(),
        duration,
        start_time,
        size_bytes: std::fs::metadata(&path)?.len(),
        format_name: format_name.to_owned(),
        streams,
    })
}
fn string(value: &Value, key: &str) -> Option<String> {
    value[key].as_str().map(str::to_owned)
}
fn number(value: &Value, key: &str) -> Option<f64> {
    value[key]
        .as_f64()
        .or_else(|| value[key].as_str().and_then(|s| s.parse().ok()))
        .filter(|n| n.is_finite())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SampleFrame {
    pub time: f64,
    pub path: PathBuf,
}
/// Sample uniformly through a selected range; manifest timestamps use source seconds.
pub fn sample_frames(
    info: &MediaInfo,
    output_dir: &Path,
    count: usize,
    start: f64,
    duration: f64,
) -> Result<Vec<SampleFrame>> {
    if !info.has_video() {
        return Ok(Vec::new());
    }
    if !(1..=32).contains(&count) {
        bail!("frame count must be between 1 and 32");
    }
    validate_range(info, start, duration)?;
    std::fs::create_dir_all(output_dir)?;
    let output_dir = output_dir.canonicalize()?;
    let mut result = Vec::new();
    for i in 0..count {
        let time = if info.duration == 0.0 {
            0.0
        } else {
            start + duration * (i as f64 + 0.5) / count as f64
        };
        let dest = output_dir.join(format!("frame-{i:03}.jpg"));
        let mut args = crate::process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-n",
            "-protocol_whitelist",
            "file,pipe",
            "-i",
        ]);
        if !info.format_name.split(',').any(|name| name == "mpegts") {
            args.splice(5..5, crate::process::args(&["-ss", &time.to_string()]));
        }
        args.push(info.path.clone().into_os_string());
        if info.format_name.split(',').any(|name| name == "mpegts") {
            args.extend(crate::process::args(&["-ss", &time.to_string()]));
        }
        args.extend(crate::process::args(&[
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-vf",
            "scale=768:768:force_original_aspect_ratio=decrease",
            "-q:v",
            "4",
        ]));
        args.push(dest.clone().into_os_string());
        crate::process::run("ffmpeg", &args, Duration::from_secs(60), 1024 * 1024)?;
        if !dest.is_file() {
            bail!("no frame decoded at {time}s");
        }
        result.push(SampleFrame { time, path: dest });
    }
    Ok(result)
}
fn validate_range_arguments(start: f64, duration: f64) -> Result<()> {
    if !start.is_finite() || start < 0.0 || !duration.is_finite() || duration <= 0.0 {
        bail!("start must be finite/nonnegative and duration finite/positive");
    }
    Ok(())
}
pub fn validate_range(info: &MediaInfo, start: f64, duration: f64) -> Result<()> {
    validate_range_arguments(start, duration)?;
    let source_duration = if info.has_audio() && !info.has_video() {
        exact_audio_timing(info, None)?.presentation_end_seconds
    } else {
        info.duration
    };
    validate_range_with_duration(source_duration, start, duration)
}
/// Validate against a duration already resolved by the caller.
pub fn validate_range_with_duration(source_duration: f64, start: f64, duration: f64) -> Result<()> {
    validate_range_arguments(start, duration)?;
    if source_duration > 0.0 && start + duration > source_duration + 0.02 {
        bail!(
            "requested range exceeds media duration ({}s)",
            source_duration
        );
    }
    Ok(())
}
pub fn extract_audio(info: &MediaInfo, output: &Path, start: f64, duration: f64) -> Result<()> {
    if !info.has_audio() {
        bail!("media has no audio stream");
    }
    validate_range(info, start, duration)?;
    extract_audio_inner(info, output, start, duration)
}
/// Extract a subrange of a range the caller already checked with
/// [`validate_range`] on the same `info`, skipping the repeated exact-duration
/// probe. Cheap argument checks still apply.
pub(crate) fn extract_audio_unchecked(
    info: &MediaInfo,
    output: &Path,
    start: f64,
    duration: f64,
) -> Result<()> {
    if !info.has_audio() {
        bail!("media has no audio stream");
    }
    validate_range_arguments(start, duration)?;
    extract_audio_inner(info, output, start, duration)
}
fn extract_audio_inner(info: &MediaInfo, output: &Path, start: f64, duration: f64) -> Result<()> {
    let mut args = crate::process::args(&[
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-n",
        "-copyts",
        "-start_at_zero",
        "-ss",
        &start.to_string(),
        "-protocol_whitelist",
        "file,pipe",
        "-i",
    ]);
    args.push(info.path.clone().into_os_string());
    args.extend(crate::process::args(&[
        "-map",
        "0:a:0",
        "-vn",
        "-af",
        &format!("asetpts=PTS-{start}/TB,aresample=16000:async=0:first_pts=0"),
        "-t",
        &duration.to_string(),
        "-ac",
        "1",
        "-ar",
        "16000",
        "-c:a",
        "pcm_s16le",
    ]));
    args.push(output.into());
    crate::process::run("ffmpeg", &args, Duration::from_secs(120), 1024 * 1024)?;
    Ok(())
}

/// Content identity for a stable regular file. Work is bounded to its opened
/// size plus one growth-detection byte, without an arbitrary large-media cap.
pub fn file_sha256(path: &Path) -> Result<String> {
    file_hash(path, None)
}
/// Hash a regular file only within a caller's explicit byte admission policy.
pub fn file_sha256_limited(path: &Path, max_bytes: u64) -> Result<String> {
    file_hash(path, Some(max_bytes))
}
fn file_hash(path: &Path, max_bytes: Option<u64>) -> Result<String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("content hashing requires a regular local file");
    }
    let size = metadata.len();
    if max_bytes.is_some_and(|max| size > max) {
        bail!("content hashing source exceeds admitted byte limit");
    }
    #[cfg(test)]
    hash_tests::after_metadata();
    let (hash, bytes) = hash_reader_bounded(&file, size)?;
    if bytes != size || file.metadata()?.len() != size {
        bail!("source changed during content hashing");
    }
    Ok(hash)
}
/// The common actual-I/O boundary, including one sentinel byte to detect growth.
/// The reader may be a file or already admitted stream; no overflow byte is hashed.
pub(crate) fn hash_reader_bounded(reader: impl std::io::Read, limit: u64) -> Result<(String, u64)> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let sentinel = limit
        .checked_add(1)
        .context("content hash limit is too large")?;
    let mut reader = reader.take(sentinel);
    let mut hash = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        #[cfg(test)]
        hash_tests::READ_BYTES.with(|bytes| bytes.set(bytes.get() + count as u64));
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > limit {
            bail!("content hashing source exceeds admitted byte limit");
        }
        hash.update(&buffer[..count]);
    }
    Ok((
        hash.finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        total,
    ))
}

#[cfg(test)]
pub(crate) mod hash_tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::io::Write;
    thread_local! {
        pub(crate) static READ_BYTES: Cell<u64> = const { Cell::new(0) };
        static AFTER_METADATA: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    }
    pub(crate) fn after_metadata() {
        let callback = AFTER_METADATA.with(|slot| slot.borrow_mut().take());
        if let Some(callback) = callback {
            callback();
        }
    }
    #[test]
    fn stable_hash_limits_empty_exact_and_unrestricted_large_inverse() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("input");
        std::fs::write(&path, b"abc").unwrap();
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(file_sha256(&path).unwrap(), expected);
        assert_eq!(file_sha256_limited(&path, 3).unwrap(), expected);
        #[cfg(unix)]
        {
            let link = temp.path().join("allowed-link");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert_eq!(
                file_sha256(&link).unwrap(),
                expected,
                "generic media symlinks retain existing semantics"
            );
        }
        READ_BYTES.with(|n| n.set(0));
        assert!(
            file_sha256_limited(&path, 2)
                .unwrap_err()
                .to_string()
                .contains("admitted byte limit")
        );
        assert_eq!(
            READ_BYTES.with(Cell::get),
            0,
            "oversize fd metadata precedes payload reads"
        );
        std::fs::write(&path, []).unwrap();
        assert_eq!(
            file_sha256_limited(&path, 0).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        std::fs::write(&path, vec![42; 1024 * 1024]).unwrap();
        assert!(file_sha256_limited(&path, 16).is_err());
        READ_BYTES.with(|n| n.set(0));
        let digest = file_sha256(&path).unwrap();
        assert_eq!(
            digest,
            crate::generation::bytes_sha256(&vec![42; 1024 * 1024])
        );
        assert_eq!(
            READ_BYTES.with(Cell::get),
            1024 * 1024,
            "default has no arbitrary small-media cap"
        );
    }
    #[test]
    fn actual_file_growth_and_shrink_after_metadata_are_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("growing");
        for max in [None, Some(16)] {
            std::fs::write(&path, b"abcdefgh").unwrap();
            let changed = path.clone();
            AFTER_METADATA.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    std::fs::OpenOptions::new()
                        .append(true)
                        .open(changed)
                        .unwrap()
                        .write_all(&vec![42; 2 * 1024 * 1024])
                        .unwrap();
                }))
            });
            READ_BYTES.with(|n| n.set(0));
            let error = file_hash(&path, max).unwrap_err().to_string();
            assert!(error.contains("admitted byte limit"), "{error}");
            assert_eq!(
                READ_BYTES.with(Cell::get),
                9,
                "only initial8 plus sentinel consumed"
            );
        }
        std::fs::write(&path, b"abcdefgh").unwrap();
        let changed = path.clone();
        AFTER_METADATA.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::File::options()
                    .write(true)
                    .open(changed)
                    .unwrap()
                    .set_len(3)
                    .unwrap();
            }))
        });
        assert!(
            file_sha256(&path)
                .unwrap_err()
                .to_string()
                .contains("source changed")
        );
    }
    #[cfg(unix)]
    #[test]
    fn nonregular_fifo_rejects_without_waiting_for_writer() {
        use std::os::unix::ffi::OsStrExt;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("fifo");
        let raw = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: owned scratch pathname, ordinary owner-only FIFO mode.
        assert_eq!(unsafe { libc::mkfifo(raw.as_ptr(), 0o600) }, 0);
        let start = std::time::Instant::now();
        assert!(
            file_sha256(&path)
                .unwrap_err()
                .to_string()
                .contains("regular local file")
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }
    #[test]
    fn reader_growth_actual_sentinel_and_impossible_limit() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"small").unwrap();
        assert_eq!(file.metadata().unwrap().len(), 5);
        file.write_all(&[42; 128]).unwrap();
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(0)).unwrap();
        READ_BYTES.with(|n| n.set(0));
        assert!(
            hash_reader_bounded(file, 16)
                .unwrap_err()
                .to_string()
                .contains("admitted byte limit")
        );
        assert_eq!(READ_BYTES.with(Cell::get), 17);
        assert!(
            hash_reader_bounded(&b""[..], u64::MAX)
                .unwrap_err()
                .to_string()
                .contains("limit is too large")
        );
    }
}
#[cfg(test)]
mod extract_tests {
    use super::*;
    #[test]
    fn unchecked_extraction_matches_checked_bytes_and_keeps_argument_checks() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("tone.wav");
        let output = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=3",
            ])
            .arg(&source)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let info = probe(&source).unwrap();
        let checked = dir.path().join("checked.wav");
        let unchecked = dir.path().join("unchecked.wav");
        extract_audio(&info, &checked, 0.5, 1.25).unwrap();
        extract_audio_unchecked(&info, &unchecked, 0.5, 1.25).unwrap();
        assert_eq!(
            std::fs::read(&checked).unwrap(),
            std::fs::read(&unchecked).unwrap()
        );
        for (start, duration) in [(-1.0, 1.0), (0.0, 0.0), (f64::NAN, 1.0)] {
            assert!(
                extract_audio_unchecked(&info, &dir.path().join("bad.wav"), start, duration)
                    .unwrap_err()
                    .to_string()
                    .contains("start must be finite")
            );
        }
    }
}

#[cfg(test)]
mod demuxer_tests {
    use super::*;

    #[test]
    fn external_reference_demuxers_are_rejected_in_any_component() {
        for format_name in ["hls", "wav,concat", "concat,wav", "wav,concat,mp3"] {
            let error = reject_external_reference_demuxer(format_name)
                .unwrap_err()
                .to_string();
            assert!(error.contains("unsupported media container"), "{error}");
            assert!(error.contains("use the underlying media files"), "{error}");
        }
        for name in REJECTED_DEMUXERS {
            let error = reject_external_reference_demuxer(name)
                .unwrap_err()
                .to_string();
            assert!(error.contains(&format!("'{name}'")), "{error}");
        }
        for format_name in [
            "mov,mp4,m4a,3gp,3g2,mj2",
            "matroska,webm",
            "wav",
            "mp3",
            "flac",
            "ogg",
            "mpegts",
            "avi",
            "image2",
            "png_pipe",
            "jpeg_pipe",
            "webp_pipe",
            "gif",
        ] {
            reject_external_reference_demuxer(format_name).unwrap();
        }
    }

    #[test]
    fn whitelist_keeps_file_demuxers_and_drops_devices_and_rejected_formats() {
        let listing = "Formats:\n D.. = Demuxing supported\n .E. = Muxing supported\n ..d = Is a device\n ---\n D   3dostr          3DO STR\n D d alsa            ALSA audio input\n D   concat          Virtual concatenation script\n D   dash            Dynamic Adaptive Streaming over HTTP\n D   hls             Apple HTTP Live Streaming\n D   image2          image2 sequence\n D d lavfi           Libavfilter virtual input device\n D   matroska,webm   Matroska / WebM\n D   mov,mp4,m4a,3gp,3g2,mj2 QuickTime / MOV\n D   wav             WAV / WAVE (Waveform Audio)\n";
        let list = demuxer_whitelist(listing).unwrap();
        assert_eq!(
            list,
            "3dostr,image2,matroska,webm,mov,mp4,m4a,3gp,3g2,mj2,wav"
        );
        // Older listings have two flag columns and no device column.
        assert_eq!(
            demuxer_whitelist(" ---\n D  dash  DASH\n D  data  raw data\n DE wav   WAV\n").unwrap(),
            "data,wav"
        );
        assert!(demuxer_whitelist("no table").is_none());
        // FFmpeg 5.1-6.x: two flag columns, `--` separator, unmarked devices.
        let legacy = "File formats:\n D. = Demuxing supported\n .E = Muxing supported\n --\n D  3dostr          3DO STR\n D  alsa            ALSA audio input\n D  hls             Apple HTTP Live Streaming\n D  lavfi           Libavfilter virtual input device\n D  matroska,webm   Matroska / WebM\n DE wav             WAV / WAVE\n D  video4linux2,v4l2 Video4Linux2 device grab\n";
        assert_eq!(
            demuxer_whitelist(legacy).unwrap(),
            "3dostr,matroska,webm,wav"
        );
    }

    #[test]
    fn whitelist_refusal_names_the_demuxer() {
        let error = anyhow::anyhow!(
            "ffprobe exited with exit status: 1: [hls @ 0x5b66f101c840] Format not on whitelist '3dostr,wav'"
        );
        assert_eq!(whitelist_refusal(&error).as_deref(), Some("hls"));
        assert!(whitelist_refusal(&anyhow::anyhow!("ffprobe exited: Invalid data")).is_none());
    }
}
