//! Place a measured sync result into an edit: `skywrite sync apply`.
//!
//! The asset (normally a separate recorder) is placed against every chosen
//! reference clip (normally the camera) so agents never do the placement
//! arithmetic themselves: start on the output frame grid, sub-frame remainder
//! in `source_in`, `speed` from the clock-drift estimate, duration trimmed to
//! the real overlap, one clip per reference clip and sync segment.
use crate::stretch::StretcherChoice;
use crate::sync::{SyncOptions, SyncResult, SyncSegment};
use crate::timeline::{self, AudioClip, AudioTrack, Edit, Mix, Rational, Role};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Clips shorter than this are not re-synced by `--verify` (a strong re-sync
/// needs a drift estimate: anchors spanning 10 s).
const VERIFY_MIN_SECONDS: f64 = 15.0;
/// Reference sound decoded around each verified clip, each side.
const VERIFY_MARGIN_SECONDS: f64 = 10.0;
/// `--verify` passes when the render lands within this of the reference at
/// both ends of every verified clip.
const VERIFY_LIMIT_SECONDS: f64 = 0.002;
/// Placing more than this outside the matched anchors is reported.
const EXTRAPOLATION_WARN_SECONDS: f64 = 10.0;

#[derive(Debug, Clone, clap::Args)]
pub struct ApplyArgs {
    /// Sync result JSON from `skywrite sync` (reference = the camera on the timeline).
    #[arg(long)]
    pub sync: PathBuf,
    /// Edit JSON to read; it is not modified.
    #[arg(long)]
    pub edit: PathBuf,
    /// Reference clip(s) to place the asset against, as TRACK:CLIP (video
    /// track name or index, clip index; `audio:TRACK:CLIP` for an audio
    /// track). Default: every clip whose source is the sync reference.
    #[arg(long, value_name = "TRACK:CLIP")]
    pub clip: Vec<String>,
    /// Audio track for the placed clips; created (after the existing audio
    /// tracks) when no track has this name. Default: "<asset file name> (synced)".
    #[arg(long)]
    pub track: Option<String>,
    /// Role of a newly created track.
    #[arg(long, value_enum, default_value_t = Role::Dialogue)]
    pub role: Role,
    /// Leave the reference clips' own sound audible (default: muted).
    #[arg(long)]
    pub keep_reference_audio: bool,
    /// Place a result that is not strong, has no drift estimate or uses the
    /// 50 ms envelope method. The output then carries warnings; check by ear.
    #[arg(long)]
    pub allow_limited: bool,
    /// Render each placed clip, re-sync the render against the matching
    /// stretch of the reference and report the residual offset and drift.
    /// Passes when every verified clip re-syncs as one strong mapping covering
    /// it, within 2 ms at both ends. Clips under 15 s, or against a reference
    /// clip not within 3% of speed 1, are skipped.
    #[arg(long)]
    pub verify: bool,
    /// Edit JSON to write, in the same directory as --edit (relative sources
    /// resolve against it).
    #[arg(short, long)]
    pub output: PathBuf,
    #[arg(long)]
    pub overwrite: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Video,
    Audio,
}

#[derive(Debug, Clone, Serialize)]
struct ReferenceClip {
    kind: Kind,
    track: usize,
    clip: usize,
    label: String,
    start: f64,
    end: f64,
    source_in: f64,
    speed: f64,
}

#[derive(Debug, Clone, Serialize)]
struct Placement {
    reference_clip: String,
    segment: usize,
    start: f64,
    source_in: f64,
    duration: f64,
    speed: f64,
    /// Output seconds covered.
    timeline_range: [f64; 2],
    /// Asset seconds consumed.
    asset_range: [f64; 2],
    /// Asset seconds placed outside the segment's matched anchors.
    extrapolated_seconds: f64,
    #[serde(skip)]
    reference: ReferenceClip,
}

fn parse_clip(spec: &str, edit: &Edit) -> Result<(Kind, usize, usize)> {
    let (kind, rest) = match spec.strip_prefix("audio:") {
        Some(rest) => (Kind::Audio, rest),
        None => (Kind::Video, spec.strip_prefix("video:").unwrap_or(spec)),
    };
    let (track, clip) = rest
        .rsplit_once(':')
        .with_context(|| format!("--clip {spec}: expected TRACK:CLIP, for example 0:0 or V1:2"))?;
    let clip: usize = clip
        .parse()
        .with_context(|| format!("--clip {spec}: clip index must be a number"))?;
    let names: Vec<Option<&str>> = match kind {
        Kind::Video => edit.video.iter().map(|t| t.name.as_deref()).collect(),
        Kind::Audio => edit.audio.iter().map(|t| t.name.as_deref()).collect(),
    };
    let track = match track.parse::<usize>() {
        Ok(index) => index,
        Err(_) => {
            let found: Vec<usize> = (0..names.len())
                .filter(|&i| names[i] == Some(track))
                .collect();
            match found.as_slice() {
                [index] => *index,
                [] => bail!("--clip {spec}: no {kind:?} track named {track:?}"),
                _ => bail!("--clip {spec}: several tracks are named {track:?}; use its index"),
            }
        }
    };
    ensure!(
        track < names.len(),
        "--clip {spec}: the edit has {} {kind:?} track(s)",
        names.len()
    );
    Ok((kind, track, clip))
}

fn gate(
    result: &SyncResult,
    segments: &[usize],
    allow: bool,
    warnings: &mut Vec<String>,
) -> Result<()> {
    ensure!(
        result.version == 1
            && result.mapping == "reference_time = offset_seconds + scale * asset_time",
        "unsupported sync result: expected version 1 with mapping \"reference_time = offset_seconds + scale * asset_time\""
    );
    let mut problems = Vec::new();
    if result.method == "audio_envelope" {
        problems.push("method audio_envelope has only 50 ms resolution".to_string());
    }
    for &i in segments {
        let s = &result.segments[i];
        let name = if result.segments.len() > 1 {
            format!("segment {i}")
        } else {
            "the result".into()
        };
        if !s.drift_estimated {
            problems.push(format!(
                "{name} has no drift estimate (anchors span {:.1} s; the speed is assumed to be 1)",
                s.anchor_span_seconds
            ));
        }
        if s.quality != "strong" {
            problems.push(format!(
                "{name} quality is {} ({} inliers, p95 error {:.2} ms; strong needs at least 16 and under {:.2} ms)",
                s.quality,
                s.inliers,
                s.p95_error_seconds * 1e3,
                result.strong_p95_limit_seconds * 1e3
            ));
        }
    }
    if result.segments.len() == 1 && result.quality != "strong" && problems.is_empty() {
        problems.push(format!("the result quality is {}", result.quality));
    }
    if problems.is_empty() {
        return Ok(());
    }
    let summary = problems.join("; ");
    if allow {
        warnings.push(format!(
            "placed a result that is not strong because of --allow-limited: {summary}. Listen at the start, middle and end of each clip"
        ));
        return Ok(());
    }
    bail!(
        "refusing to place this sync result: {summary}. Next: read the warnings in the sync result and `skywrite skills read synchronization` (Low confidence or failure): use a longer or cleaner overlap (at least about 20 s of shared sound) and sync again, or sync a distinctive excerpt. Tell the user what was measured. --allow-limited places it anyway"
    )
}

/// Asset path as written into the edit: relative when inside the edit's directory.
fn edit_path(asset: &Path, base: &Path) -> PathBuf {
    asset
        .strip_prefix(base)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| asset.to_path_buf())
}

/// Frames within this fraction of a frame of a boundary count as on it, so
/// float noise in a mapping never moves a placement by a whole frame.
const FRAME_SNAP: f64 = 1e-6;
fn ceil_frame(fps: Rational, seconds: f64) -> u64 {
    (seconds * fps.value() - FRAME_SNAP).ceil().max(0.0) as u64
}
fn floor_frame(fps: Rational, seconds: f64) -> u64 {
    (seconds * fps.value() + FRAME_SNAP).floor().max(0.0) as u64
}

/// Place one segment against one reference clip, or `None` without overlap.
fn place(
    fps: Rational,
    reference: &ReferenceClip,
    segment: &SyncSegment,
    index: usize,
    asset_duration: f64,
) -> Option<Placement> {
    let (o, k, sv) = (segment.offset_seconds, segment.scale, reference.speed);
    let lo = segment.asset_range[0].max(0.0);
    let hi = segment.asset_range[1].min(asset_duration);
    if hi <= lo {
        return None;
    }
    // asset a -> reference r = o + k a -> timeline t = start + (r - source_in) / speed
    let timeline = |a: f64| reference.start + (o + k * a - reference.source_in) / sv;
    let t0 = reference.start.max(timeline(lo));
    let t1 = reference.end.min(timeline(hi));
    let first = ceil_frame(fps, t0);
    let last = floor_frame(fps, t1);
    if last <= first {
        return None;
    }
    let start = fps.seconds(first);
    let end = fps.seconds(last);
    let source_in = (reference.source_in + sv * (start - reference.start) - o) / k;
    // `first` may sit up to FRAME_SNAP of a frame before timeline(lo), which
    // puts source_in that far (in asset time) before the asset's start; that is
    // the snap, not a missing overlap. A 9.1-decoded scratch track gave an
    // offset of 18.000000004 s at 24 fps: start 18 s, source_in -4e-9 s.
    let snap = FRAME_SNAP / fps.value() * sv / k + 1e-12;
    if source_in < -snap {
        return None;
    }
    let source_in = source_in.max(0.0);
    let speed = if sv == 1.0 {
        segment.playback_rate
    } else {
        sv * segment.playback_rate
    };
    let duration = end - start;
    let asset_end = source_in + duration * speed;
    let extrapolated = (segment.asset_interval[0] - source_in).max(0.0)
        + (asset_end - segment.asset_interval[1]).max(0.0);
    Some(Placement {
        reference_clip: reference.label.clone(),
        segment: index,
        start,
        source_in,
        duration,
        speed,
        timeline_range: [start, end],
        asset_range: [source_in, asset_end],
        extrapolated_seconds: extrapolated,
        reference: reference.clone(),
    })
}

pub fn apply(args: &ApplyArgs) -> Result<Value> {
    ensure!(
        args.overwrite || !args.output.exists(),
        "output exists; use --overwrite to replace it"
    );
    let result: SyncResult = crate::io::read_json(&args.sync)
        .with_context(|| format!("reading sync result {}", args.sync.display()))?;
    ensure!(!result.segments.is_empty(), "sync result has no segments");
    let edit_dir = args
        .edit
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .canonicalize()
        .context("resolving the edit's directory")?;
    let output_dir = args
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .canonicalize()
        .context("resolving the output's directory")?;
    ensure!(
        edit_dir == output_dir,
        "write the output edit next to --edit ({}): relative sources resolve against the edit's directory",
        edit_dir.display()
    );
    let sync_dir = args
        .sync
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let resolve = |p: &Path| {
        let path = if p.is_absolute() {
            p.to_path_buf()
        } else {
            sync_dir.join(p)
        };
        crate::media::canonical_file(&path).with_context(|| {
            format!(
                "sync result names {}, which is not a readable file here; sync the media files themselves",
                p.display()
            )
        })
    };
    let reference_path = resolve(&result.reference)?;
    let asset_path = resolve(&result.asset)?;
    let mut inputs = vec![
        args.edit.clone(),
        args.sync.clone(),
        reference_path.clone(),
        asset_path.clone(),
    ];
    let value: Value = crate::io::read_json(&args.edit)?;
    let mut edit = timeline::parse(value)?;
    let validated = timeline::validate(&edit, &edit_dir)?;
    inputs.extend(validated.protected_paths().map(Path::to_path_buf));
    crate::io::protect_inputs(&args.output, &inputs)?;
    let fps = validated.fps();
    let asset_info = crate::media::probe(&asset_path)?;
    ensure!(
        asset_info.has_audio(),
        "{} has no audio stream",
        asset_path.display()
    );

    // Reference clips: explicit --clip, else every clip of the sync reference.
    let mut references = Vec::new();
    let wanted: Option<Vec<(Kind, usize, usize)>> = if args.clip.is_empty() {
        None
    } else {
        Some(
            args.clip
                .iter()
                .map(|spec| parse_clip(spec, &edit))
                .collect::<Result<_>>()?,
        )
    };
    for clip in validated.video_clips() {
        let label = format!("video:{}:{}", clip.track, clip.index);
        if wanted
            .as_ref()
            .is_some_and(|w| !w.contains(&(Kind::Video, clip.track, clip.index)))
        {
            continue;
        }
        if clip.source != reference_path {
            if wanted.is_some() {
                bail!(
                    "{label} uses {}, but the sync reference is {}",
                    clip.source.display(),
                    reference_path.display()
                );
            }
            continue;
        }
        references.push(ReferenceClip {
            kind: Kind::Video,
            track: clip.track,
            clip: clip.index,
            label,
            start: fps.seconds(clip.start_frames),
            end: fps.seconds(clip.end_frames()),
            source_in: clip.source_in,
            speed: clip.speed,
        });
    }
    for clip in validated.audio_clips() {
        let label = format!("audio:{}:{}", clip.track, clip.index);
        if wanted
            .as_ref()
            .is_some_and(|w| !w.contains(&(Kind::Audio, clip.track, clip.index)))
        {
            continue;
        }
        if clip.source != reference_path {
            if wanted.is_some() {
                bail!(
                    "{label} uses {}, but the sync reference is {}",
                    clip.source.display(),
                    reference_path.display()
                );
            }
            continue;
        }
        references.push(ReferenceClip {
            kind: Kind::Audio,
            track: clip.track,
            clip: clip.index,
            label,
            start: fps.seconds(clip.start_frames),
            end: fps.seconds(clip.end_frames()),
            source_in: clip.source_in,
            speed: clip.speed,
        });
    }
    if let Some(wanted) = &wanted {
        ensure!(
            references.len() == wanted.len(),
            "--clip names a clip the edit does not have"
        );
    }
    ensure!(
        !references.is_empty(),
        "no clip in the edit uses the sync reference {}; put it on the timeline first, or pass --clip",
        reference_path.display()
    );

    let mut warnings = Vec::new();
    let asset_duration = asset_info.duration;
    let mut placements = Vec::new();
    for reference in &references {
        if reference.speed != 1.0 {
            warnings.push(format!(
                "{} plays at speed {}: the asset follows it at speed x playback_rate, so its sound is time-stretched",
                reference.label, reference.speed
            ));
        }
        for (i, segment) in result.segments.iter().enumerate() {
            if let Some(p) = place(fps, reference, segment, i, asset_duration) {
                placements.push(p);
            }
        }
    }
    ensure!(
        !placements.is_empty(),
        "the asset does not overlap any chosen reference clip on the timeline (sync offset {:.3} s): the recordings cover different parts of the take, or the clip's source_in/duration excludes the shared part",
        result.offset_seconds
    );
    let used: Vec<usize> = {
        let mut v: Vec<usize> = placements.iter().map(|p| p.segment).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    gate(&result, &used, args.allow_limited, &mut warnings)?;
    for w in result
        .warnings
        .iter()
        .filter(|w| !w.starts_with("audio alignment includes microphone propagation delay"))
    {
        warnings.push(format!("sync: {w}"));
    }
    for p in &placements {
        if p.extrapolated_seconds > EXTRAPOLATION_WARN_SECONDS {
            warnings.push(format!(
                "{} segment {}: {:.1} s of the placed asset lies outside the matched anchors (asset {:.1}-{:.1} s); the mapping is extrapolated there",
                p.reference_clip,
                p.segment,
                p.extrapolated_seconds,
                result.segments[p.segment].asset_interval[0],
                result.segments[p.segment].asset_interval[1]
            ));
        }
    }
    placements.sort_by(|a, b| a.start.total_cmp(&b.start));
    for pair in placements.windows(2) {
        ensure!(
            pair[1].timeline_range[0] >= pair[0].timeline_range[1] - 1e-9,
            "placements for {} and {} overlap on the timeline; choose reference clips with --clip",
            pair[0].reference_clip,
            pair[1].reference_clip
        );
    }

    // Timeline inside reference clips that the asset does not cover.
    let mut uncovered = Vec::new();
    for reference in &references {
        let mut at = reference.start;
        let mut spans: Vec<[f64; 2]> = placements
            .iter()
            .filter(|p| p.reference_clip == reference.label)
            .map(|p| p.timeline_range)
            .collect();
        spans.sort_by(|a, b| a[0].total_cmp(&b[0]));
        spans.push([reference.end, reference.end]);
        for span in spans {
            if span[0] - at > 0.5 / fps.value() {
                uncovered
                    .push(json!({"reference_clip":reference.label,"timeline_range":[at, span[0]]}));
            }
            at = at.max(span[1]);
        }
    }
    if !uncovered.is_empty() {
        warnings.push(format!(
            "{} timeline range(s) inside the reference clip(s) have no asset sound (see uncovered){}",
            uncovered.len(),
            if args.keep_reference_audio {
                ""
            } else {
                "; the reference's own sound is muted there too, so they are silent unless you trim the clip or use --keep-reference-audio"
            }
        ));
    }

    // Write the clips.
    let asset_name = asset_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "asset".into());
    let track_name = args
        .track
        .clone()
        .unwrap_or_else(|| format!("{asset_name} (synced)"));
    let source = edit_path(&asset_path, &edit_dir);
    let track_index = match edit
        .audio
        .iter()
        .position(|t| t.name.as_deref() == Some(track_name.as_str()))
    {
        Some(i) => i,
        None => {
            edit.audio.push(AudioTrack {
                name: Some(track_name.clone()),
                role: Some(args.role),
                clips: Vec::new(),
            });
            edit.audio.len() - 1
        }
    };
    // Mute by the references' clip indices before new clips shift them.
    if !args.keep_reference_audio {
        for reference in &references {
            match reference.kind {
                Kind::Video => {
                    edit.video[reference.track].clips[reference.clip].audio = Mix {
                        volume: 0.0,
                        ..Mix::default()
                    }
                }
                Kind::Audio => edit.audio[reference.track].clips[reference.clip].volume = 0.0,
            }
        }
    }
    for p in &placements {
        edit.audio[track_index].clips.push(AudioClip {
            start: p.start,
            source: source.clone(),
            source_in: p.source_in,
            duration: p.duration,
            speed: p.speed,
            volume: 1.0,
            volume_keyframes: Vec::new(),
            fade_in: 0.0,
            fade_out: 0.0,
        });
    }
    edit.audio[track_index]
        .clips
        .sort_by(|a, b| a.start.total_cmp(&b.start));
    timeline::validate(&edit, &edit_dir).with_context(|| {
        format!(
            "the placed clips do not fit track {track_name:?} (existing clips overlap them?); pass a new --track name"
        )
    })?;
    // Verify before publishing, so a verification error leaves no output.
    let verify = if args.verify {
        Some(verify(&placements, &asset_path, &reference_path, fps)?)
    } else {
        None
    };
    crate::io::write_json(&args.output, &edit, args.overwrite)?;
    let validation = timeline::load_and_validate(&args.output)?.summary();
    let mut out = json!({
        "output": args.output,
        "sync": args.sync,
        "reference": reference_path,
        "asset": asset_path,
        "method": result.method,
        "quality": result.quality,
        "discontinuous": result.discontinuous,
        "track": {"index": track_index, "name": track_name},
        "reference_audio": if args.keep_reference_audio { "kept" } else { "muted" },
        "reference_clips": references.iter().map(|r| &r.label).collect::<Vec<_>>(),
        "placements": placements,
        "uncovered": uncovered,
        "validation": validation,
        "warnings": warnings,
    });
    if let Some(verify) = verify {
        out["verify"] = verify;
    }
    Ok(out)
}

/// Reference sound from `lo` to `hi` seconds, in the same time base as
/// `skywrite sync` (container origin, delayed audio preserved), as mono WAV.
fn reference_excerpt(reference: &Path, lo: f64, hi: f64, output: &Path) -> Result<()> {
    let args: Vec<std::ffi::OsString> = [
        "-nostdin", "-v", "error", "-threads", "1", "-copyts", "-start_at_zero",
        "-protocol_whitelist", "file,pipe", "-i",
    ]
    .into_iter()
    .map(Into::into)
    .chain([reference.as_os_str().to_owned()])
    .chain(
        [
            "-map".to_string(),
            "0:a:0".into(),
            "-vn".into(),
            "-af".into(),
            format!("aresample=48000:async=0:first_pts=0,atrim=start={lo:.6}:end={hi:.6},asetpts=PTS-STARTPTS"),
            "-ac".into(),
            "1".into(),
            "-c:a".into(),
            "pcm_s16le".into(),
        ]
        .into_iter()
        .map(Into::into),
    )
    .chain([output.as_os_str().to_owned()])
    .collect();
    crate::process::run(
        "ffmpeg",
        &args,
        std::time::Duration::from_secs(900),
        1 << 20,
    )?;
    Ok(())
}
/// Render each placed clip alone, sync the render against the reference and
/// compare with the mapping the edit intends: render time u shows reference
/// time source_in + speed * (start - clip start + u) of the reference clip.
fn verify(
    placements: &[Placement],
    asset: &Path,
    reference: &Path,
    fps: Rational,
) -> Result<Value> {
    let dir = tempfile::tempdir().context("creating a verification directory")?;
    let mut clips = Vec::new();
    let mut passed = true;
    let mut verified = 0;
    for (i, p) in placements.iter().enumerate() {
        let label = format!("{} segment {}", p.reference_clip, p.segment);
        if p.duration < VERIFY_MIN_SECONDS {
            clips.push(json!({"placement":i,"clip":label,"status":"skipped","reason":format!("shorter than {VERIFY_MIN_SECONDS} s")}));
            continue;
        }
        let r = &p.reference;
        if (r.speed - 1.0).abs() > 0.03 {
            clips.push(json!({"placement":i,"clip":label,"status":"skipped","reason":format!("{} plays at speed {}; --verify supports reference clips within 3% of speed 1", r.label, r.speed)}));
            continue;
        }
        let edit = Edit {
            version: timeline::EDIT_VERSION,
            name: format!("sync verify {i}"),
            width: 160,
            height: 90,
            fps,
            duration: None,
            sources: Vec::new(),
            video: Vec::new(),
            audio: vec![AudioTrack {
                name: Some("asset".into()),
                role: None,
                clips: vec![AudioClip {
                    start: 0.0,
                    source: asset.to_path_buf(),
                    source_in: p.source_in,
                    duration: p.duration,
                    speed: p.speed,
                    volume: 1.0,
                    volume_keyframes: Vec::new(),
                    fade_in: 0.0,
                    fade_out: 0.0,
                }],
            }],
            master: Default::default(),
            captions: Vec::new(),
            chapters: Vec::new(),
            pauses: Vec::new(),
        };
        // Reference time shown at render time u: expected_offset + speed * u.
        let expected_offset = r.source_in + r.speed * (p.start - r.start);
        let lo = (expected_offset - VERIFY_MARGIN_SECONDS).max(0.0);
        let hi = expected_offset + r.speed * p.duration + VERIFY_MARGIN_SECONDS;
        let options = SyncOptions {
            max_audio_seconds: (hi - lo + 30.0).max(3600.0),
            ..SyncOptions::default()
        };
        if options.validate().is_err() {
            clips.push(json!({"placement":i,"clip":label,"status":"skipped","reason":"longer than the 4-hour sync analysis limit"}));
            continue;
        }
        let validated = timeline::validate(&edit, dir.path())?;
        let render = dir.path().join(format!("verify-{i}.mp4"));
        eprintln!(
            "sync apply --verify: rendering {label} ({:.1} s)",
            p.duration
        );
        crate::export::render(&validated, &render, false, StretcherChoice::Auto)
            .with_context(|| format!("rendering {label} for verification"))?;
        let excerpt = dir.path().join(format!("reference-{i}.wav"));
        reference_excerpt(reference, lo, hi, &excerpt)
            .with_context(|| format!("decoding the reference around {label}"))?;
        let expected_offset = expected_offset - lo;
        verified += 1;
        match crate::audio_sync::align_audio(&excerpt, &render, &options) {
            Ok(m) => {
                let at = |u: f64| m.offset_seconds + m.scale * u - (expected_offset + r.speed * u);
                let (start, end) = (at(0.0), at(p.duration));
                let margin = (0.1 * p.duration).max(5.0);
                let mut problems = Vec::new();
                if m.method != "audio_waveform" {
                    problems.push(format!("method {}", m.method));
                }
                if m.discontinuous {
                    problems.push(format!(
                        "the render re-syncs as {} segments",
                        m.segments.len()
                    ));
                }
                if m.quality != "strong" {
                    problems.push(format!("re-sync quality {}", m.quality));
                }
                if m.asset_interval[0] > margin || m.asset_interval[1] < p.duration - margin {
                    problems.push(format!(
                        "matched only render {:.1}-{:.1} s of {:.1} s",
                        m.asset_interval[0], m.asset_interval[1], p.duration
                    ));
                }
                if start.abs() > VERIFY_LIMIT_SECONDS || end.abs() > VERIFY_LIMIT_SECONDS {
                    problems.push(format!(
                        "residual {:.3}/{:.3} ms exceeds {} ms",
                        start * 1e3,
                        end * 1e3,
                        VERIFY_LIMIT_SECONDS * 1e3
                    ));
                }
                let ok = problems.is_empty();
                passed &= ok;
                clips.push(json!({
                    "placement": i, "clip": label, "status": if ok { "passed" } else { "failed" },
                    "problems": problems,
                    "residual_start_ms": (start * 1e6).round() / 1e3,
                    "residual_end_ms": (end * 1e6).round() / 1e3,
                    "residual_drift_ppm": ((m.scale / r.speed - 1.0) * 1e9).round() / 1e3,
                    "method": m.method, "quality": m.quality, "drift_estimated": m.drift_estimated,
                    "p95_error_ms": (m.p95_error_seconds * 1e6).round() / 1e3,
                }));
            }
            Err(error) => {
                passed = false;
                clips.push(json!({"placement":i,"clip":label,"status":"failed","error":format!("{error:#}")}));
            }
        }
    }
    Ok(json!({
        "passed": passed && verified > 0,
        "verified": verified,
        "limit_ms": VERIFY_LIMIT_SECONDS * 1e3,
        "note": "re-syncs a render of each placed clip against the reference: checks the placement and the renderer's speed conform, not the acoustic delay between microphones",
        "clips": clips,
    }))
}
