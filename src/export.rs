//! Deterministic output backends compiled from a validated edit.
use crate::{
    captions::{self, CaptionMode},
    io, media, process,
    stretch::{SpeedFilters, StretcherChoice},
    timeline::{MediaKind, ValidatedEdit, ValidatedMix, ValidatedVideoClip},
};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fmt::Write as _,
    path::{Path, PathBuf},
    time::Duration,
};
mod fcpxml;
mod media_url;
mod project;
mod xmeml;

/// Nearest-frame rounding may run a clip up to one frame past its media end.
/// Skywrite's renderers pad that tail, but an NLE may treat it as missing media,
/// so interchange files end such clips on the last whole frame inside the media.
fn within_media(edit: &ValidatedEdit) -> (ValidatedEdit, Vec<String>) {
    let mut edit = edit.clone();
    let fps = edit.fps;
    let mut notes = Vec::new();
    let overruns = |source_in: f64, frames: u64, speed: f64, media: &crate::media::MediaInfo| {
        source_in + fps.seconds(frames) * speed > media.duration + 0.002
    };
    let fit_mix = |mix: &mut crate::timeline::ValidatedMix, duration: f64| {
        mix.fade_out = mix.fade_out.min((duration - mix.fade_in).max(0.0));
    };
    for track in &mut edit.video {
        for clip in &mut track.clips {
            if clip.kind != MediaKind::Video {
                continue;
            }
            let before = clip.duration_frames;
            while clip.duration_frames > 1
                && overruns(
                    clip.source_in,
                    clip.duration_frames,
                    clip.speed,
                    &clip.media,
                )
            {
                clip.duration_frames -= 1;
            }
            if clip.duration_frames != before {
                let duration = fps.seconds(clip.duration_frames);
                clip.fade_out = clip.fade_out.min((duration - clip.fade_in).max(0.0));
                fit_mix(&mut clip.audio, duration);
                notes.push(format!(
                    "video track {} clip {}: ends {} frame(s) earlier in this interchange file so it stays within its media",
                    clip.track,
                    clip.index,
                    before - clip.duration_frames
                ));
            }
        }
    }
    for track in &mut edit.audio {
        for clip in &mut track.clips {
            let before = clip.duration_frames;
            while clip.duration_frames > 1
                && overruns(
                    clip.source_in,
                    clip.duration_frames,
                    clip.speed,
                    &clip.media,
                )
            {
                clip.duration_frames -= 1;
            }
            if clip.duration_frames != before {
                fit_mix(&mut clip.mix, fps.seconds(clip.duration_frames));
                notes.push(format!(
                    "audio track {} clip {}: ends {} frame(s) earlier in this interchange file so it stays within its media",
                    clip.track,
                    clip.index,
                    before - clip.duration_frames
                ));
            }
        }
    }
    (edit, notes)
}

/// Export settings beyond the target and output.
#[derive(Debug, Clone, Default)]
pub struct ExportOptions {
    /// FCPXML/Premiere XML only: the editor's absolute media folder. Each
    /// source URL is rewritten beneath it, keeping its layout relative to the
    /// folder that holds the edit and all its media.
    pub media_root: Option<String>,
    /// FFmpeg script only: time-stretcher for creative clip speeds (default auto).
    pub stretcher: Option<StretcherChoice>,
}

pub fn export(edit: &ValidatedEdit, target: &str, output: &Path, overwrite: bool) -> Result<Value> {
    export_with(edit, target, output, overwrite, &ExportOptions::default())
}

pub fn export_with(
    edit: &ValidatedEdit,
    target: &str,
    output: &Path,
    overwrite: bool,
    options: &ExportOptions,
) -> Result<Value> {
    prevent_source_overwrite(edit, output)?;
    let interchange_target = matches!(target, "fcpxml" | "premiere" | "xmeml" | "fcp7");
    let root = options
        .media_root
        .as_deref()
        .map(|root| {
            ensure!(
                interchange_target,
                "--media-root applies only to --to fcpxml or --to premiere; composition projects copy their media"
            );
            media_url::MediaRoot::parse(root)
        })
        .transpose()?;
    let mut warnings = edit.warnings.clone();
    let interchange;
    let edit = if interchange_target {
        let (fitted, notes) = within_media(edit);
        warnings.extend(notes);
        interchange = fitted;
        &interchange
    } else {
        edit
    };
    let urls = if interchange_target {
        let (urls, notes) = media_url::MediaUrls::new(edit, root.as_ref())?;
        warnings.extend(notes);
        Some(urls)
    } else {
        None
    };
    ensure!(
        options.stretcher.is_none() || matches!(target, "ffmpeg" | "python"),
        "--stretcher applies only to --to ffmpeg; other targets leave speed changes to the NLE or composition"
    );
    let mut sidecar_files: Vec<PathBuf> = Vec::new();
    let mut audio_stretcher = Value::Null;
    match (target, &urls) {
        ("fcpxml", Some(urls)) => {
            io::atomic_write(output, fcpxml::compile(edit, urls)?.as_bytes(), overwrite)?;
            warnings.extend(interchange_warnings(edit, "FCPXML"));
            if !edit.captions.is_empty() {
                warnings.push("Caption tracks are embedded as native FCPXML captions (SRT role) above the video tracks. Burn-in is not applied. Final Cut/Resolve import of embedded captions has not been verified; please report results with skywrite nit. Unknown languages are labelled English.".into());
            }
        }
        ("premiere" | "xmeml" | "fcp7", Some(urls)) => {
            let (xml, notes) = xmeml::compile(edit, urls)?;
            io::atomic_write(output, xml.as_bytes(), overwrite)?;
            warnings.extend(interchange_warnings(edit, "Premiere XML"));
            warnings.extend(notes);
            if !edit.captions.is_empty() {
                let files = caption_sidecars(edit, output, overwrite)?;
                warnings.push(format!(
                    "Premiere XML cannot carry captions, so each caption track was written beside it as SRT ({}). In Premiere, import the XML and the SRT together (File > Import), then drag the caption item onto the sequence at 00:00:00:00; the captions stay editable as text.",
                    files.iter().map(|f| f.display().to_string()).collect::<Vec<_>>().join(", ")
                ));
                sidecar_files = files;
            }
        }
        ("ffmpeg" | "python", _) => {
            let speeds = SpeedFilters::resolve(
                options.stretcher.unwrap_or_default(),
                edit.audible_spans().iter().map(|s| s.speed),
            )?;
            io::atomic_write(output, python_script(edit, &speeds)?.as_bytes(), overwrite)?;
            warnings.extend(speeds.warnings());
            audio_stretcher = speeds.report();
        }
        ("hyperframes" | "remotion", _) => return project::export(edit, target, output),
        _ => bail!(
            "unsupported export target {target}; choose fcpxml, premiere, ffmpeg, hyperframes, or remotion"
        ),
    }
    let mut result = json!({"target":target,"output":output,"caption_files":sidecar_files,"external_sources":edit.external_sources(),"duration_seconds":edit.duration(),"duration_frames":edit.duration_frames,"warnings":warnings});
    if let (Some(root), Some(base)) = (
        &options.media_root,
        urls.as_ref().and_then(|urls| urls.base.as_ref()),
    ) {
        result["media_root"] = json!(root);
        result["media_base"] = json!(base);
    }
    if !audio_stretcher.is_null() {
        result["audio_stretcher"] = audio_stretcher;
    }
    Ok(result)
}

/// Write one validated caption track (output timeline) as SRT or VTT.
pub fn write_caption_track(
    edit: &ValidatedEdit,
    index: usize,
    output: &Path,
    format: Option<&str>,
    overwrite: bool,
) -> Result<Value> {
    prevent_source_overwrite(edit, output)?;
    let track = edit.captions.get(index).with_context(|| {
        format!(
            "edit has {} caption tracks; add one (for example {{\"from_transcripts\": [\"words.json\"]}}) or choose --track",
            edit.captions.len()
        )
    })?;
    let format = match format.or_else(|| output.extension().and_then(|e| e.to_str())) {
        Some("vtt") => "vtt",
        Some("srt") | None => "srt",
        Some(other) => bail!("caption format must be srt or vtt, not {other}"),
    };
    io::atomic_write(
        output,
        captions::encode(&track.cues, format)?.as_bytes(),
        overwrite,
    )?;
    Ok(
        json!({"output":output,"format":format,"track":index,"language":track.language,"cue_count":track.cues.len(),
        "timeline":"output","note":"Cues are on the edit's output timeline. Import alongside the timeline export so captions stay editable in the NLE."}),
    )
}

/// Sidecar caption files written next to an NLE export: NAME.LANG[.N].srt.
fn caption_sidecars(edit: &ValidatedEdit, output: &Path, overwrite: bool) -> Result<Vec<PathBuf>> {
    let stem = output
        .file_stem()
        .and_then(|s| s.to_str())
        .context("export output needs a file name")?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut written = Vec::new();
    for (index, track) in edit.captions.iter().enumerate() {
        let repeated = edit
            .captions
            .iter()
            .filter(|t| t.language == track.language)
            .count()
            > 1;
        let name = if repeated {
            format!("{stem}.{}.{index}.srt", track.language)
        } else {
            format!("{stem}.{}.srt", track.language)
        };
        let path = parent.join(name);
        prevent_source_overwrite(edit, &path)?;
        io::atomic_write(
            &path,
            captions::encode(&track.cues, "srt")?.as_bytes(),
            overwrite,
        )?;
        written.push(path);
    }
    Ok(written)
}

/// Finishing that NLE interchange cannot carry identically to the MP4 render.
fn interchange_warnings(edit: &ValidatedEdit, format: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    for source in &edit.sources {
        if let Some(lut) = &source.lut {
            warnings.push(format!(
                "{format} cannot reference LUT {} for {}; apply it in the NLE (Lumetri/Custom LUT) to match the rendered MP4",
                lut.display(),
                source.path.display()
            ));
        }
    }
    if let Some(limit) = edit.master.limiter_db {
        warnings.push(format!(
            "{format} has no master limiter; add a {limit} dBFS limiter on the NLE mix bus to match the rendered MP4"
        ));
    }
    warnings
}
fn prevent_source_overwrite(edit: &ValidatedEdit, path: &Path) -> Result<()> {
    io::protect_inputs(
        path,
        &edit
            .protected_paths()
            .map(Path::to_owned)
            .collect::<Vec<_>>(),
    )
}
#[derive(Debug, Serialize)]
struct RenderPlan {
    args: Vec<String>,
    sources: Vec<PathBuf>,
    duration_seconds: f64,
    timeout_seconds: u64,
    sidecars: Vec<Sidecar>,
}
/// Generated private file content. Names are fixed ASCII; text is inert data.
#[derive(Debug, Serialize)]
struct Sidecar {
    name: String,
    text: String,
}
fn number(value: f64) -> String {
    format!("{value:.9}")
}
fn push(args: &mut Vec<String>, items: &[&str]) {
    args.extend(items.iter().map(|s| (*s).to_owned()));
}
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn unseeked_prefix(info: &media::MediaInfo, source_in: f64) -> f64 {
    let before_video = info
        .video()
        .and_then(|video| video.start_time)
        .is_some_and(|start| source_in < start - info.start_time);
    // TS has no reliable random-access index; input seeking can miss the
    // only usable keyframe. This affects rendering, never metadata inspection.
    if before_video || info.format_name.split(',').any(|name| name == "mpegts") {
        source_in
    } else {
        0.0
    }
}

/// Linear gain curve as an FFmpeg expression of segment-relative time `t`.
fn envelope_expression(mix: &ValidatedMix) -> String {
    let k = &mix.keyframes;
    let mut expr = number(k[k.len() - 1].volume);
    for i in (0..k.len() - 1).rev() {
        let (a, b) = (k[i], k[i + 1]);
        let span = b.time - a.time;
        expr = format!(
            "if(lt(t,{}),{}+({})*(t-{})/{},{expr})",
            number(b.time),
            number(a.volume),
            number(b.volume - a.volume),
            number(a.time),
            number(span)
        );
    }
    format!(
        "{}*if(lt(t,{}),{},{expr})",
        number(mix.volume),
        number(k[0].time),
        number(k[0].volume)
    )
}

/// Gain, keyframe envelope, then linear fades, applied in output time.
fn mix_filters(mix: &ValidatedMix, duration: f64) -> String {
    let mut parts = Vec::new();
    if mix.keyframes.is_empty() {
        if mix.volume != 1.0 {
            parts.push(format!("volume={}", number(mix.volume)));
        }
    } else {
        // Commas are protected by filtergraph quoting; no user text is present.
        parts.push(format!(
            "volume=eval=frame:volume='{}'",
            envelope_expression(mix)
        ));
    }
    if mix.fade_in > 0.0 {
        parts.push(format!(
            "afade=t=in:st=0:d={}:curve=tri",
            number(mix.fade_in)
        ));
    }
    if mix.fade_out > 0.0 {
        parts.push(format!(
            "afade=t=out:st={}:d={}:curve=tri",
            number(duration - mix.fade_out),
            number(mix.fade_out)
        ));
    }
    if parts.is_empty() {
        "anull".into()
    } else {
        parts.join(",")
    }
}

fn silence(samples: u64, label: &str) -> String {
    format!("anullsrc=r=48000:cl=stereo,atrim=end_sample={samples},asetpts=PTS-STARTPTS[{label}]")
}

/// Where a clip's input is decoded from: `seek` is the `-ss` point (0: from
/// the start), `prefix` the decoded time before `source_in` (`source_in - seek`).
#[derive(Debug, Clone, Copy)]
struct SourceTimes {
    source_in: f64,
    prefix: f64,
    seek: f64,
}

impl SourceTimes {
    fn new(source_in: f64, seek: f64) -> Self {
        Self {
            source_in,
            prefix: source_in - seek,
            seek,
        }
    }
}

/// How far before `source_in` the picture decode starts, so the source frame
/// already on screen at `source_in` is decoded (FFmpeg's accurate `-ss` drops
/// frames that start before the seek point).
const PICTURE_PREROLL_SECONDS: f64 = 1.0;
/// Decoded past the clip's last source time, so rounded input timestamps (an
/// AVI at 1001/30000 s) never drop the frame on screen at the end; the output
/// is trimmed to the clip's frames anyway.
const PICTURE_TAIL_SECONDS: f64 = 0.5;
/// A positive rational such as "1/15360" or "30000/1001".
fn rational(text: Option<&str>) -> Option<(u64, u64)> {
    let (num, den) = text?.split_once('/')?;
    let (num, den) = (num.parse::<u64>().ok()?, den.parse::<u64>().ok()?);
    (num > 0 && den > 0).then_some((num, den))
}

/// A stream's start time as its file stores it. FFprobe reports start times
/// to the microsecond, while a stream's own times are whole ticks of its time
/// base; with a tick of at least 2 us the reported time rounds back to that
/// tick exactly (an MP4 starting at 76831/15360 s is reported as 5.002018).
fn exact_start(stream: &media::MediaStream) -> Option<f64> {
    let start = stream.start_time?;
    Some(match rational(stream.time_base.as_deref()) {
        Some((num, den)) if num as f64 / den as f64 >= 0.000_002 => {
            let tick = num as f64 / den as f64;
            (start / tick).round() * tick
        }
        _ => start,
    })
}

/// The file's timeline origin (its earliest stream start, as FFprobe reports
/// it) at the exact time of the stream it comes from, and the first picture's
/// exact time, both in the file's own seconds.
fn exact_origins(info: &media::MediaInfo) -> (f64, Option<f64>) {
    let start = info
        .streams
        .iter()
        .filter_map(exact_start)
        .filter(|exact| (exact - info.start_time).abs() < 0.000_001)
        .reduce(f64::min)
        .unwrap_or(info.start_time);
    (start, info.video().and_then(exact_start))
}

/// How a video stream's frame times are read for on-screen sampling.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PictureTiming {
    /// How far the container may have rounded a frame time, either way
    /// (seconds, half a tick; 0 when the times are exact).
    rounding: f64,
    /// Nominal frame rate whose grid a frame time within `rounding` of it is
    /// snapped back to.
    grid: Option<(u64, u64)>,
}

/// Timestamps are exact when the stream's frame rate puts every frame on a
/// whole tick of its time base (30 fps in 1/15360, 29.97 in 1/30000, 25 in
/// 1/1000). Otherwise the container rounded them, by up to half a tick either
/// way (Matroska stores 29.97 or 30 fps to 1 ms, MOV often uses 1/600 s). A
/// frame time within that of its nominal frame grid (counted from the first
/// picture) is snapped back to the grid; any other frame (variable frame rate)
/// keeps its time and counts as on screen up to that rounding, at most 0.5 ms
/// (`early`), late. Without a time base or frame rate, 0.5 ms is assumed.
impl PictureTiming {
    /// How late a frame that keeps its own time may start and still count as
    /// on screen: the rounding, at most 0.5 ms because NTSC frames lie at least
    /// 1 ms off round seconds.
    fn early(self) -> f64 {
        self.rounding.min(0.000_5)
    }
}

fn picture_timing(info: &media::MediaInfo) -> PictureTiming {
    let video = info.video();
    let time_base = video.and_then(|v| rational(v.time_base.as_deref()));
    let rate = video.and_then(|v| rational(v.r_frame_rate.as_deref()));
    match (time_base, rate) {
        (Some((tick_num, tick_den)), Some((rate_num, rate_den))) => {
            // Ticks per frame: (rate_den / rate_num) / (tick_num / tick_den).
            let (whole, per) = (
                u128::from(rate_den) * u128::from(tick_den),
                u128::from(rate_num) * u128::from(tick_num),
            );
            if whole % per == 0 {
                PictureTiming {
                    rounding: 0.0,
                    grid: None,
                }
            } else {
                PictureTiming {
                    rounding: tick_num as f64 / tick_den as f64 / 2.0,
                    grid: Some((rate_num, rate_den)),
                }
            }
        }
        _ => PictureTiming {
            rounding: 0.000_5,
            grid: None,
        },
    }
}

/// Seek point for a video clip: up to the pre-roll before `source_in`, not
/// before the first picture, in whole microseconds (FFmpeg parses `-ss` to
/// microseconds). 0 means decode from the start (also for MPEG-TS and a
/// `source_in` at or before the first picture, see `unseeked_prefix`). Picture
/// timing does not depend on it: with `-copyts` frames keep the file's times.
fn picture_seek(info: &media::MediaInfo, source_in: f64) -> f64 {
    if unseeked_prefix(info, source_in) > 0.0 {
        return 0.0;
    }
    let video = info.video();
    let first = video
        .and_then(|v| v.start_time)
        .map_or(0.0, |start| start - info.start_time);
    if source_in <= first {
        // Nothing is on screen before the first picture: decode from the start.
        return 0.0;
    }
    let seek = ((source_in - PICTURE_PREROLL_SECONDS).max(first) * 1e6).round() / 1e6;
    if seek > 0.0 { seek } else { 0.0 }
}

#[allow(clippy::too_many_arguments)]
fn audio_segment(
    speeds: &SpeedFilters,
    input: usize,
    info: &media::MediaInfo,
    source_in: f64,
    output_samples: u64,
    speed: f64,
    envelope: &str,
    label: &str,
) -> Result<String> {
    let duration = output_samples as f64 / 48000.0;
    let (origin, _) = exact_origins(info);
    let audio_start = info
        .audio()
        .map(|audio| exact_start(audio).unwrap_or(origin) - origin)
        .unwrap_or(0.0);
    let lead = (audio_start - source_in).max(0.0);
    let content = (duration * speed - lead).max(0.0);
    if content <= 0.0 {
        return Ok(silence(output_samples, label));
    }
    // Retiming the content and padding the known stream delay separately prevents
    // a time-stretcher from moving the leading boundary when it stretches silence.
    let delay = (lead / speed * 48000.0).round() as u64;
    let source_samples = (content * 48000.0).round() as u64;
    Ok(format!(
        "[{input}:a:0]asetpts=round(PTS-{}/TB),atrim=start={},asetpts=PTS-STARTPTS,aresample=48000:async=1:first_pts=0,atrim=end_sample={source_samples},{},aformat=sample_fmts=fltp:channel_layouts=stereo,adelay={delay}S:all=1,apad=whole_len={output_samples},atrim=end_sample={output_samples},asetpts=N/SR/TB,{envelope}[{label}]",
        // With -copyts the sound keeps the file's times: count them from the
        // container start and trim at `source_in` (or the sound's own start)
        // there, wherever the decode after a seek happens to begin.
        number(origin),
        number(source_in.max(audio_start)),
        speeds.filter(speed, source_samples, label)?,
    ))
}

/// Restamp a picture stream on the output frame grid: time base one frame,
/// frame N at pts N. Every segment emits exactly its frame count, so after a
/// concat this places frame N of a track on output frame N. Microsecond
/// timestamps (concat's own time base) cannot hold 1001/30000 frame steps, and
/// concat estimates where a segment ends from its last frame (a one-frame
/// segment advanced time by nothing); either made overlay show the previous
/// frame past a cut, drop a one-frame clip, or end a track early.
fn frame_clock(edit: &ValidatedEdit) -> String {
    format!("settb={}/{},setpts=N", edit.fps.den, edit.fps.num)
}

fn transparent(edit: &ValidatedEdit, frames: u64, label: &str) -> String {
    format!(
        "color=c=black@0:s={}x{}:r={},format=yuva420p,trim=end_frame={frames},{}[{label}]",
        edit.width,
        edit.height,
        edit.fps.ffmpeg(),
        frame_clock(edit)
    )
}

/// Add one clip's input and return its picture chain.
///
/// Output frame n of the clip shows the source frame on screen at source time
/// `source_in + n * speed / fps`: the last one that starts at or before it.
/// Frame timestamps stay relative to `source_in` (the decode starts a pre-roll
/// earlier), `fps` rounds them up onto output frames (an output frame takes the
/// latest frame whose time is not after its own), and output frames before the
/// first picture of a late-starting video stream are black.
fn video_clip(
    edit: &ValidatedEdit,
    clip: &ValidatedVideoClip,
    args: &mut Vec<String>,
    input: usize,
    lut_marker: Option<(&str, bool)>,
    label: &str,
) -> Result<String> {
    let fps = edit.fps.ffmpeg();
    let duration = edit.fps.seconds(clip.duration_frames);
    let frames = clip.duration_frames;
    let source = clip.source.to_str().context("source path is not UTF-8")?;
    let info = &clip.media;
    let times = if clip.kind == MediaKind::Image {
        SourceTimes::new(0.0, 0.0)
    } else {
        let mut seek = picture_seek(info, clip.source_in);
        if seek > 0.0 {
            // A variable-frame-rate picture can stay on screen for longer than
            // the pre-roll; start the decode no later than it (accurate -ss
            // drops frames that start before the seek point).
            // When the file gives no packet times, decode from its start.
            let (origin, _) = exact_origins(info);
            seek = match media::picture_start_at_or_before(&clip.source, origin + clip.source_in)? {
                Some(start) => seek.min(((start - origin) * 1e6).floor() / 1e6),
                None => 0.0,
            };
        }
        SourceTimes::new(clip.source_in, seek.max(0.0))
    };
    if clip.kind == MediaKind::Image {
        push(
            args,
            &[
                "-loop",
                "1",
                "-framerate",
                &fps,
                "-t",
                &number(duration),
                "-protocol_whitelist",
                "file,pipe",
                "-i",
                source,
            ],
        );
    } else {
        // Seeking to a video keyframe can skip audio before that frame, even
        // with -ss 0. Decode a pre-video prefix from the beginning and trim
        // its audio in the shared filter.
        if times.seek > 0.0 {
            push(args, &["-ss", &number(times.seek)]);
        }
        push(
            args,
            &[
                "-t",
                &number(duration * clip.speed + times.prefix + PICTURE_TAIL_SECONDS),
                "-protocol_whitelist",
                "file,pipe",
                "-i",
                source,
            ],
        );
    }
    // Output frames before the first picture of a stream that starts after
    // `source_in` are black: those whose source time is before it.
    let (origin, picture) = exact_origins(info);
    let first_picture = picture.map_or(0.0, |start| start - origin - times.source_in);
    let timing = if clip.kind == MediaKind::Image {
        PictureTiming {
            rounding: 0.0,
            grid: None,
        }
    } else {
        picture_timing(info)
    };
    // The first picture's time as the expression below sees it (on the grid
    // when snapped, else counted `early`), with the same 20 ns margins in
    // source and output time, gives the output frame it first appears on.
    let first_seen = first_picture
        - if timing.grid.is_some() {
            0.0
        } else {
            timing.early()
        };
    let first_out = (first_seen - 0.000_000_02) / clip.speed - 0.000_000_02;
    let lead_frames = if clip.kind == MediaKind::Video && first_out > 0.0 {
        (first_out * edit.fps.value()).ceil() as u64
    } else {
        0
    };
    if lead_frames >= frames {
        // No picture is on screen during the clip (an audio-only prefix).
        return Ok(transparent(edit, frames, label));
    }
    // `tpad` adds the lead after `fps`, whose output keeps the frame rate that
    // FFmpeg 7.0 and 7.1 `setpts` clears.
    // With -copyts a frame's PTS is the file's own time; the source timeline
    // counts from the container start (`exact_origins`). A frame time the
    // container may have rounded is snapped to the frame grid from the first
    // picture when it lies within that rounding of it, else it counts up to the
    // rounding early. Times, in nanoseconds (exact in a double for 24 h), are
    // then relative to `source_in` (less 20 ns), in output time, less the lead
    // and 20 ns of output time so that `ceil` (which keeps setpts's truncation
    // from moving a frame earlier) cannot carry a frame that is exactly on an
    // output frame past it: NTSC output frames do not fall on whole
    // nanoseconds. Commas and semicolons are protected by filtergraph quoting;
    // only numbers enter the expression.
    let (start, first) = if clip.kind == MediaKind::Image {
        (0.0, 0.0)
    } else {
        (origin, picture.unwrap_or(origin))
    };
    let early = number(timing.early() * 1e9);
    let time = match timing.grid {
        Some((rate_num, rate_den)) => format!(
            "st(0,round((PTS-{origin})/{frame})*{frame}+{origin});if(lte(abs(PTS-ld(0)),{near}),ld(0),PTS-{early})",
            origin = number(first * 1e9),
            frame = number(rate_den as f64 * 1e9 / rate_num as f64),
            near = number(timing.rounding * 1e9 + 2.0),
        ),
        None => format!("PTS-{early}"),
    };
    let mut chain = format!(
        "[{input}:v:0]settb=1/1000000000,setpts='ceil((({time})-{})/{}-{})',fps={fps}:start_time=0:round=up",
        number((start + times.source_in + 0.000_000_02) * 1e9),
        number(clip.speed),
        number(edit.fps.seconds(lead_frames) * 1e9 + 20.0),
    );
    if lead_frames > 0 {
        write!(chain, ",tpad=start={lead_frames}:color=black")?;
    }
    if let Some((marker, three_d)) = lut_marker {
        // Only generated ASCII sidecar paths enter the filter graph.
        let filter = if three_d { "lut3d" } else { "lut1d" };
        write!(chain, ",{filter}=file={marker}:interp=tetrahedral")?;
        if !three_d {
            chain = chain.replace(":interp=tetrahedral", ":interp=linear");
        }
    }
    write!(
        chain,
        ",scale={w}:{h}:force_original_aspect_ratio=decrease,format=yuva420p,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black@0,setsar=1,tpad=stop_mode=clone:stop_duration=1,trim=end_frame={frames}",
        w = edit.width,
        h = edit.height
    )?;
    if clip.opacity < 1.0 {
        write!(chain, ",colorchannelmixer=aa={}", number(clip.opacity))?;
    }
    if clip.fade_in > 0.0 {
        write!(chain, ",fade=t=in:st=0:d={}:alpha=1", number(clip.fade_in))?;
    }
    if clip.fade_out > 0.0 {
        write!(
            chain,
            ",fade=t=out:st={}:d={}:alpha=1",
            number(duration - clip.fade_out),
            number(clip.fade_out)
        )?;
    }
    write!(chain, ",{}[{label}]", frame_clock(edit))?;
    Ok(chain)
}

fn concat(labels: &[String], video: bool, out: &str) -> String {
    let mut text = labels.iter().map(|l| format!("[{l}]")).collect::<String>();
    if labels.len() == 1 {
        text.push_str(if video { "null" } else { "anull" });
    } else {
        write!(
            text,
            "concat=n={}:v={}:a={}",
            labels.len(),
            u8::from(video),
            u8::from(!video)
        )
        .expect("string write");
    }
    write!(text, "[{out}]").expect("string write");
    text
}

fn render_plan(edit: &ValidatedEdit, speeds: &SpeedFilters) -> Result<RenderPlan> {
    let mut args = vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
        // Keep each input's own timestamps: FFmpeg otherwise shifts them by the
        // container start plus -ss, rounded to the stream's tick, and for
        // MPEG-TS by an origin that changes when a stream goes unused. Video
        // timing subtracts the probed container start itself; sound and stills
        // count from their first sample or frame.
        "-copyts".into(),
        "-filter_complex_threads".into(),
        "1".into(),
    ];
    let mut sidecars = Vec::new();
    let lut_markers = edit
        .luts
        .iter()
        .map(|lut| {
            let index = sidecars.len();
            sidecars.push(Sidecar {
                name: format!("lut-{index}.cube"),
                text: lut.text.clone(),
            });
            (
                format!("__SKYWRITE_SIDECAR_{index}__"),
                lut.text
                    .lines()
                    .any(|l| l.trim_start().starts_with("LUT_3D_SIZE")),
            )
        })
        .collect::<Vec<_>>();
    let mut filters = Vec::new();
    let mut input = 0usize;
    let total = edit.duration_frames;
    let total_samples = edit.fps.samples_48k(total);
    filters.push(format!(
        "color=c=black:s={}x{}:r={},format=yuv420p,trim=end_frame={total},{}[base]",
        edit.width,
        edit.height,
        edit.fps.ffmpeg(),
        frame_clock(edit)
    ));
    let mut current = "base".to_owned();
    let mut lanes = Vec::<String>::new();
    for (t, track) in edit.video.iter().enumerate() {
        if track.clips.is_empty() {
            continue;
        }
        let mut pictures = Vec::new();
        let mut sounds = Vec::new();
        let mut sound_filters = Vec::new();
        let mut cursor = 0u64;
        let mut has_sound = false;
        for (c, clip) in track.clips.iter().enumerate() {
            if clip.start_frames > cursor {
                let label = format!("vg{t}_{c}");
                filters.push(transparent(edit, clip.start_frames - cursor, &label));
                pictures.push(label);
                let label = format!("ag{t}_{c}");
                sound_filters.push(silence(
                    edit.fps.samples_48k(clip.start_frames) - edit.fps.samples_48k(cursor),
                    &label,
                ));
                sounds.push(label);
            }
            let label = format!("v{t}_{c}");
            let marker = clip
                .lut
                .map(|i| (lut_markers[i].0.as_str(), lut_markers[i].1));
            let chain = video_clip(edit, clip, &mut args, input, marker, &label)?;
            filters.push(chain);
            pictures.push(label);
            let samples =
                edit.fps.samples_48k(clip.end_frames()) - edit.fps.samples_48k(clip.start_frames);
            let label = format!("a{t}_{c}");
            if clip.has_audible_sound() {
                has_sound = true;
                sound_filters.push(audio_segment(
                    speeds,
                    input,
                    &clip.media,
                    clip.source_in,
                    samples,
                    clip.speed,
                    &mix_filters(&clip.audio, edit.fps.seconds(clip.duration_frames)),
                    &label,
                )?);
            } else {
                sound_filters.push(silence(samples, &label));
            }
            sounds.push(label);
            cursor = clip.end_frames();
            input += 1;
        }
        filters.push(concat(&pictures, true, &format!("cat{t}")));
        filters.push(format!("[cat{t}]{}[track{t}]", frame_clock(edit)));
        let next = format!("comp{t}");
        filters.push(format!(
            "[{current}][track{t}]overlay=x=0:y=0:eof_action=pass:repeatlast=0:format=yuv420[{next}]"
        ));
        current = next;
        if has_sound {
            filters.extend(sound_filters);
            let lane = format!("lane_v{t}");
            filters.push(concat(&sounds, false, &lane));
            lanes.push(lane);
        }
    }
    filters.push(format!("[{current}]format=yuv420p,setsar=1[vout]"));
    for (t, track) in edit.audio.iter().enumerate() {
        let mut sounds = Vec::new();
        let mut cursor = 0u64;
        for (c, clip) in track.clips.iter().filter(|c| c.mix.audible()).enumerate() {
            if clip.start_frames > cursor {
                let label = format!("xg{t}_{c}");
                filters.push(silence(
                    edit.fps.samples_48k(clip.start_frames) - edit.fps.samples_48k(cursor),
                    &label,
                ));
                sounds.push(label);
            }
            // As for video clips: decode from up to a second before
            // `source_in`, so a seek that resumes the sound at a later packet
            // (just after a keyframe of a video file) never reaches it.
            let seek = picture_seek(&clip.media, clip.source_in);
            let prefix = clip.source_in - seek;
            let duration = edit.fps.seconds(clip.duration_frames);
            if seek > 0.0 {
                push(&mut args, &["-ss", &number(seek)]);
            }
            push(
                &mut args,
                &[
                    "-t",
                    &number(duration * clip.speed + prefix),
                    "-protocol_whitelist",
                    "file,pipe",
                    "-i",
                    clip.source.to_str().context("source path is not UTF-8")?,
                ],
            );
            let label = format!("x{t}_{c}");
            filters.push(audio_segment(
                speeds,
                input,
                &clip.media,
                clip.source_in,
                edit.fps.samples_48k(clip.end_frames()) - edit.fps.samples_48k(clip.start_frames),
                clip.speed,
                &mix_filters(&clip.mix, duration),
                &label,
            )?);
            sounds.push(label);
            cursor = clip.end_frames();
            input += 1;
        }
        if !sounds.is_empty() {
            let lane = format!("lane_a{t}");
            filters.push(concat(&sounds, false, &lane));
            lanes.push(lane);
        }
    }
    let mut mix = match lanes.len() {
        0 => {
            filters.push(silence(total_samples, "mixed"));
            "[mixed]".to_owned()
        }
        1 => format!("[{}]", lanes[0]),
        n => {
            let mut text = lanes.iter().map(|l| format!("[{l}]")).collect::<String>();
            write!(
                text,
                "amix=inputs={n}:duration=longest:dropout_transition=0:normalize=0"
            )?;
            text
        }
    };
    if lanes.len() <= 1 {
        mix.push_str("anull");
    }
    if let Some(limit) = edit.master.limiter_db {
        write!(
            mix,
            ",alimiter=limit={}:level=0:latency=1",
            number(10f64.powf(limit / 20.0).max(0.0625))
        )?;
    }
    write!(
        mix,
        ",apad=whole_len={total_samples},atrim=end_sample={total_samples},asetpts=N/SR/TB[aout]"
    )?;
    filters.push(mix);
    let video = caption_args(edit, &mut args, input, &mut filters, "vout", &mut sidecars)?;
    // Frames are composited upright on the output canvas. Some FFmpeg builds
    // still carry a source's display matrix (iPhone rotation) to the encoder,
    // which makes players turn the finished picture sideways; drop it.
    filters.push(format!(
        "[{video}]sidedata=mode=delete:type=DISPLAYMATRIX[vfinal]"
    ));
    let fps = edit.fps.ffmpeg();
    push(
        &mut args,
        &[
            "-filter_complex",
            &filters.join(";"),
            "-map",
            "[vfinal]",
            "-map",
            "[aout]",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "18",
            "-pix_fmt",
            "yuv420p",
            "-threads",
            "2",
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-r",
            &fps,
            "-t",
            &number(edit.duration()),
            "-movflags",
            "+faststart",
            "-f",
            "mp4",
            "__SKYWRITE_OUTPUT__",
        ],
    );
    Ok(RenderPlan {
        args,
        sources: edit.protected_paths().map(Path::to_owned).collect(),
        sidecars,
        duration_seconds: edit.duration(),
        timeout_seconds: (edit.duration() * 20.0 + 120.0)
            .ceil()
            .clamp(120.0, 86_400.0) as u64,
    })
}

// Sidecar text is inert data. Only generated placeholders enter FFmpeg filters.
fn caption_args(
    edit: &ValidatedEdit,
    args: &mut Vec<String>,
    mut input: usize,
    filters: &mut Vec<String>,
    initial_video: &str,
    sidecars: &mut Vec<Sidecar>,
) -> Result<String> {
    let mut video = initial_video.to_owned();
    let mut soft = Vec::new();
    for track in &edit.captions {
        let index = sidecars.len();
        sidecars.push(Sidecar {
            name: format!("caption-{index}.vtt"),
            text: captions::encode(&track.cues, "vtt")?,
        });
        let marker = format!("__SKYWRITE_SIDECAR_{index}__");
        if track.mode == CaptionMode::Burn {
            let next = format!("vcaption{index}");
            filters.push(format!("[{video}]subtitles=filename={marker}[{next}]"));
            video = next;
        } else {
            push(
                args,
                &[
                    "-protocol_whitelist",
                    "file,pipe",
                    "-f",
                    "webvtt",
                    "-i",
                    &marker,
                ],
            );
            soft.push((input, track.language.clone()));
            input += 1;
        }
    }
    if !edit.chapters.is_empty() {
        let index = sidecars.len();
        sidecars.push(Sidecar {
            name: format!("chapters-{index}.txt"),
            text: captions::chapter_metadata(&edit.chapters, edit.duration()),
        });
        push(
            args,
            &[
                "-protocol_whitelist",
                "file,pipe",
                "-f",
                "ffmetadata",
                "-i",
                &format!("__SKYWRITE_SIDECAR_{index}__"),
                "-map_metadata",
                "-1",
                "-map_chapters",
                &input.to_string(),
            ],
        );
        for (index, chapter) in edit.chapters.iter().enumerate() {
            push(
                args,
                &[
                    &format!("-metadata:c:{index}"),
                    &format!("title={}", chapter.title),
                ],
            );
        }
    } else {
        push(args, &["-map_chapters", "-1"]);
    }
    for (index, (input, language)) in soft.iter().enumerate() {
        push(
            args,
            &[
                "-map",
                &format!("{input}:s:0"),
                &format!("-metadata:s:s:{index}"),
                &format!("language={language}"),
            ],
        );
    }
    if !soft.is_empty() {
        push(args, &["-c:s", "mov_text"]);
    }
    Ok(video)
}
fn private_sidecars(plan: &RenderPlan) -> Result<(tempfile::TempDir, process::CleanupGuard)> {
    let dir = tempfile::Builder::new()
        .prefix("skywrite-sidecars-")
        .tempdir()?;
    let cleanup = process::CleanupGuard::new(dir.path())?;
    for sidecar in &plan.sidecars {
        std::fs::write(dir.path().join(&sidecar.name), &sidecar.text)?;
    }
    Ok((dir, cleanup))
}
fn plan_args(plan: &RenderPlan, dir: &Path) -> Result<Vec<OsString>> {
    let mut args = plan.args.clone();
    for (index, sidecar) in plan.sidecars.iter().enumerate() {
        let path = dir.join(&sidecar.name);
        let path = path
            .to_str()
            .context("sidecar temporary path must be UTF-8")?;
        ensure!(
            path.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"/._-".contains(&c)),
            "sidecar temporary path must contain only ASCII letters, digits, /._-"
        );
        let marker = format!("__SKYWRITE_SIDECAR_{index}__");
        for index in 0..args.len() {
            if args[index] == marker || (index > 0 && args[index - 1] == "-filter_complex") {
                args[index] = args[index].replace(&marker, path);
            }
        }
    }
    Ok(args.into_iter().map(OsString::from).collect())
}
/// Finish the output of an optional renderer with the same validated metadata.
pub fn mux_captions(
    input: &Path,
    edit: &ValidatedEdit,
    output: &Path,
    overwrite: bool,
) -> Result<Value> {
    prevent_source_overwrite(edit, output)?;
    io::protect_inputs(output, &[input.to_owned()])?;
    ensure!(
        overwrite || !output.exists(),
        "output already exists; pass --overwrite"
    );
    let input = media::canonical_file(input)?;
    let info = media::probe_for_timing(&input)?;
    ensure!(
        info.has_video() && (info.duration - edit.duration()).abs() < 0.1 + 1.0 / edit.fps.value(),
        "mux input must be a video matching the validated output timeline duration"
    );
    let mut args = vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
        "-filter_complex_threads".into(),
        "1".into(),
    ];
    push(
        &mut args,
        &[
            "-protocol_whitelist",
            "file,pipe",
            "-i",
            input.to_str().context("input path must be UTF-8")?,
        ],
    );
    let mut filters = Vec::new();
    let mut sidecars = Vec::new();
    let video = caption_args(edit, &mut args, 1, &mut filters, "0:v:0", &mut sidecars)?;
    if filters.is_empty() {
        push(&mut args, &["-map", "0:v:0", "-c:v", "copy"]);
    } else {
        push(
            &mut args,
            &[
                "-filter_complex",
                &filters.join(";"),
                "-map",
                &format!("[{video}]"),
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-crf",
                "18",
                "-pix_fmt",
                "yuv420p",
                "-threads",
                "2",
            ],
        );
    }
    push(
        &mut args,
        &[
            "-map",
            "0:a?",
            "-c:a",
            "copy",
            "-t",
            &number(edit.duration()),
            "-movflags",
            "+faststart",
            "-f",
            "mp4",
            "__SKYWRITE_OUTPUT__",
        ],
    );
    let plan = RenderPlan {
        args,
        sidecars,
        sources: vec![input],
        duration_seconds: edit.duration(),
        timeout_seconds: (edit.duration() * 20.0 + 120.0)
            .ceil()
            .clamp(120.0, 86400.0) as u64,
    };
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temp = tempfile::Builder::new()
        .prefix(".skywrite-mux-")
        .suffix(".mp4")
        .tempfile_in(parent)?;
    let cleanup = process::CleanupGuard::new(temp.path())?;
    let (sidecars, _sidecar_cleanup) = private_sidecars(&plan)?;
    let mut args = plan_args(&plan, sidecars.path())?;
    *args.last_mut().context("missing output")? = temp.path().as_os_str().to_owned();
    process::run(
        "ffmpeg",
        &args,
        Duration::from_secs(plan.timeout_seconds),
        4 * 1024 * 1024,
    )?;
    let rendered = media::probe(temp.path())?;
    ensure!(
        rendered.has_video()
            && (rendered.duration - edit.duration()).abs() < 0.1 + 1.0 / edit.fps.value(),
        "mux output duration differs from timeline"
    );
    if overwrite {
        temp.persist(output).map_err(|e| e.error)?;
    } else {
        crate::io::persist_new(temp, output)?;
    }
    drop(cleanup);
    Ok(
        json!({"output":output,"container":"mp4","caption_track_count":edit.captions.len(),"chapter_count":edit.chapters.len(),"duration_seconds":rendered.duration}),
    )
}

pub fn render(
    edit: &ValidatedEdit,
    output: &Path,
    overwrite: bool,
    stretcher: StretcherChoice,
) -> Result<Value> {
    prevent_source_overwrite(edit, output)?;
    ensure!(
        overwrite || !output.exists(),
        "output already exists; pass --overwrite to replace it"
    );
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    ensure!(
        parent.is_dir(),
        "output parent directory does not exist: {}",
        parent.display()
    );
    let temp = tempfile::Builder::new()
        .prefix(".skywrite-render-")
        .suffix(".mp4")
        .tempfile_in(parent)?;
    let cleanup = process::CleanupGuard::new(temp.path())?;
    let speeds = SpeedFilters::resolve(stretcher, edit.audible_spans().iter().map(|s| s.speed))?;
    let plan = render_plan(edit, &speeds)?;
    let (sidecars, _sidecar_cleanup) = private_sidecars(&plan)?;
    let mut args = plan_args(&plan, sidecars.path())?;
    *args.last_mut().context("missing output argument")? = temp.path().as_os_str().to_owned();
    process::run(
        "ffmpeg",
        &args,
        Duration::from_secs(plan.timeout_seconds),
        4 * 1024 * 1024,
    )?;
    let rendered = media::probe(temp.path())?;
    ensure!(
        rendered.has_video() && rendered.has_audio(),
        "rendered output is missing audio or video"
    );
    ensure!(
        (rendered.duration - edit.duration()).abs() < 0.1 + 1.0 / edit.fps.value(),
        "rendered duration differs from requested timeline"
    );
    let mut warnings = edit.warnings.clone();
    warnings.extend(speeds.warnings());
    let (temp, rotation_cleanup) = match display_rotation(temp.path())? {
        Some(rotation) => {
            let (cleared, rotation_cleanup) = clear_display_rotation(temp.path(), parent)?;
            if let Some(left) = display_rotation(cleared.path())? {
                warnings.push(format!(
                    "the output still carries a {left} degree display rotation that this FFmpeg could not remove; players may show it sideways"
                ));
            } else {
                warnings.push(format!(
                    "removed a {rotation} degree display rotation that FFmpeg copied from a source; the picture is already upright"
                ));
            }
            (cleared, Some(rotation_cleanup))
        }
        None => (temp, None),
    };
    if overwrite {
        temp.persist(output).map_err(|e| e.error)?;
    } else {
        crate::io::persist_new(temp, output)?;
    }
    drop(rotation_cleanup);
    drop(cleanup);
    let peak = peak_dbfs(output)?;
    if let Some(peak) = peak
        && peak >= -0.1
    {
        warnings.push(format!(
            "the mix peaks at {peak:.2} dBFS and is likely clipping; lower gains or set master.limiter_db (for example -1)"
        ));
    }
    Ok(
        json!({"output":output,"container":"mp4","duration_seconds":rendered.duration,"duration_frames":edit.duration_frames,"peak_dbfs":peak,"external_sources":edit.external_sources(),"audio_stretcher":speeds.report(),"warnings":warnings}),
    )
}

/// Display-matrix rotation of the first video stream, when it is not zero.
fn display_rotation(path: &Path) -> Result<Option<String>> {
    let mut args = process::args(&[
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream_side_data=rotation",
        "-of",
        "default=noprint_wrappers=1:nokey=1",
    ]);
    args.push(path.as_os_str().to_owned());
    let out = process::run("ffprobe", &args, Duration::from_secs(60), 64 * 1024)?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|value| value.parse::<f64>().is_ok_and(|v| v != 0.0))
        .map(str::to_owned))
}

/// Stream-copy `path` with an identity display matrix (`-display_rotation`,
/// FFmpeg 6.0+), falling back to the legacy `rotate` tag on 5.1.
fn clear_display_rotation(
    path: &Path,
    parent: &Path,
) -> Result<(tempfile::NamedTempFile, process::CleanupGuard)> {
    let cleared = tempfile::Builder::new()
        .prefix(".skywrite-render-")
        .suffix(".mp4")
        .tempfile_in(parent)?;
    let cleanup = process::CleanupGuard::new(cleared.path())?;
    let attempt = |modern: bool| -> Result<()> {
        let mut args = process::args(&["-hide_banner", "-nostdin", "-v", "error", "-y"]);
        if modern {
            args.extend(process::args(&["-display_rotation", "0"]));
        }
        args.extend(process::args(&["-i"]));
        args.push(path.as_os_str().to_owned());
        args.extend(process::args(&[
            "-map",
            "0",
            "-c",
            "copy",
            "-map_chapters",
            "0",
        ]));
        if !modern {
            args.extend(process::args(&["-metadata:s:v:0", "rotate=0"]));
        }
        args.extend(process::args(&["-movflags", "+faststart", "-f", "mp4"]));
        args.push(cleared.path().as_os_str().to_owned());
        process::run("ffmpeg", &args, Duration::from_secs(600), 4 * 1024 * 1024).map(|_| ())
    };
    if attempt(true).is_err() {
        attempt(false)?;
    }
    Ok((cleared, cleanup))
}

/// Sample peak of the decoded output audio in dBFS (None when silent or absent).
fn peak_dbfs(path: &Path) -> Result<Option<f64>> {
    let mut args = process::args(&["-hide_banner", "-nostdin", "-v", "info", "-i"]);
    args.push(path.as_os_str().to_owned());
    args.extend(process::args(&[
        "-map",
        "0:a:0?",
        "-af",
        "astats=measure_perchannel=none:measure_overall=Peak_level",
        "-f",
        "null",
        "-",
    ]));
    let out = process::run("ffmpeg", &args, Duration::from_secs(600), 4 * 1024 * 1024)?;
    let log = String::from_utf8_lossy(&out.stderr);
    Ok(log
        .lines()
        .rev()
        .filter_map(|line| line.split("Peak level dB:").nth(1))
        .filter_map(|value| value.trim().parse::<f64>().ok())
        .find(|value| value.is_finite()))
}

fn python_script(edit: &ValidatedEdit, speeds: &SpeedFilters) -> Result<String> {
    let literal = serde_json::to_string(&serde_json::to_string(&render_plan(edit, speeds)?)?)?;
    // The summary holds only fixed text and numbers; keep the comment one line.
    let stretcher = speeds.summary().replace(['\n', '\r'], " ");
    Ok(format!(
        r##"#!/usr/bin/env python3
"""Skywrite edit: Python standard library and FFmpeg only. Outputs H.264/AAC MP4.
Run: python3 render.py --output edit.mp4 [--overwrite]
"""
# Audio time-stretcher: {stretcher}
import argparse
import json
import os
import signal
import time
from pathlib import Path
import subprocess
import sys
import tempfile

PLAN = json.loads({literal})

CANCELLED = None

def cancel(signum, frame):
    # Recording covers spawn assignment and private output publication without
    # raising in either critical section. The owner observes it before publish.
    global CANCELLED
    CANCELLED = signum

def run_ffmpeg(args):
    child = None
    group_owner = os.name == 'posix'
    if group_owner and not all(hasattr(os, name) for name in ('waitid', 'WNOWAIT', 'WEXITED', 'WNOHANG', 'P_PID')):
        raise RuntimeError('safe process-group ownership requires waitid(WNOWAIT) on this platform')
    try:
        child = subprocess.Popen(['ffmpeg', *args], stdin=subprocess.DEVNULL,
                                 start_new_session=group_owner)
        deadline = time.monotonic() + PLAN['timeout_seconds']
        while True:
            if CANCELLED is not None:
                raise SystemExit(128 + CANCELLED)
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise subprocess.TimeoutExpired(child.args, PLAN['timeout_seconds'])
            if group_owner:
                # Observe without reaping: the leader PID reserves the group
                # identity until every group signal in finally has completed.
                observed = os.waitid(os.P_PID, child.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
                code = None if observed is None else (observed.si_status if observed.si_code == os.CLD_EXITED else -observed.si_status)
            else:
                # Windows Popen retains a process handle, not a recycled PGID.
                code = child.poll()
            if code is not None:
                if CANCELLED is not None:
                    raise SystemExit(128 + CANCELLED)
                if code:
                    raise subprocess.CalledProcessError(code, child.args)
                return
            time.sleep(min(0.01, remaining))
    finally:
        if child is not None:
            # Descendants may still hold the output open after the leader exits.
            # Complete group cleanup before unlinking/publishing private output.
            def terminate(sig):
                try:
                    if group_owner:
                        os.killpg(child.pid, sig)
                    elif child.poll() is None:
                        child.kill() if sig == getattr(signal, 'SIGKILL', None) else child.terminate()
                except ProcessLookupError:
                    pass
                except PermissionError:
                    # macOS reports EPERM, not ESRCH, for a group whose members
                    # are all zombies, as the WNOWAIT-held leader is after exit.
                    if not group_owner:
                        raise
            terminate(signal.SIGTERM)
            if group_owner:
                # Do not call poll/wait before the final group signal: either
                # one can release the leader identity even after normal exit.
                time.sleep(0.05)
            else:
                try:
                    child.wait(timeout=0.5)
                except subprocess.TimeoutExpired:
                    pass
            terminate(getattr(signal, 'SIGKILL', signal.SIGTERM))
            child.wait()

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True)
    parser.add_argument('--overwrite', action='store_true')
    options = parser.parse_args()
    output = Path(options.output).absolute()
    protected = [Path(__file__).resolve()] + [Path(source).resolve() for source in PLAN['sources']]
    if output.resolve() in protected:
        raise ValueError('output must not replace an input renderer script or original source media')
    if output.exists() and not options.overwrite:
        raise FileExistsError('output exists; pass --overwrite')
    handlers = {{sig: signal.signal(sig, cancel) for sig in (signal.SIGINT, signal.SIGTERM)}}
    fd, temporary = tempfile.mkstemp(prefix='.skywrite-render-', suffix='.mp4', dir=output.parent)
    os.close(fd)
    sidecars = tempfile.TemporaryDirectory(prefix='skywrite-sidecars-')
    try:
        args = PLAN['args'].copy()
        for index, sidecar in enumerate(PLAN['sidecars']):
            path = Path(sidecars.name) / sidecar['name']
            path.write_text(sidecar['text'], encoding='utf-8')
            # Only generated ASCII paths enter caption and LUT filters.
            if not all(c.isascii() and (c.isalnum() or c in '/._-') for c in str(path)):
                raise ValueError('sidecar temporary path must contain only ASCII letters, digits, /._-')
            marker = '__SKYWRITE_SIDECAR_%d__' % index
            args = [arg.replace(marker, str(path)) if arg == marker or (position > 0 and args[position-1] == '-filter_complex') else arg for position, arg in enumerate(args)]
        args[-1] = temporary
        run_ffmpeg(args)
        if CANCELLED is not None:
            raise SystemExit(128 + CANCELLED)
        if options.overwrite:
            os.replace(temporary, output)
        else:
            os.link(temporary, output)
            os.unlink(temporary)
        print(json.dumps({{'output': str(output), 'duration_seconds': PLAN['duration_seconds']}}))
    finally:
        sidecars.cleanup()
        if os.path.exists(temporary):
            os.unlink(temporary)
        for sig, handler in handlers.items():
            signal.signal(sig, handler)

if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print(json.dumps({{'error': str(error)}}), file=sys.stderr)
        sys.exit(1)
"##
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture_media(
        format: &str,
        start: f64,
        video_start: f64,
        tb: &str,
        rate: &str,
    ) -> media::MediaInfo {
        media::MediaInfo {
            path: PathBuf::from("source"),
            duration: 10.0,
            start_time: start,
            size_bytes: 1,
            format_name: format.into(),
            streams: vec![media::MediaStream {
                index: 0,
                codec_type: "video".into(),
                codec_name: Some("h264".into()),
                duration: Some(10.0),
                start_time: Some(video_start),
                width: Some(160),
                height: Some(90),
                rotation: None,
                sample_rate: None,
                channels: None,
                avg_frame_rate: Some(rate.into()),
                r_frame_rate: Some(rate.into()),
                time_base: Some(tb.into()),
                pix_fmt: None,
            }],
        }
    }

    #[test]
    fn rounded_frame_times_are_snapped_or_tolerated_and_exact_ones_kept() {
        let exact = PictureTiming {
            rounding: 0.0,
            grid: None,
        };
        for (tb, rate) in [
            ("1/15360", "30/1"),
            ("1/30000", "30000/1001"),
            ("1/24000", "24000/1001"),
            ("1/12800", "25/1"),
            ("1/1000", "25/1"),
            ("1001/30000", "30000/1001"),
            ("1/90000", "30000/1001"),
        ] {
            let info = picture_media("mov,mp4,m4a,3gp,3g2,mj2", 0.0, 0.0, tb, rate);
            assert_eq!(picture_timing(&info), exact, "{tb} {rate}");
        }
        // Matroska rounds 29.97 and 30 fps to 1 ms, MOV 29.97 to 1/600 s: half
        // a tick, snapped back to the frame grid where a frame lies that close.
        for (format, tb, rate, rounding, grid) in [
            (
                "matroska,webm",
                "1/1000",
                "30000/1001",
                0.000_5,
                (30000, 1001),
            ),
            ("matroska,webm", "1/1000", "30/1", 0.000_5, (30, 1)),
            (
                "matroska,webm",
                "1/1000",
                "60000/1001",
                0.000_5,
                (60000, 1001),
            ),
            (
                "mov,mp4,m4a,3gp,3g2,mj2",
                "1/600",
                "30000/1001",
                1.0 / 1200.0,
                (30000, 1001),
            ),
            (
                "matroska,webm",
                "1/1000",
                "24000/1001",
                0.000_5,
                (24000, 1001),
            ),
            ("avi", "1/100", "30000/1001", 0.005, (30000, 1001)),
        ] {
            let info = picture_media(format, 0.0, 0.0, tb, rate);
            let timing = picture_timing(&info);
            assert_eq!(timing.grid, Some(grid), "{tb} {rate}");
            assert!((timing.rounding - rounding).abs() < 1e-12, "{tb} {rate}");
            // A frame off the grid counts at most 0.5 ms early.
            assert!((timing.early() - rounding.min(0.000_5)).abs() < 1e-12);
        }
        // Without a time base: 0.5 ms, no grid.
        let mut unknown = picture_media("avi", 0.0, 0.0, "1/1000", "30/1");
        unknown.streams[0].time_base = None;
        assert_eq!(
            picture_timing(&unknown),
            PictureTiming {
                rounding: 0.000_5,
                grid: None
            }
        );
    }

    #[test]
    fn start_times_round_back_to_their_stream_ticks() {
        // FFprobe reports 76831/15360 s as 5.002018.
        let mut info = picture_media(
            "mov,mp4,m4a,3gp,3g2,mj2",
            5.002018,
            5.002018,
            "1/15360",
            "30/1",
        );
        let close = |a: f64, b: f64| (a - b).abs() < 1e-12;
        let (origin, picture) = exact_origins(&info);
        assert!(close(origin, 76831.0 / 15360.0), "{origin}");
        assert!(close(picture.unwrap(), 76831.0 / 15360.0));
        // Ticks finer than 2 us cannot be recovered; the reported time stays.
        info.streams[0].time_base = Some("1/1000000".into());
        assert_eq!(exact_origins(&info), (5.002018, Some(5.002018)));
        // The origin comes from the stream that starts the file; a later
        // picture keeps its own exact time.
        let mut late = picture_media("mov,mp4,m4a,3gp,3g2,mj2", 0.0, 0.021021, "1/15360", "30/1");
        late.streams.push(media::MediaStream {
            index: 1,
            codec_type: "audio".into(),
            start_time: Some(0.0),
            time_base: Some("1/48000".into()),
            ..late.streams[0].clone()
        });
        let (origin, picture) = exact_origins(&late);
        assert_eq!(origin, 0.0);
        assert!(close(picture.unwrap(), 323.0 / 15360.0));
    }

    #[test]
    fn picture_seek_starts_a_preroll_early() {
        let mp4 = picture_media("mov,mp4,m4a,3gp,3g2,mj2", 0.0, 0.0, "1/15360", "30/1");
        // A pre-roll of 1 s before source_in, in whole microseconds.
        assert_eq!(picture_seek(&mp4, 2.37), 1.37);
        assert_eq!(picture_seek(&mp4, 2.022_003_4), 1.022_003);
        // Within the first second: decode from the start.
        assert_eq!(picture_seek(&mp4, 0.6), 0.0);
        // Never before the first picture.
        let late = picture_media("mov,mp4,m4a,3gp,3g2,mj2", 0.0, 5.0, "1/1000", "25/1");
        assert_eq!(picture_seek(&late, 5.5), 5.0);
        // At or before the first picture, and MPEG-TS: from the start, as for sound.
        assert_eq!(picture_seek(&late, 4.0), 0.0);
        assert_eq!(picture_seek(&late, 0.0), 0.0);
        assert_eq!(picture_seek(&late, 5.0), 0.0);
        let ts = picture_media("mpegts", 1.4, 1.4, "1/90000", "30000/1001");
        assert_eq!(picture_seek(&ts, 3.0), 0.0);
        assert_eq!(SourceTimes::new(3.0, 0.0).prefix, 3.0);
        assert!((SourceTimes::new(2.37, 1.37).prefix - 1.0).abs() < 1e-12);
    }

    #[test]
    fn display_rotation_is_detected_and_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain.mp4");
        let rotated = dir.path().join("rotated.mp4");
        let run = |args: &[&str]| {
            let mut all = process::args(&["-v", "error", "-y"]);
            all.extend(process::args(args));
            process::run("ffmpeg", &all, Duration::from_secs(60), 1024 * 1024).unwrap();
        };
        run(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=64x36:r=10:d=0.5",
            "-c:v",
            "libx264",
            plain.to_str().unwrap(),
        ]);
        // FFmpeg 6.0+ sets the display matrix with `-display_rotation`; 5.1 has
        // no such option and writes it from the legacy `rotate` tag instead,
        // which is also the path clear_display_rotation falls back to there.
        let help = process::run(
            "ffmpeg",
            &process::args(&["-hide_banner", "-h", "full"]),
            Duration::from_secs(60),
            16 * 1024 * 1024,
        )
        .unwrap();
        if String::from_utf8_lossy(&help.stdout).contains("-display_rotation") {
            run(&[
                "-display_rotation",
                "-90",
                "-i",
                plain.to_str().unwrap(),
                "-c",
                "copy",
                rotated.to_str().unwrap(),
            ]);
        } else {
            run(&[
                "-i",
                plain.to_str().unwrap(),
                "-c",
                "copy",
                "-metadata:s:v:0",
                "rotate=-90",
                rotated.to_str().unwrap(),
            ]);
        }
        assert_eq!(display_rotation(&plain).unwrap(), None);
        assert!(display_rotation(&rotated).unwrap().is_some());
        let (cleared, _cleanup) = clear_display_rotation(&rotated, dir.path()).unwrap();
        assert_eq!(display_rotation(cleared.path()).unwrap(), None);
    }
    #[test]
    fn escaping() {
        assert_eq!(xml("a'\"<&>"), "a&apos;&quot;&lt;&amp;&gt;");
    }
}
