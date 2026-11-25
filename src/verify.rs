//! Optional local technical checks, without artistic suitability claims.
use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;
#[derive(Debug, Args)]
pub struct VerifyArgs {
    pub input: PathBuf,
    #[arg(long)]
    pub metadata_only: bool,
    #[arg(long)]
    pub expected_duration: Option<f64>,
    #[arg(long, default_value_t = 0.05)]
    pub duration_tolerance: f64,
    /// Expected displayed width in pixels, from stream metadata (basis
    /// stream_metadata), with the display rotation applied: a phone clip stored
    /// 1920x1080 with rotation 90 or -90 is 1080 wide. A mismatch fails the check
    /// (nonzero exit, report still printed). Example: --expected-width 1080
    /// --expected-height 1920 for an upright vertical phone clip.
    #[arg(long, value_name = "PIXELS")]
    pub expected_width: Option<u32>,
    /// Expected displayed height in pixels (rotation applied, like
    /// --expected-width): a phone clip stored 1920x1080 with rotation 90 is 1920 high.
    #[arg(long, value_name = "PIXELS")]
    pub expected_height: Option<u32>,
    #[arg(long)]
    pub expected_fps: Option<f64>,
    #[arg(long, default_value_t = 0.01)]
    pub fps_tolerance: f64,
    /// Check that the first video and first audio streams end within --end-tolerance of each other.
    #[arg(long, conflicts_with = "metadata_only")]
    pub ends_together: bool,
    /// Largest allowed difference between the video and audio stream ends, in seconds.
    #[arg(long, default_value_t = 0.05, value_name = "SECONDS")]
    pub end_tolerance: f64,
    /// Measure integrated loudness (EBU R128, LUFS) and true peak (dBTP) of the first audio stream and check them against --target-lufs and --max-true-peak.
    #[arg(long, conflicts_with = "metadata_only")]
    pub loudness: bool,
    /// Integrated loudness target in LUFS (-70 to -5).
    #[arg(long, default_value_t = -16.0, value_name = "LUFS", allow_negative_numbers = true)]
    pub target_lufs: f64,
    /// Allowed distance from --target-lufs, in LU (0 to 10).
    #[arg(long, default_value_t = 1.0, value_name = "LU")]
    pub lufs_tolerance: f64,
    /// Highest allowed true peak in dBTP (-30 to 6).
    #[arg(long, default_value_t = -1.0, value_name = "DBTP", allow_negative_numbers = true)]
    pub max_true_peak: f64,
    /// Find black frames (FFmpeg blackdetect): a black first frame, and black spans of at least --black-min-seconds.
    #[arg(long, conflicts_with = "metadata_only")]
    pub black: bool,
    /// Shortest black span that fails the check, in seconds (0 to 60; 0 fails on any black frame).
    #[arg(long, default_value_t = DEFAULT_BLACK_MIN_SECONDS, value_name = "SECONDS")]
    pub black_min_seconds: f64,
    /// Luma below this fraction of full range counts as a black pixel (0 to 1); a frame is black when 98% of its pixels are.
    #[arg(long, default_value_t = 0.1, value_name = "FRACTION")]
    pub black_pixel_threshold: f64,
    /// Find frozen pictures (FFmpeg freezedetect): spans of at least --frozen-min-seconds where the picture does not change.
    #[arg(long, conflicts_with = "metadata_only")]
    pub frozen: bool,
    /// Shortest frozen span that fails the check, in seconds (0.1 to 3600).
    #[arg(long, default_value_t = 2.0, value_name = "SECONDS")]
    pub frozen_min_seconds: f64,
    /// Frame-to-frame difference below this level counts as no change, in dB (-90 to -10).
    #[arg(long, default_value_t = -60.0, value_name = "DB", allow_negative_numbers = true)]
    pub frozen_noise_db: f64,
    /// The edit JSON this file was rendered from. With --black and --frozen, black at the start or end that lies within the edit's own fade-in of its first clip or fade-out of its last clip, and frozen spans within a still-image clip, are reported as allowed instead of failing.
    #[arg(long, value_name = "EDIT.json")]
    pub edit: Option<PathBuf>,
    /// Slack around the edit's fades and stills for --edit, in seconds (0 to 1).
    #[arg(long, default_value_t = DEFAULT_EDIT_TOLERANCE_SECONDS, value_name = "SECONDS")]
    pub edit_tolerance: f64,
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    #[arg(long)]
    pub overwrite: bool,
}

/// Shortest black span that fails `--black`, in seconds.
pub const DEFAULT_BLACK_MIN_SECONDS: f64 = 0.5;
/// Slack around the edit's own fades and stills for `--edit`, in seconds:
/// three frames at 30 fps, so frame rounding and a decoder's first-frame
/// offset never turn an intended fade into a failure.
pub const DEFAULT_EDIT_TOLERANCE_SECONDS: f64 = 0.1;

/// Where the edit itself makes the picture black or still, on the file's
/// timeline (seconds from the container start).
#[derive(Debug, Default)]
struct Allowances {
    fade_in: Option<(f64, f64)>,
    fade_out: Option<(f64, f64)>,
    stills: Vec<(f64, f64)>,
    tolerance: f64,
}
impl Allowances {
    /// Only visible clips (opacity above zero) count: black is allowed at
    /// the start until the first visible clip there is fully faded in, and at
    /// the end from where the last visible clip there starts fading out; a
    /// still allows a freeze only while no other visible clip is on screen.
    fn from_edit(edit: &crate::timeline::ValidatedEdit, offset: f64, tolerance: f64) -> Self {
        let clips: Vec<_> = edit
            .video
            .iter()
            .flat_map(|t| &t.clips)
            .filter(|c| c.opacity > 0.0)
            .collect();
        let at = |frames: u64| offset + edit.fps.seconds(frames);
        let span =
            |c: &crate::timeline::ValidatedVideoClip| (at(c.start_frames), at(c.end_frames()));
        let first = clips.iter().map(|c| c.start_frames).min().map(at);
        let last = clips.iter().map(|c| c.end_frames()).max().map(at);
        let fade_in = first.and_then(|first| {
            let fade = clips
                .iter()
                .filter(|c| at(c.start_frames) == first)
                .map(|c| c.fade_in)
                .fold(0.0, f64::max);
            // Any visible clip on screen within the fade that is fully
            // visible earlier ends the window.
            let end = clips
                .iter()
                .filter(|c| span(c).0 < first + fade && span(c).1 > first)
                .map(|c| span(c).0 + c.fade_in)
                .fold(first + fade, f64::min);
            (end > first).then_some((first, end))
        });
        let fade_out = last.and_then(|last| {
            let fade = clips
                .iter()
                .filter(|c| at(c.end_frames()) == last)
                .map(|c| c.fade_out)
                .fold(0.0, f64::max);
            let start = clips
                .iter()
                .filter(|c| span(c).1 > last - fade && span(c).0 < last)
                .map(|c| span(c).1 - c.fade_out)
                .fold(last - fade, f64::max);
            (start < last).then_some((start, last))
        });
        let mut stills = Vec::new();
        for still in clips
            .iter()
            .filter(|c| c.kind == crate::timeline::MediaKind::Image)
        {
            let (from, to) = span(still);
            let mut others: Vec<(f64, f64)> = clips
                .iter()
                .filter(|c| !std::ptr::eq(**c, *still))
                .map(|c| span(c))
                .filter(|&(a, b)| a < to && b > from)
                .collect();
            others.sort_by(|a, b| a.0.total_cmp(&b.0));
            let mut cursor = from;
            for (a, b) in others {
                if a > cursor {
                    stills.push((cursor, a));
                }
                cursor = cursor.max(b);
            }
            if cursor < to {
                stills.push((cursor, to));
            }
        }
        Self {
            fade_in,
            fade_out,
            stills,
            tolerance,
        }
    }
    fn within(&self, (from, to): (f64, f64), start: f64, end: Option<f64>) -> bool {
        start + self.tolerance >= from && end.is_some_and(|end| end <= to + self.tolerance)
    }
    /// Why a black span is the edit's own fade, if it is.
    fn black(
        &self,
        start: f64,
        end: Option<f64>,
        at_start: bool,
        at_end: bool,
    ) -> Option<&'static str> {
        if at_start && self.fade_in.is_some_and(|w| self.within(w, start, end)) {
            Some("fade_in")
        } else if at_end && self.fade_out.is_some_and(|w| self.within(w, start, end)) {
            Some("fade_out")
        } else {
            None
        }
    }
    /// Why a frozen span is the edit's own still, if it is.
    fn frozen(&self, start: f64, end: Option<f64>) -> Option<&'static str> {
        self.stills
            .iter()
            .any(|&w| self.within(w, start, end))
            .then_some("still")
    }
    fn report(&self) -> Value {
        json!({"fade_in":self.fade_in.map(|(a, b)| [a, b]),"fade_out":self.fade_out.map(|(a, b)| [a, b]),
            "stills":self.stills.iter().map(|&(a, b)| [a, b]).collect::<Vec<_>>(),"tolerance_seconds":self.tolerance})
    }
}

pub const VERIFY_ABOUT: &str = "Technical checks of a media file, usually a finished render. You cannot watch or hear a render: run this after skywrite edit render, quote the measured numbers, and say which checks were not run (the report's not_measured list). Lint the edit before rendering with skywrite review lint.

By default it hashes the file, fully decodes the first audio and first video streams, and reports decoded sample and frame counts, audio peak and RMS (dBFS) and the decoded duration. Checks run only when requested; each adds an entry to checks with its measured values and passed. The command prints the JSON report and exits 1 if any requested check fails (the full report is still printed to stdout, with \"passed\": false).

Checks:
  --expected-duration S [--duration-tolerance 0.05]   decoded duration in seconds
  --expected-width/--expected-height/--expected-fps   stream metadata
  --ends-together [--end-tolerance 0.05]              picture and sound end together: video and audio stream ends (seconds from the container start, video from its last packets' timestamps plus durations, audio where its decoded samples or packets end, whichever is earlier) differ by at most the tolerance. Fails with a reason when the file lacks either stream. skywrite edit render output measured within 0.7 ms on FFmpeg 5.1.2 to 9.1; 0.05 s also allows other encoders' one-AAC-frame (21 ms) padding and catches two frames or more at 30 fps.
  --loudness [--target-lufs -16 --lufs-tolerance 1 --max-true-peak -1]   integrated loudness (EBU R128, LUFS) and true peak (dBTP) of the whole first audio stream, measured with FFmpeg loudnorm as skywrite audio normalize does. -16 LUFS / -1 dBTP suits online video and podcasts; use -14 for music streaming targets or -23 for EBU broadcast. Digital silence cannot be measured and fails.
  --black [--black-min-seconds 0.5 --black-pixel-threshold 0.1]   black frames (FFmpeg blackdetect): first_frame_black fails when the first frame is black (a late-starting picture or a black thumbnail); black_frames fails on any black span of at least --black-min-seconds, such as a gap between clips. Every span is listed with start_seconds, end_seconds, duration_seconds, at_start, at_end, allowed and fails. Fades through black leave short spans: up to 0.43 s for 1-s fades of bright footage, 0.77 s for dark footage, so 0.5 s passes ordinary dips; use 0 to fail on any black frame.
  --frozen [--frozen-min-seconds 2 --frozen-noise-db -60]   frozen pictures (FFmpeg freezedetect): spans where the picture does not change for at least --frozen-min-seconds. Each span has start_seconds, end_seconds, duration_seconds, allowed and fails. Intentional stills, title cards and slides are frozen too (pass --edit); spans inside black spans are not counted twice.
  --edit EDIT.json [--edit-tolerance 0.1]   the edit the file was rendered from, so its intended black and stills pass: a black span at the start that lies within the first clip's fade_in (allowed: fade_in, which also passes first_frame_black), a black span at the end within the last clip's fade_out (allowed: fade_out), and a frozen span within a still-image clip while no other clip is on screen (allowed: still), each with --edit-tolerance seconds of slack. Only visible clips (opacity above 0) count, and the fade-in window ends where any visible clip on screen is fully shown (the fade-out window likewise). A gap before the first clip, or black or a freeze longer than the fade or still, still fails. The edit is validated as skywrite edit validate does, so its sources must be present. Without --edit, not_measured says the edit's intent was not considered.

Times are seconds from the start of the file. --metadata-only skips decoding and cannot run the decode checks. Cost: one full decode per stream plus one loudness pass for --loudness; --black and --frozen add one more video decode (both together). No network.";

pub const VERIFY_EXAMPLES: &str = "Examples:
  skywrite verify out.mp4 --ends-together --loudness --black --frozen --edit edit.json
  skywrite verify out.mp4 --expected-duration 62.5 --loudness --target-lufs -14 --max-true-peak -1 --output out.verify.json";
#[derive(Debug)]
pub struct CheckFailed {
    pub report: Value,
}
impl std::fmt::Display for CheckFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "technical verification expectations failed")
    }
}
impl std::error::Error for CheckFailed {}
pub fn execute(args: VerifyArgs) -> Result<Value> {
    execute_with_limit(args, 16 * 1024 * 1024 * 1024)
}
fn execute_with_limit(args: VerifyArgs, hash_limit: u64) -> Result<Value> {
    if !args.duration_tolerance.is_finite()
        || args.duration_tolerance < 0.0
        || !args.fps_tolerance.is_finite()
        || args.fps_tolerance < 0.0
        || args
            .expected_duration
            .is_some_and(|x| !x.is_finite() || x <= 0.0)
        || args
            .expected_fps
            .is_some_and(|x| !x.is_finite() || x <= 0.0)
        || args.expected_width == Some(0)
        || args.expected_height == Some(0)
    {
        bail!(
            "expected dimensions/rates/duration must be positive; tolerances finite and nonnegative"
        );
    }
    for (name, value, min, max) in [
        ("--end-tolerance", args.end_tolerance, 0.0, 10.0),
        ("--target-lufs", args.target_lufs, -70.0, -5.0),
        ("--lufs-tolerance", args.lufs_tolerance, 0.0, 10.0),
        ("--max-true-peak", args.max_true_peak, -30.0, 6.0),
        ("--black-min-seconds", args.black_min_seconds, 0.0, 60.0),
        (
            "--black-pixel-threshold",
            args.black_pixel_threshold,
            0.0,
            1.0,
        ),
        ("--frozen-min-seconds", args.frozen_min_seconds, 0.1, 3600.0),
        ("--frozen-noise-db", args.frozen_noise_db, -90.0, -10.0),
        ("--edit-tolerance", args.edit_tolerance, 0.0, 1.0),
    ] {
        if !value.is_finite() || value < min || value > max {
            bail!("{name} must be between {min} and {max}");
        }
    }
    let edit = args
        .edit
        .as_deref()
        .map(crate::timeline::load_and_validate)
        .transpose()?;
    if let Some(path) = &args.output {
        // The verified file, and with --edit the edit and everything it reads.
        let mut inputs = vec![args.input.clone()];
        if let Some(edit) = &edit {
            inputs.extend(edit.protected_paths().map(PathBuf::from));
        }
        crate::io::protect_inputs(path, &inputs)?;
        if path.exists() && !args.overwrite {
            bail!("output exists; use --overwrite to replace it");
        }
    }
    let info = crate::media::probe(&args.input)?;
    #[cfg(test)]
    hash_boundary_tests::after_probe();
    if info.size_bytes > hash_limit {
        bail!("verification source exceeds 16 GiB work limit");
    }
    let hash = crate::media::file_sha256_limited(&info.path, info.size_bytes.min(hash_limit))?;
    let audio = if !args.metadata_only && info.has_audio() {
        Some(crate::timing::measure_audio(&info)?)
    } else {
        None
    };
    let detect = Detect {
        black: args
            .black
            .then_some((args.black_pixel_threshold, args.black_min_seconds)),
        frozen: args
            .frozen
            .then_some((args.frozen_noise_db, args.frozen_min_seconds)),
    };
    let (video, detections) = if !args.metadata_only && info.has_video() {
        let (video, detections) = decode_video(&info, &detect, video_start(&info))?;
        (Some(video), detections)
    } else {
        (None, Detections::default())
    };
    // Packet-measured ends; `None` (reported, never fatal) when the packets
    // cannot be read.
    let video_packets_end = video.as_ref().and_then(|_| stream_end(&info, "v:0"));
    let ends = match (&video_packets_end, &audio) {
        (Some(video_end), Some(audio)) => Some((
            video_end.clone(),
            stream_end(&info, "a:0"),
            audio.presentation_end_seconds,
        )),
        _ => None,
    };
    let loudness = if args.loudness && info.has_audio() {
        let deadline = std::time::Instant::now() + Duration::from_secs(600);
        Some(crate::audio::loudness_stats(
            &info.path, -16.0, -1.0, 11.0, deadline,
        )?)
    } else {
        None
    };
    if crate::media::file_sha256_limited(&info.path, info.size_bytes.min(hash_limit))? != hash {
        bail!("source changed during verification");
    }
    let measured_duration = match (&audio, &video) {
        (Some(a), Some(v)) => Some(
            a.presentation_end_seconds
                .max(v["presentation_end_seconds"].as_f64().unwrap()),
        ),
        (Some(a), None) => Some(a.presentation_end_seconds),
        (None, Some(v)) => v["presentation_end_seconds"].as_f64(),
        _ => None,
    };
    let mut checks = Vec::new();
    let duration = measured_duration.unwrap_or(info.duration);
    if let Some(expected) = args.expected_duration {
        checks.push(json!({"property":"duration","expected":expected,"actual":duration,"tolerance":args.duration_tolerance,"basis":if measured_duration.is_some(){"decoded_presentation"}else{"metadata_estimate"},"passed":(duration-expected).abs() <= args.duration_tolerance}));
    }
    let (width, height) = info
        .video()
        .map_or((None, None), |v| v.display_dimensions());
    for (name, expected, actual) in [
        ("width", args.expected_width, width),
        ("height", args.expected_height, height),
    ] {
        if let Some(expected) = expected {
            checks.push(json!({"property":name,"expected":expected,"actual":actual,"basis":"stream_metadata","passed":actual==Some(expected)}));
        }
    }
    if let Some(expected) = args.expected_fps {
        let actual = info
            .video()
            .and_then(|v| v.avg_frame_rate.as_deref())
            .and_then(parse_rate);
        checks.push(json!({"property":"fps","expected":expected,"actual":actual,"tolerance":args.fps_tolerance,"basis":"average_stream_metadata","passed":actual.is_some_and(|v|(v-expected).abs()<=args.fps_tolerance)}));
    }
    let stream_ends = ends.as_ref().map(|(video_end, audio_packets_end, audio_decoded_end)| {
        // Encoder padding only ever lengthens audio, and FFmpeg versions trim
        // it in different places (8.0 in packet timestamps, 9.1 in the
        // decoder), so the earlier end is where the sound stops.
        let audio_end = audio_packets_end
            .as_ref()
            .map_or(*audio_decoded_end, |packets| packets.end.min(*audio_decoded_end));
        let micro = |seconds: f64| (seconds * 1e6).round() / 1e6;
        let (video, audio_end) = (micro(video_end.end), micro(audio_end));
        json!({"video_end_seconds":video,"audio_end_seconds":audio_end,"difference_seconds":micro(video-audio_end),
            "video_end_basis":video_end.basis,
            "audio_packets_end_seconds":audio_packets_end.as_ref().map(|p| p.end),"audio_decoded_end_seconds":audio_decoded_end,
            "basis":"seconds from the container start: video from its last packets' timestamps plus durations (edit lists applied); audio the earlier of its packets' end and the end of its decoded samples, since encoder padding is trimmed in one or the other; positive difference means the picture runs longer"})
    });
    if args.ends_together {
        checks.push(match &stream_ends {
            Some(ends) => {
                let difference = ends["difference_seconds"].as_f64().unwrap_or(f64::INFINITY);
                json!({"property":"ends_together","video_end_seconds":ends["video_end_seconds"],"audio_end_seconds":ends["audio_end_seconds"],
                    "difference_seconds":difference,"tolerance":args.end_tolerance,"basis":ends["basis"],"passed":difference.abs() <= args.end_tolerance})
            }
            None if info.has_video() && info.has_audio() && !args.metadata_only => json!({"property":"ends_together","passed":false,"reason":"the video stream's packet timestamps could not be read"}),
            None => json!({"property":"ends_together","passed":false,"reason":"the file needs both a video and an audio stream"}),
        });
    }
    if args.loudness {
        checks.extend(loudness_checks(&args, loudness.as_ref()));
    }
    let video_start = video_start(&info);
    let video_end = video_packets_end.as_ref().map(|e| e.end).or(video
        .as_ref()
        .and_then(|v| v["presentation_end_seconds"].as_f64()));
    let last_frame_start = video_packets_end.as_ref().map(|e| e.last_start);
    let allowances = edit.as_ref().map_or_else(Allowances::default, |edit| {
        Allowances::from_edit(edit, video_start, args.edit_tolerance)
    });
    let no_video = |property: &str| json!({"property":property,"passed":false,"reason":"the file has no video stream"});
    if args.black {
        if video.is_some() {
            checks.extend(black_checks(
                &args,
                &detections,
                video_start,
                (last_frame_start, video_end),
                &allowances,
            ));
        } else {
            checks.extend([no_video("first_frame_black"), no_video("black_frames")]);
        }
    }
    if args.frozen {
        checks.push(if video.is_some() {
            frozen_check(
                &args,
                &detections,
                (last_frame_start, video_end),
                &allowances,
            )
        } else {
            no_video("frozen_frames")
        });
    }
    let mut not_measured = Vec::new();
    if args.metadata_only {
        not_measured.push("decoded audio and video (--metadata-only)");
    }
    for (requested, available, what) in [
        (
            args.ends_together,
            true,
            "picture and sound end together (--ends-together)",
        ),
        (
            args.loudness,
            info.has_audio(),
            "loudness and true peak (--loudness)",
        ),
        (args.black, info.has_video(), "black frames (--black)"),
        (args.frozen, info.has_video(), "frozen pictures (--frozen)"),
    ] {
        if !requested || !available {
            not_measured.push(what);
        }
    }
    if (args.black || args.frozen) && edit.is_none() {
        not_measured.push("which black or frozen spans the edit intends (pass --edit EDIT.json to allow its own fades and stills)");
    }
    if !info.has_audio() {
        not_measured.push("audio: the file has no audio stream");
    }
    if !info.has_video() {
        not_measured.push("video: the file has no video stream");
    }
    not_measured
        .push("creative quality, sync of particular words or events, and how the mix sounds");
    let passed = checks.iter().all(|c| c["passed"] == true);
    let report = json!({"version":1,"passed":passed,"source_sha256":hash,"media":info,"metadata_duration_basis":"estimate","selected_streams":"first audio and first video","full_decode_executed":!args.metadata_only,"decoded_duration_seconds":measured_duration,"audio":audio,"video":video,"stream_ends":stream_ends,"edit_allowances":edit.as_ref().map(|_| allowances.report()),"checks":checks,"not_measured":not_measured,"scope":"local technical evidence only; no creative, child-suitability, or NLE compatibility judgment"});
    if let Some(path) = &args.output {
        crate::io::write_json(path, &report, args.overwrite)?;
    }
    if !passed {
        return Err(CheckFailed { report }.into());
    }
    Ok(report)
}
/// The first video stream's start, in seconds from the container start.
fn video_start(info: &crate::media::MediaInfo) -> f64 {
    info.video()
        .and_then(|v| v.start_time)
        .map_or(0.0, |start| (start - info.start_time).max(0.0))
}

fn parse_rate(rate: &str) -> Option<f64> {
    let (n, d) = rate.split_once('/')?;
    let value = n.parse::<f64>().ok()? / d.parse::<f64>().ok()?;
    (value.is_finite() && value > 0.0).then_some(value)
}
/// Optional detectors that run in a second video decode.
#[derive(Default)]
struct Detect {
    /// blackdetect pixel threshold and the span length that fails.
    black: Option<(f64, f64)>,
    /// freezedetect noise (dB) and minimum duration (seconds).
    frozen: Option<(f64, f64)>,
}

/// Spans reported by the detectors, in seconds from the start of the file.
/// An end of `None` means the span lasts to the end of the video.
#[derive(Default)]
struct Detections {
    black: Vec<(f64, Option<f64>)>,
    frozen: Vec<(f64, Option<f64>)>,
}

impl Detections {
    fn offset(mut self, by: f64) -> Self {
        for span in self.black.iter_mut().chain(self.frozen.iter_mut()) {
            span.0 += by;
            span.1 = span.1.map(|end| end + by);
        }
        self
    }
}

/// The `key:value` number in a filter log line (blackdetect prints
/// `black_start:0`, freezedetect `lavfi.freezedetect.freeze_start: 0`).
fn log_value(line: &str, key: &str) -> Option<f64> {
    let rest = &line[line.find(key)? + key.len()..];
    rest.trim_start()
        .split(|c: char| c.is_whitespace())
        .next()?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}

/// Parse blackdetect and freezedetect output; identical on FFmpeg 5.1 to 9.1
/// except for the filter instance names, which are not used. Lines can share
/// a line with progress output on 5.1, so keys are found anywhere in a line.
fn parse_detections(log: &str) -> Detections {
    let mut detections = Detections::default();
    let mut open_freeze: Option<f64> = None;
    for line in log.lines() {
        if let Some(start) = log_value(line, "black_start:") {
            detections
                .black
                .push((start, log_value(line, "black_end:")));
        }
        if let Some(start) = log_value(line, "lavfi.freezedetect.freeze_start:")
            && let Some(previous) = open_freeze.replace(start)
        {
            detections.frozen.push((previous, None));
        }
        if let Some(end) = log_value(line, "lavfi.freezedetect.freeze_end:")
            && let Some(start) = open_freeze.take()
        {
            detections.frozen.push((start, Some(end)));
        }
    }
    if let Some(start) = open_freeze {
        detections.frozen.push((start, None));
    }
    detections
}

/// A stream end measured from packet timestamps.
#[derive(Clone)]
struct StreamEnd {
    end: f64,
    /// Presentation time of the last packet (the last frame's start).
    last_start: f64,
    basis: &'static str,
}

/// The end of the first stream matching `selector` (`v:0`, `a:0`): the
/// latest packet presentation time plus duration among the packets of the
/// stream's last 30 seconds, in seconds from the container start. A last
/// packet without a duration gets the measured spacing of the last two
/// packets. FFmpeg's progress `out_time` is not used because 6.x reports the
/// last frame's start and other versions its end. `None` when no packet
/// timestamps can be read.
fn stream_end(info: &crate::media::MediaInfo, selector: &str) -> Option<StreamEnd> {
    let stream = if selector.starts_with('v') {
        info.video()
    } else {
        info.audio()
    };
    let own_end = stream
        .and_then(|s| Some(s.start_time.unwrap_or(info.start_time) + s.duration?))
        .filter(|end| end.is_finite());
    let container_end = info.start_time + info.duration;
    // The stream's own end first: a short stream can end long before the container.
    let mut seeks = vec![
        own_end.unwrap_or(container_end) - 30.0,
        container_end - 30.0,
    ];
    seeks.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    seeks.into_iter().find_map(|seek| {
        let mut args = crate::process::args(&[
            "-v",
            "error",
            "-protocol_whitelist",
            "file,pipe",
            "-select_streams",
            selector,
            "-read_intervals",
            &format!("{:.6}%", seek.max(0.0)),
            "-show_entries",
            "packet=pts_time,duration_time",
            "-of",
            "csv=p=0",
        ]);
        args.push(info.path.clone().into_os_string());
        let out = crate::process::run("ffprobe", &args, Duration::from_secs(120), 8 * 1024 * 1024)
            .ok()?;
        let (last_start, end, basis) = packets_end(&String::from_utf8_lossy(&out.stdout))?;
        Some(StreamEnd {
            end: end - info.start_time,
            last_start: last_start - info.start_time,
            basis,
        })
    })
}

/// The last packet's start and its end (`pts_time + duration_time`) in
/// ffprobe's `csv=p=0` packet lines. Without a duration, the spacing of the
/// last two distinct timestamps is used; with fewer, the end is unknown.
fn packets_end(csv: &str) -> Option<(f64, f64, &'static str)> {
    let mut packets: Vec<(f64, Option<f64>)> = csv
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(',');
            let pts = fields
                .next()?
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())?;
            let duration = fields
                .next()
                .and_then(|d| d.trim().parse::<f64>().ok())
                .filter(|d| d.is_finite() && *d > 0.0);
            Some((pts, duration))
        })
        .collect();
    packets.sort_by(|a, b| a.0.total_cmp(&b.0));
    let &(last, duration) = packets.last()?;
    let (duration, basis) = match duration {
        Some(duration) => (duration, "last packets' timestamps plus durations"),
        None => {
            let previous = packets
                .iter()
                .rev()
                .map(|p| p.0)
                .find(|&pts| pts < last - 1e-9)?;
            (
                last - previous,
                "last packet's timestamp plus the measured spacing of the last two packets (the packets carry no durations)",
            )
        }
    };
    Some((last, last + duration, basis))
}

fn loudness_checks(args: &VerifyArgs, stats: Option<&Value>) -> Vec<Value> {
    let Some(stats) = stats else {
        return vec![
            json!({"property":"integrated_loudness","passed":false,"reason":"the file has no audio stream"}),
        ];
    };
    let number = |key: &str| {
        stats[key]
            .as_str()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite())
    };
    let basis = "EBU R128 over the whole first audio stream (FFmpeg loudnorm analysis, the measurement skywrite audio normalize uses)";
    let integrated = match number("input_i") {
        Some(lufs) => {
            json!({"property":"integrated_loudness","actual_lufs":lufs,"target_lufs":args.target_lufs,
            "tolerance_lu":args.lufs_tolerance,"loudness_range_lu":number("input_lra"),"basis":basis,
            "passed":(lufs-args.target_lufs).abs() <= args.lufs_tolerance + 1e-9})
        }
        None => {
            json!({"property":"integrated_loudness","actual_lufs":null,"target_lufs":args.target_lufs,"basis":basis,
            "passed":false,"reason":"not measurable: the audio is digital silence or too short to gate"})
        }
    };
    let peak = match number("input_tp") {
        Some(dbtp) => {
            json!({"property":"true_peak","actual_dbtp":dbtp,"max_dbtp":args.max_true_peak,"basis":basis,
            "passed":dbtp <= args.max_true_peak + 1e-9})
        }
        None => {
            json!({"property":"true_peak","actual_dbtp":null,"max_dbtp":args.max_true_peak,"basis":basis,
            "passed":true,"reason":"digital silence has no peak"})
        }
    };
    vec![integrated, peak]
}

/// Close a detector span at the stream end. The detectors see one white frame
/// after the last frame, so a span that lasts to the end ends at that padded
/// frame, after the last frame's start; a span stopped by a visible last frame
/// ends exactly at that frame's start. Returns the end and whether it is the
/// stream end. Without packet timestamps only open spans count as ending there.
fn close_at_end(
    end: Option<f64>,
    (last_start, video_end): (Option<f64>, Option<f64>),
) -> (Option<f64>, bool) {
    let at_end = match (end, last_start) {
        (None, _) => true,
        // Detector times are printed to 1 us; the padded frame starts a whole
        // frame (over 1 ms below 1000 fps) after the last frame.
        (Some(end), Some(last_start)) => end > last_start + 1e-4,
        (Some(_), None) => false,
    };
    (if at_end { video_end.or(end) } else { end }, at_end)
}

fn black_checks(
    args: &VerifyArgs,
    detections: &Detections,
    video_start: f64,
    ends: (Option<f64>, Option<f64>),
    allowances: &Allowances,
) -> Vec<Value> {
    let spans: Vec<Value> = detections
        .black
        .iter()
        .map(|&(start, end)| {
            let (end, at_end) = close_at_end(end, ends);
            let duration = end.map(|end| end - start);
            // The first frame starts at the stream's start time; detector
            // times are printed to 1 us.
            let at_start = start <= video_start + 1e-4;
            let allowed = allowances.black(start, end, at_start, at_end);
            json!({"start_seconds":start,"end_seconds":end,"duration_seconds":duration,
                "at_start":at_start,"at_end":at_end,"allowed":allowed,
                "fails":allowed.is_none() && duration.is_none_or(|d| d + 1e-6 >= args.black_min_seconds)})
        })
        .collect();
    let first = spans.iter().find(|s| s["at_start"] == true);
    let basis = format!(
        "FFmpeg blackdetect: a frame is black when 98% of its pixels have luma below {} of full range",
        args.black_pixel_threshold
    );
    vec![
        json!({"property":"first_frame_black","passed":first.is_none_or(|s| !s["allowed"].is_null()),
            "black_until_seconds":first.map(|s| s["end_seconds"].clone()),
            "allowed":first.map(|s| s["allowed"].clone()),"basis":basis}),
        json!({"property":"black_frames","min_seconds":args.black_min_seconds,
            "pixel_threshold":args.black_pixel_threshold,"spans":spans,"basis":basis,
            "passed":!spans.iter().any(|s| s["fails"] == true)}),
    ]
}

fn frozen_check(
    args: &VerifyArgs,
    detections: &Detections,
    ends: (Option<f64>, Option<f64>),
    allowances: &Allowances,
) -> Value {
    let black: Vec<(f64, f64)> = detections
        .black
        .iter()
        .map(|&(start, end)| (start, close_at_end(end, ends).0.unwrap_or(f64::INFINITY)))
        .collect();
    let mut inside_black = 0usize;
    let mut spans = Vec::new();
    for &(start, end) in &detections.frozen {
        let (end, _) = close_at_end(end, ends);
        // freezedetect runs slightly below the minimum (it measures to the
        // last frozen frame's start); the minimum applies to the full span.
        if end.is_some_and(|end| end - start + 1e-6 < args.frozen_min_seconds) {
            continue;
        }
        let finish = end.unwrap_or(f64::INFINITY);
        // A black span is a frozen picture too; report it once, as black, but
        // only when it lies inside the black span (to timestamp precision).
        if black
            .iter()
            .any(|&(b0, b1)| start + 1e-4 >= b0 && finish <= b1 + 1e-4)
        {
            inside_black += 1;
            continue;
        }
        let allowed = allowances.frozen(start, end);
        spans.push(json!({"start_seconds":start,"end_seconds":end,"duration_seconds":end.map(|e| e - start),
            "allowed":allowed,"fails":allowed.is_none()}));
    }
    json!({"property":"frozen_frames","min_seconds":args.frozen_min_seconds,"noise_db":args.frozen_noise_db,
        "spans":spans,"black_spans_not_repeated":inside_black,
        "basis":"FFmpeg freezedetect: no frame-to-frame change above the noise level for at least min_seconds",
        "passed":spans.iter().all(|s| s["fails"] == false)})
}

fn decode_video(
    info: &crate::media::MediaInfo,
    detect: &Detect,
    video_start: f64,
) -> Result<(Value, Detections)> {
    if info.size_bytes > 16 * 1024 * 1024 * 1024 {
        bail!("verification source exceeds 16 GiB work limit");
    }
    // Filter strings hold only validated numbers.
    let frame = info
        .video()
        .and_then(|v| v.avg_frame_rate.as_deref())
        .and_then(parse_rate)
        .map_or(0.0, |fps| 1.0 / fps);
    let mut filters = Vec::new();
    if let Some((pixel, _)) = detect.black {
        filters.push(format!("blackdetect=d=0:pic_th=0.98:pix_th={pixel:.6}"));
    }
    if let Some((noise, duration)) = detect.frozen {
        // freezedetect measures to the last frozen frame's start.
        let threshold = (duration - frame * 1.5).max(0.01);
        filters.push(format!("freezedetect=n={noise:.3}dB:d={threshold:.6}"));
    }
    let input = |loglevel: &str| {
        let mut args = crate::process::args(&[
            "-hide_banner",
            "-nostdin",
            "-loglevel",
            loglevel,
            "-xerror",
            "-threads",
            "1",
            "-protocol_whitelist",
            "file,pipe",
            "-i",
        ]);
        args.push(info.path.clone().into_os_string());
        args
    };
    let mut args = input("error");
    args.extend(crate::process::args(&[
        "-map",
        "0:v:0",
        "-an",
        "-sn",
        "-dn",
        "-threads",
        "1",
        "-t",
        "21601",
        "-fps_mode",
        "passthrough",
        "-f",
        "null",
        "-",
        "-progress",
        "pipe:1",
        "-stats_period",
        "60",
        "-nostats",
    ]));
    let out = crate::process::run("ffmpeg", &args, Duration::from_secs(600), 1024 * 1024)?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.lines().any(|l| l == "progress=end") {
        bail!("video decode did not complete");
    }
    let value = |key: &str| {
        stdout
            .lines()
            .filter_map(|l| l.strip_prefix(key))
            .filter_map(|v| v.trim().parse::<f64>().ok())
            .next_back()
    };
    let frames = value("frame=").context("video decode missing frame count")?;
    let end = value("out_time_us=").context("video decode missing presentation end")? / 1_000_000.0;
    if frames < 1.0 || !end.is_finite() || end <= 0.0 || end >= crate::timing::MAX_DECODE_SECONDS {
        bail!("video decode empty or exceeds six-hour work limit");
    }
    let mut detections = Detections::default();
    if !filters.is_empty() {
        // The detectors run in a second decode, from the first frame, with
        // one white frame after the last so spans that run to the end of the
        // stream close there. Their chain is the only output: FFmpeg 7.0 and
        // 7.1 abort on a padded branch ending in a nullsink beside a mapped
        // output, and with two outputs the progress end is not the picture's.
        let mut args = input("info");
        args.extend(crate::process::args(&[
            "-filter_complex_threads",
            "1",
            "-filter_complex",
            &format!(
                "[0:v:0]setpts=PTS-STARTPTS,tpad=stop=1:stop_mode=add:color=white,{}[detect]",
                filters.join(",")
            ),
            "-map",
            "[detect]",
            "-an",
            "-sn",
            "-dn",
            "-threads",
            "1",
            "-t",
            "21601",
            "-fps_mode",
            "passthrough",
            "-f",
            "null",
            "-",
            "-nostats",
        ]));
        let out = crate::process::run("ffmpeg", &args, Duration::from_secs(600), 1024 * 1024)?;
        // The detectors see times from the stream's first frame (FFmpeg's own
        // rebasing differs between containers: MP4 keeps the container
        // origin, MPEG-TS can start the stream at zero); report them from the
        // container start like the packet timestamps.
        detections = parse_detections(&String::from_utf8_lossy(&out.stderr)).offset(video_start);
    }
    Ok((
        json!({"full_decode_executed":true,"frames":frames as u64,"presentation_end_seconds":end,"decoder":crate::timing::decoder_identity()?}),
        detections,
    ))
}

#[cfg(test)]
mod detection_tests {
    use super::*;

    #[test]
    fn packet_ends_use_durations_or_the_measured_spacing() {
        let (last, end, basis) = packets_end("2.900000,0.033333\n2.966667,0.033333\n").unwrap();
        assert!((last - 2.966667).abs() < 1e-9 && (end - 3.0).abs() < 1e-6);
        assert!(basis.contains("durations"));
        // Matroska packets can lack durations; B-frames arrive out of order.
        let (_, end, basis) = packets_end("2.933000,N/A\n2.967000,N/A\n2.900000,N/A\n").unwrap();
        assert!((end - 3.001).abs() < 1e-6, "{end}");
        assert!(basis.contains("spacing"));
        // One timestamp without a duration has no measurable end.
        assert!(packets_end("2.967000,N/A\n").is_none());
        assert!(packets_end("2.967000,N/A\n2.967000,N/A\n").is_none());
        assert!(packets_end("").is_none());
        assert!(packets_end("N/A,N/A\n").is_none());
    }

    #[test]
    fn spans_close_at_the_stream_end_only_past_the_last_frame_start() {
        let ends = (Some(2.966667), Some(3.0));
        // Stopped by a visible last frame (even a 1-ms one): not the end.
        assert_eq!(close_at_end(Some(2.966667), ends), (Some(2.966667), false));
        // Closed by the padded frame after the last frame: the stream end.
        assert_eq!(close_at_end(Some(3.0), ends), (Some(3.0), true));
        assert_eq!(close_at_end(Some(2.967667), ends), (Some(3.0), true));
        assert_eq!(close_at_end(None, ends), (Some(3.0), true));
        assert_eq!(
            close_at_end(Some(2.9), (None, Some(3.0))),
            (Some(2.9), false)
        );
    }

    #[test]
    fn edit_allowances_hold_to_the_tolerance_and_no_further() {
        let a = Allowances {
            fade_in: Some((0.0, 1.0)),
            fade_out: Some((9.0, 10.0)),
            stills: vec![(2.0, 5.0)],
            tolerance: 0.1,
        };
        assert_eq!(a.black(0.0, Some(1.1), true, false), Some("fade_in"));
        assert_eq!(a.black(0.0, Some(1.11), true, false), None);
        // A span inside the fade window but not at the start is not the fade.
        assert_eq!(a.black(0.2, Some(0.8), false, false), None);
        assert_eq!(a.black(8.9, Some(10.0), false, true), Some("fade_out"));
        assert_eq!(a.black(8.89, Some(10.0), false, true), None);
        assert_eq!(a.black(9.5, None, false, true), None);
        assert!(a.within(a.stills[0], 1.9, Some(5.1)));
        assert!(!a.within(a.stills[0], 1.89, Some(5.0)));
        assert!(!a.within(a.stills[0], 2.0, Some(5.11)));
        assert_eq!(
            Allowances::default().black(0.0, Some(0.1), true, true),
            None
        );
    }

    #[test]
    fn detector_lines_parse_on_every_supported_ffmpeg() {
        // 5.1.2 can print a progress line and the detector on one line; 8.1+
        // names instances Parsed_*; freezes at the end have no freeze_end.
        let log = "frame=    1 fps=0.0 q=-0.0 size=N/A time=00:00:00.03 bitrate=N/A speed=6.01x    [blackdetect @ 0x7] black_start:0 black_end:0.5 black_duration:0.5
[Parsed_freezedetect_1 @ 0x7] lavfi.freezedetect.freeze_start: 0.966667
[Parsed_freezedetect_1 @ 0x7] lavfi.freezedetect.freeze_duration: 1.033333
[Parsed_freezedetect_1 @ 0x7] lavfi.freezedetect.freeze_end: 2
[Parsed_blackdetect_0 @ 0x7] black_start:2 black_end:2.966667 black_duration:0.966667
[freezedetect @ 0x7] lavfi.freezedetect.freeze_start: 2.5
";
        let d = parse_detections(log);
        assert_eq!(d.black, vec![(0.0, Some(0.5)), (2.0, Some(2.966667))]);
        assert_eq!(d.frozen, vec![(0.966667, Some(2.0)), (2.5, None)]);
    }
}

#[cfg(test)]
mod hash_boundary_tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::Write;
    thread_local! { static AFTER_PROBE: RefCell<Option<Box<dyn FnOnce()>>> = const {RefCell::new(None)}; }
    pub(super) fn after_probe() {
        let callback = AFTER_PROBE.with(|s| s.borrow_mut().take());
        if let Some(f) = callback {
            f();
        }
    }
    #[test]
    fn metadata_only_verify_enforces_hash_cap_after_real_probe() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.wav");
        let mut args = crate::process::args(&[
            "-v",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=48000:cl=mono",
            "-t",
            "0.01",
        ]);
        args.push(path.clone().into_os_string());
        crate::process::run("ffmpeg", &args, Duration::from_secs(10), 65536).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() < 4096);
        let changed = path.clone();
        AFTER_PROBE.with(|s| {
            *s.borrow_mut() = Some(Box::new(move || {
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(changed)
                    .unwrap()
                    .write_all(&[42; 16384])
                    .unwrap();
            }))
        });
        crate::media::hash_tests::READ_BYTES.with(|n| n.set(0));
        let error = execute_with_limit(
            VerifyArgs {
                input: path,
                metadata_only: true,
                expected_duration: None,
                duration_tolerance: 0.05,
                expected_width: None,
                expected_height: None,
                expected_fps: None,
                fps_tolerance: 0.01,
                ends_together: false,
                end_tolerance: 0.05,
                loudness: false,
                target_lufs: -16.0,
                lufs_tolerance: 1.0,
                max_true_peak: -1.0,
                black: false,
                black_min_seconds: DEFAULT_BLACK_MIN_SECONDS,
                black_pixel_threshold: 0.1,
                frozen: false,
                frozen_min_seconds: 2.0,
                frozen_noise_db: -60.0,
                edit: None,
                edit_tolerance: DEFAULT_EDIT_TOLERANCE_SECONDS,
                output: None,
                overwrite: false,
            },
            4096,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("admitted byte limit"), "{error}");
        assert_eq!(
            crate::media::hash_tests::READ_BYTES.with(|n| n.get()),
            0,
            "re-probed metadata cap must precede any hash read"
        );
    }
}
