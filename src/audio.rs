//! Bounded local audio finishing; publication always follows a full output decode.
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const RATE: u32 = 48_000;
const MAX_SECONDS: f64 = 3_600.0;
const MAX_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_LOOPS: usize = 128;

#[derive(Debug, Subcommand)]
pub enum AudioCommand {
    /// Normalize integrated loudness (default -18 LUFS), or sample peak only.
    Normalize(FinishArgs),
    /// Change speed, preserving pitch beyond 0.1% (smaller changes resample); requires --tempo.
    Tempo(FinishArgs),
    /// Repeat a music selection with crossfades to --duration.
    Loop(FinishArgs),
    /// Attenuate music under --sidechain narration; output contains music only.
    Duck(FinishArgs),
    /// Compose trim, tempo, normalization, looping, ducking, fades and exact padding.
    Finish(FinishArgs),
    /// Put dry speech on a room-tone bed: speech loudness-normalized first, the
    /// bed looped or trimmed to it at a target level, summed without a later
    /// loudness pass; output length equals the speech.
    Roomtone(RoomtoneArgs),
    /// List capture devices this FFmpeg sees (PulseAudio/PipeWire and ALSA,
    /// macOS avfoundation, Windows dshow). Records nothing.
    Inputs,
    /// Record room tone from a capture device, only with --confirm (ask the user
    /// first): mono 48 kHz PCM16, measured and checked; never uploaded.
    RecordBed(crate::capture::RecordBedArgs),
}
#[derive(Debug, Args)]
pub struct RoomtoneArgs {
    /// Dry speech, for example a generated narration take.
    pub input: PathBuf,
    /// Room tone: a recording of the room with nobody talking (see `audio record-bed`).
    #[arg(long)]
    pub bed: PathBuf,
    #[arg(short, long)]
    pub output: PathBuf,
    #[arg(long)]
    pub overwrite: bool,
    /// Speech integrated loudness before mixing, -50 through -5 LUFS (two-pass loudnorm).
    #[arg(long, default_value_t = -18.0, allow_hyphen_values = true)]
    pub lufs: f64,
    /// Speech true-peak ceiling, -20 through 0 dBTP.
    #[arg(long, default_value_t = -2.0, allow_hyphen_values = true)]
    pub peak_db: f64,
    #[arg(long, default_value_t = 7.0)]
    pub loudness_range: f64,
    /// Bed RMS level in the mix, -80 through -40 dBFS, measured from the bed
    /// (after --bed-highpass); any bed lands at this floor.
    #[arg(long, default_value_t = -58.0, allow_hyphen_values = true)]
    pub bed_dbfs: f64,
    /// High-pass the bed at this frequency (20 through 400 Hz), for example 120
    /// against machine hum.
    #[arg(long)]
    pub bed_highpass: Option<f64>,
    /// Crossfade where a short bed loops, 0 through 30 seconds.
    #[arg(long, default_value_t = 0.5)]
    pub crossfade: f64,
}
/// A bed louder than this is not room tone (speech, music or a loud noise).
const BED_MAX_INPUT_DBFS: f64 = -30.0;
/// The most gain a quiet bed may get to reach its target level.
const BED_MAX_GAIN_DB: f64 = 40.0;
/// Shortest usable bed recording.
const BED_MIN_SECONDS: f64 = 1.0;
#[derive(Debug, Args)]
pub struct FinishArgs {
    /// Audio to finish. Output is 48-kHz stereo: a mono input is converted to
    /// stereo at -3 dB per channel (equal power) during preparation, before
    /// level targets, looping, ducking and fades.
    pub input: PathBuf,
    #[arg(short, long)]
    pub output: PathBuf,
    #[arg(long)]
    pub overwrite: bool,
    /// Seconds from the first decoded audio sample, not container timestamps.
    #[arg(long, default_value_t = 0.0)]
    pub start: f64,
    /// Source selection length before tempo; defaults to remaining decoded samples.
    #[arg(long)]
    pub length: Option<f64>,
    /// Pitch-preserving speed multiplier, 0.25 through 4. Within 0.1% of 1 it
    /// resamples sample-exactly; beyond that it time-stretches (see --stretcher).
    #[arg(long)]
    pub tempo: Option<f64>,
    /// Time-stretcher for --tempo beyond 0.1%: auto uses rubberband when this
    /// FFmpeg has it (latency-compensated, within about 6 ms (8 ms at 0.125x)), else atempo with a
    /// warning (3-36 ms early at 0.5-2x, up to about 60 ms below 0.5x); rubberband fails if it is missing.
    #[arg(long, value_enum, default_value_t = crate::stretch::StretcherChoice::Auto)]
    pub stretcher: crate::stretch::StretcherChoice,
    /// Integrated loudness target, -50 through -5 LUFS; two-pass loudnorm.
    #[arg(long, allow_hyphen_values = true)]
    pub lufs: Option<f64>,
    /// With --lufs: true-peak ceiling. Alone: sample-peak normalization (-20 to 0 dB).
    #[arg(long, allow_hyphen_values = true)]
    pub peak_db: Option<f64>,
    #[arg(long, default_value_t = 7.0)]
    pub loudness_range: f64,
    /// Exact final duration; pad silence before trimming.
    #[arg(long)]
    pub duration: Option<f64>,
    #[arg(long, requires = "duration")]
    pub r#loop: bool,
    #[arg(long, default_value_t = 0.25)]
    pub crossfade: f64,
    /// Narration controls gain; first decoded sample aligns with output second zero.
    #[arg(long)]
    pub sidechain: Option<PathBuf>,
    #[arg(long, default_value_t = 0.035)]
    pub threshold: f64,
    #[arg(long, default_value_t = 4.0)]
    pub ratio: f64,
    #[arg(long, default_value_t = 15.0)]
    pub attack_ms: f64,
    #[arg(long, default_value_t = 600.0)]
    pub release_ms: f64,
    #[arg(long, default_value_t = 0.0)]
    pub fade_in: f64,
    #[arg(long, default_value_t = 0.0)]
    pub fade_out: f64,
}
fn bounded(name: &str, value: f64, min: f64, max: f64) -> Result<()> {
    if !value.is_finite() || value < min || value > max {
        bail!("{name} must be finite and between {min} and {max}");
    }
    Ok(())
}
fn samples(seconds: f64) -> u64 {
    (seconds * RATE as f64).round() as u64
}
fn base_args() -> Vec<OsString> {
    crate::process::args(&[
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-xerror",
        "-y",
        "-threads",
        "1",
    ])
}
fn input(args: &mut Vec<OsString>, path: &Path) {
    args.extend(crate::process::args(&[
        "-protocol_whitelist",
        "file,pipe",
        "-i",
    ]));
    args.push(path.as_os_str().to_owned());
}
fn wav_output(args: &mut Vec<OsString>, path: &Path) {
    args.extend(crate::process::args(&[
        "-vn",
        "-sn",
        "-dn",
        "-threads",
        "1",
        "-ar",
        "48000",
        "-ac",
        "2",
        "-c:a",
        "pcm_s16le",
        "-f",
        "wav",
        "-fs",
        "1073741824",
    ]));
    args.push(path.as_os_str().to_owned());
}
fn run(args: &[OsString], deadline: Instant) -> Result<std::process::Output> {
    let left = deadline
        .checked_duration_since(Instant::now())
        .context("audio finishing deadline exceeded")?;
    crate::process::run(
        "ffmpeg",
        args,
        left.min(Duration::from_secs(600)),
        1024 * 1024,
    )
}
fn filter_file(source: &Path, output: &Path, filter: &str, deadline: Instant) -> Result<()> {
    let mut args = base_args();
    input(&mut args, source);
    args.extend(crate::process::args(&[
        "-map",
        "0:a:0",
        "-filter_threads",
        "1",
        "-af",
        filter,
    ]));
    wav_output(&mut args, output);
    run(&args, deadline)?;
    Ok(())
}
/// Crossfade shape at loop seams.
#[derive(Clone, Copy)]
enum Seam {
    /// Linear (triangular) gain: constant amplitude for correlated music.
    Linear,
    /// Quarter-sine gain: constant power for uncorrelated noise such as room tone.
    EqualPower,
}
/// `repetitions` copies of `source` (`content_samples` long), joined with
/// `seam` crossfades of `crossfade_samples` (plain concatenation at zero), cut
/// at `duration`.
///
/// Each copy is faded and delayed to its own position, then the copies are
/// summed by `amix`. Chained `acrossfade` is not used: before FFmpeg 8.0 it
/// ends its output as soon as its second input reaches end of stream, even if
/// the first is still being read, so a short source looped through
/// FFmpeg 7.x's threaded CLI was cut short at random points.
fn loop_file(
    source: &Path,
    output: &Path,
    repetitions: usize,
    content_samples: u64,
    (crossfade_samples, seam): (u64, Seam),
    duration: f64,
    deadline: Instant,
) -> Result<()> {
    let curve = match seam {
        Seam::Linear => "tri",
        Seam::EqualPower => "qsin",
    };
    let mut args = base_args();
    for _ in 0..repetitions {
        input(&mut args, source);
    }
    let mut graph = String::new();
    if crossfade_samples == 0 {
        for i in 0..repetitions {
            graph.push_str(&format!("[{i}:a:0]"));
        }
        graph.push_str(&format!("concat=n={repetitions}:v=0:a=1[out]"));
    } else {
        // Copy `i` starts at `i * stride`; its last `crossfade_samples` fade out.
        let stride = content_samples - crossfade_samples;
        for i in 0..repetitions {
            graph.push_str(&format!("[{i}:a:0]asetpts=N/SR/TB"));
            if i > 0 {
                graph.push_str(&format!(
                    ",afade=t=in:ss=0:ns={crossfade_samples}:curve={curve}"
                ));
            }
            if i + 1 < repetitions {
                graph.push_str(&format!(
                    ",afade=t=out:ss={stride}:ns={crossfade_samples}:curve={curve}"
                ));
            }
            if i > 0 {
                graph.push_str(&format!(",adelay={}S:all=1", i as u64 * stride));
            }
            graph.push_str(&format!("[c{i}];"));
        }
        for i in 0..repetitions {
            graph.push_str(&format!("[c{i}]"));
        }
        graph.push_str(&format!(
            "amix=inputs={repetitions}:duration=longest:dropout_transition=0:normalize=0[out]"
        ));
    }
    args.extend(crate::process::args(&[
        "-filter_complex_threads",
        "1",
        "-filter_complex",
        &graph,
        "-map",
        "[out]",
        "-t",
        &duration.to_string(),
    ]));
    wav_output(&mut args, output);
    run(&args, deadline)?;
    Ok(())
}
/// Loop repetitions needed to reach `target` samples from `content` samples.
fn repetitions_for(content: u64, target: u64, crossfade_samples: u64) -> Result<usize> {
    if target <= content {
        return Ok(1);
    }
    if crossfade_samples * 2 >= content {
        bail!("crossfade must be shorter than half the prepared loop source");
    }
    let stride = content - crossfade_samples;
    let repetitions = 1 + (target - content).div_ceil(stride) as usize;
    if repetitions > MAX_LOOPS {
        bail!("loop needs {repetitions} repetitions; maximum is {MAX_LOOPS}");
    }
    Ok(repetitions)
}
fn checked_timing(path: &Path) -> Result<crate::media::AudioTiming> {
    let info = crate::media::probe(path)?;
    if info.size_bytes > MAX_BYTES {
        bail!("audio finishing input/output exceeds one GiB");
    }
    let audio = info.audio().context("media has no audio stream")?;
    if audio.channels.is_none_or(|n| n == 0 || n > 8)
        || audio.sample_rate.is_none_or(|n| n == 0 || n > 192_000)
    {
        bail!("audio finishing accepts at most eight channels and 192 kHz input");
    }
    let timing = crate::media::exact_audio_timing(&info, None)?;
    if timing.decoded_duration_seconds > MAX_SECONDS {
        bail!("audio finishing input/output exceeds one hour of decoded audio");
    }
    Ok(timing)
}
/// EBU R128 measurements of the first audio stream (FFmpeg `loudnorm`
/// analysis: `input_i` LUFS, `input_tp` dBTP, `input_lra` LU, `input_thresh`),
/// as the strings loudnorm prints; digital silence reports `-inf`.
pub(crate) fn loudness_stats(
    path: &Path,
    target: f64,
    peak: f64,
    range: f64,
    deadline: Instant,
) -> Result<Value> {
    let mut args = base_args();
    args.extend(crate::process::args(&["-loglevel", "info"]));
    input(&mut args, path);
    args.extend(crate::process::args(&[
        "-map",
        "0:a:0",
        "-filter_threads",
        "1",
        "-af",
        &format!("loudnorm=I={target}:TP={peak}:LRA={range}:print_format=json"),
        "-f",
        "null",
        "-",
    ]));
    let out = run(&args, deadline)?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let start = stderr
        .rfind('{')
        .context("loudnorm did not report measurements")?;
    let end = stderr
        .rfind('}')
        .context("loudnorm measurement was incomplete")?;
    serde_json::from_str(&stderr[start..=end]).context("invalid loudnorm measurements")
}
/// Overall RMS and sample peak (dBFS) of `source` after `filter`, decoded in
/// floating point without writing a file. `None` for digital silence.
fn levels(source: &Path, filter: &str, deadline: Instant) -> Result<(Option<f64>, Option<f64>)> {
    let mut args = base_args();
    args.extend(crate::process::args(&["-loglevel", "info"]));
    input(&mut args, source);
    args.extend(crate::process::args(&[
        "-map",
        "0:a:0",
        "-filter_threads",
        "1",
        "-af",
        &format!(
            "{filter},astats=metadata=0:reset=0:measure_perchannel=none:measure_overall=RMS_level+Peak_level"
        ),
        "-f",
        "null",
        "-",
    ]));
    let out = run(&args, deadline)?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let value = |name: &str| -> Result<Option<f64>> {
        let text = stderr
            .lines()
            .filter_map(|line| line.split_once(name).map(|(_, v)| v.trim().to_owned()))
            .next_back()
            .with_context(|| format!("level measurement missing {name}"))?;
        // astats reports digital silence as -inf.
        Ok(text.parse::<f64>().ok().filter(|v| v.is_finite()))
    };
    Ok((value("RMS level dB:")?, value("Peak level dB:")?))
}
/// Two-pass linear loudnorm of `source` into `output`. `None` (nothing written)
/// when the measurements are not finite, as for digital silence.
fn two_pass_loudnorm(
    source: &Path,
    output: &Path,
    target: f64,
    peak: f64,
    range: f64,
    deadline: Instant,
) -> Result<Option<Value>> {
    let stats = loudness_stats(source, target, peak, range, deadline)?;
    let (Some(i), Some(tp), Some(lra), Some(thresh), Some(offset)) = (
        stat(&stats, "input_i"),
        stat(&stats, "input_tp"),
        stat(&stats, "input_lra"),
        stat(&stats, "input_thresh"),
        stat(&stats, "target_offset"),
    ) else {
        return Ok(None);
    };
    filter_file(
        source,
        output,
        &format!(
            "loudnorm=I={target}:TP={peak}:LRA={range}:measured_I={i}:measured_TP={tp}:measured_LRA={lra}:measured_thresh={thresh}:offset={offset}:linear=true,aresample={RATE}"
        ),
        deadline,
    )?;
    Ok(Some(stats))
}
fn stat(stats: &Value, key: &str) -> Option<f64> {
    stats[key]
        .as_str()?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}
pub fn execute(command: AudioCommand) -> Result<Value> {
    let (mut options, operation) = match command {
        AudioCommand::Normalize(mut args) => {
            if args.lufs.is_none() && args.peak_db.is_none() {
                args.lufs = Some(-18.0);
            }
            (args, "normalize")
        }
        AudioCommand::Tempo(args) => {
            if args.tempo.is_none() {
                bail!("audio tempo requires --tempo");
            }
            (args, "tempo")
        }
        AudioCommand::Loop(mut args) => {
            if args.duration.is_none() {
                bail!("audio loop requires --duration");
            }
            args.r#loop = true;
            (args, "loop")
        }
        AudioCommand::Duck(args) => {
            if args.sidechain.is_none() {
                bail!("audio duck requires --sidechain");
            }
            (args, "duck")
        }
        AudioCommand::Finish(args) => (args, "finish"),
        AudioCommand::Roomtone(args) => return roomtone(args),
        AudioCommand::Inputs => return crate::capture::inputs(),
        AudioCommand::RecordBed(args) => return crate::capture::record_bed(args),
    };
    bounded("start", options.start, 0.0, MAX_SECONDS)?;
    for (name, value) in [("length", options.length), ("duration", options.duration)] {
        if let Some(v) = value {
            bounded(name, v, 1.0 / RATE as f64, MAX_SECONDS)?;
        }
    }
    bounded("tempo", options.tempo.unwrap_or(1.0), 0.25, 4.0)?;
    if let Some(v) = options.lufs {
        bounded("lufs", v, -50.0, -5.0)?;
    }
    if let Some(v) = options.peak_db {
        bounded("peak-db", v, -20.0, 0.0)?;
    }
    bounded("loudness-range", options.loudness_range, 1.0, 20.0)?;
    bounded("crossfade", options.crossfade, 0.0, 30.0)?;
    bounded("threshold", options.threshold, 0.000976, 1.0)?;
    bounded("ratio", options.ratio, 1.0, 20.0)?;
    bounded("attack-ms", options.attack_ms, 0.01, 2000.0)?;
    bounded("release-ms", options.release_ms, 0.01, 9000.0)?;
    bounded("fade-in", options.fade_in, 0.0, MAX_SECONDS)?;
    bounded("fade-out", options.fade_out, 0.0, MAX_SECONDS)?;
    if options.r#loop && options.duration.is_none() {
        bail!("looping requires --duration");
    }
    let extension = options
        .output
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(extension.as_str(), "wav" | "mp3") {
        bail!("audio output requires .wav or .mp3");
    }
    let source = crate::media::canonical_file(&options.input)?;
    let mut consumed = vec![source.clone()];
    if let Some(side) = &options.sidechain {
        let side = crate::media::canonical_file(side)?;
        consumed.push(side.clone());
        options.sidechain = Some(side);
    }
    for path in &consumed {
        if std::fs::metadata(path)?.len() > MAX_BYTES {
            bail!("audio finishing input exceeds one GiB");
        }
    }
    crate::io::protect_inputs(&options.output, &consumed)?;
    if std::fs::symlink_metadata(&options.output).is_ok() && !options.overwrite {
        bail!("output exists; use --overwrite to replace it");
    }
    let deadline = Instant::now() + Duration::from_secs(900);
    let source_info = crate::media::probe_for_timing(&source)?;
    let source_timing = checked_timing(&source)?;
    let side_timing = options
        .sidechain
        .as_deref()
        .map(checked_timing)
        .transpose()?;
    let start_samples = samples(options.start);
    let available_samples = samples(source_timing.decoded_duration_seconds);
    if start_samples >= available_samples {
        bail!("start is beyond the decoded audio");
    }
    let selection_samples = options
        .length
        .map(samples)
        .unwrap_or(available_samples - start_samples);
    if start_samples + selection_samples > available_samples + 1 {
        bail!("source selection exceeds decoded audio duration");
    }
    if selection_samples == 0 {
        bail!("source selection rounds to zero samples");
    }
    let prepared_estimate = selection_samples as f64 / RATE as f64 / options.tempo.unwrap_or(1.0);
    if prepared_estimate > MAX_SECONDS {
        bail!("tempo-adjusted source exceeds one hour");
    }
    let parent = options
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let workspace = tempfile::Builder::new()
        .prefix(".skywrite-audio-")
        .tempdir_in(parent)?;
    let _workspace_cleanup = crate::process::CleanupGuard::new(workspace.path())?;
    let prepared = workspace.path().join("prepared.wav");
    let mut filter = format!(
        "asetpts=N/SR/TB,aresample={RATE},atrim=start_sample={start_samples}:end_sample={},asetpts=N/SR/TB",
        start_samples + selection_samples
    );
    let speed_filters = crate::stretch::SpeedFilters::resolve(options.stretcher, options.tempo)?;
    if let Some(speed) = options.tempo {
        filter.push(',');
        filter.push_str(&speed_filters.filter(speed, selection_samples, "tempo")?);
    }
    filter_file(&source, &prepared, &filter, deadline)?;
    let prepared_timing = checked_timing(&prepared)?;
    let mut current = prepared;
    let mut replaced = false;
    let mut normalization = Value::Null;
    if let Some(target) = options.lufs {
        let peak = options.peak_db.unwrap_or(-2.0);
        let normalized = workspace.path().join("normalized.wav");
        if let Some(stats) = two_pass_loudnorm(
            &current,
            &normalized,
            target,
            peak,
            options.loudness_range,
            deadline,
        )? {
            current = normalized;
            replaced = true;
            normalization = json!({"method":"two_pass_loudnorm","target_lufs":target,"true_peak_ceiling_db":peak,"input_measurements":stats});
        } else if prepared_timing.peak_dbfs.is_none() {
            normalization = json!({"method":"silent_input_unchanged","target_achievable":false});
        } else {
            bail!("loudnorm returned nonfinite measurements for nonsilent input");
        }
    } else if let Some(target) = options.peak_db {
        if let Some(peak) = prepared_timing.peak_dbfs {
            let normalized = workspace.path().join("normalized.wav");
            filter_file(
                &current,
                &normalized,
                &format!("volume={}dB", target - peak),
                deadline,
            )?;
            current = normalized;
            replaced = true;
            normalization =
                json!({"method":"sample_peak","target_dbfs":target,"input_peak_dbfs":peak});
        } else {
            normalization = json!({"method":"silent_input_unchanged","target_achievable":false});
        }
    }
    // An unreplaced `current` is still the file `prepared_timing` measured.
    let content_timing = if replaced {
        checked_timing(&current)?
    } else {
        prepared_timing
    };
    let target_samples = options
        .duration
        .map(samples)
        .unwrap_or(samples(content_timing.decoded_duration_seconds));
    if target_samples == 0 {
        bail!("output duration rounds to zero samples");
    }
    let final_duration = target_samples as f64 / RATE as f64;
    if options.fade_in > final_duration || options.fade_out > final_duration {
        bail!("fades cannot exceed the final duration");
    }
    let mut repetitions = 1;
    let crossfade_samples = samples(options.crossfade);
    if options.r#loop && target_samples > content_timing.samples {
        repetitions = repetitions_for(content_timing.samples, target_samples, crossfade_samples)?;
        let looped = workspace.path().join("looped.wav");
        loop_file(
            &current,
            &looped,
            repetitions,
            content_timing.samples,
            (crossfade_samples, Seam::Linear),
            final_duration,
            deadline,
        )?;
        current = looped;
    }
    if let Some(sidechain) = &options.sidechain {
        let ducked = workspace.path().join("ducked.wav");
        let mut args = base_args();
        input(&mut args, &current);
        input(&mut args, sidechain);
        // A short narration must never shorten the music. sidechaincompress
        // output depends on how its inputs are split into frames, which on
        // FFmpeg 7.0+ varies with thread timing; fixed 1024-sample frames on
        // both inputs make the result the same on every run and version.
        let graph = format!(
            "[0:a:0]asetnsamples=n=1024:p=0[music];[1:a:0]asetpts=N/SR/TB,aresample={RATE},apad,atrim=end_sample={target_samples},asetnsamples=n=1024:p=0[voice];[music][voice]sidechaincompress=threshold={}:ratio={}:attack={}:release={}:makeup=1[out]",
            options.threshold, options.ratio, options.attack_ms, options.release_ms
        );
        args.extend(crate::process::args(&[
            "-filter_complex_threads",
            "1",
            "-filter_complex",
            &graph,
            "-map",
            "[out]",
            "-t",
            &final_duration.to_string(),
        ]));
        wav_output(&mut args, &ducked);
        run(&args, deadline)?;
        current = ducked;
    }
    let final_path = workspace.path().join(format!("finished.{extension}"));
    encode_final(
        &current,
        &final_path,
        &extension,
        target_samples,
        (samples(options.fade_in), samples(options.fade_out)),
        deadline,
    )?;
    let mut identities = vec![(source.clone(), source_timing.source_sha256.clone())];
    if let (Some(side), Some(timing)) = (&options.sidechain, &side_timing) {
        identities.push((side.clone(), timing.source_sha256.clone()));
    }
    let (output_timing, codec) = publish_verified(
        &final_path,
        &extension,
        target_samples,
        &identities,
        &options.output,
        options.overwrite,
        parent,
    )?;
    let codec = codec.as_deref();
    Ok(
        json!({"version":1,"operation":operation,"output":options.output,"format":extension,"codec":codec,"sample_rate":RATE,"channels":2,"source":source,"source_sha256":source_timing.source_sha256,"source_timing":source_timing,"source_selection":{"time_basis":"seconds from first decoded audio sample; output rebased to zero","container_start_seconds":source_info.start_time,"audio_stream_start_seconds":source_info.audio().and_then(|s|s.start_time),"requested_start_seconds":options.start,"start_samples_48000":start_samples,"length_samples_48000":selection_samples,"tempo":options.tempo.unwrap_or(1.0)},"audio_stretcher":speed_filters.report(),"warnings":speed_filters.warnings(),"normalization":normalization,"loop_repetitions":repetitions,"crossfade_samples":crossfade_samples,"sidechain":options.sidechain,"sidechain_timing":side_timing,"requested_duration_seconds":options.duration,"rounded_target_samples":target_samples,"actual_duration_seconds":output_timing.decoded_duration_seconds,"rounding":"nearest sample at 48000 Hz; exact decoded sample count checked","output_timing":output_timing,"full_output_decode_executed":true,"note":"Normalization precedes loops, ducking and fades; these can change final loudness. Duck output contains the music only."}),
    )
}

fn roomtone(options: RoomtoneArgs) -> Result<Value> {
    bounded("lufs", options.lufs, -50.0, -5.0)?;
    bounded("peak-db", options.peak_db, -20.0, 0.0)?;
    bounded("loudness-range", options.loudness_range, 1.0, 20.0)?;
    bounded("bed-dbfs", options.bed_dbfs, -80.0, -40.0)?;
    bounded("crossfade", options.crossfade, 0.0, 30.0)?;
    if let Some(hz) = options.bed_highpass {
        bounded("bed-highpass", hz, 20.0, 400.0)?;
    }
    let extension = options
        .output
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(extension.as_str(), "wav" | "mp3") {
        bail!("audio output requires .wav or .mp3");
    }
    let speech = crate::media::canonical_file(&options.input)
        .with_context(|| format!("speech input {}", options.input.display()))?;
    let bed = crate::media::canonical_file(&options.bed).with_context(|| {
        format!(
            "room-tone bed {} not found or unreadable; pass a recording of the room with nobody talking (`skywrite audio record-bed`, after asking the user)",
            options.bed.display()
        )
    })?;
    if speech == bed {
        bail!("the bed must be a separate room-tone recording, not the speech itself");
    }
    let consumed = vec![speech.clone(), bed.clone()];
    for path in &consumed {
        if std::fs::metadata(path)?.len() > MAX_BYTES {
            bail!("audio finishing input exceeds one GiB");
        }
    }
    crate::io::protect_inputs(&options.output, &consumed)?;
    if std::fs::symlink_metadata(&options.output).is_ok() && !options.overwrite {
        bail!("output exists; use --overwrite to replace it");
    }
    let deadline = Instant::now() + Duration::from_secs(900);
    let speech_timing = checked_timing(&speech)?;
    let bed_timing = checked_timing(&bed).context("room-tone bed")?;
    if bed_timing.decoded_duration_seconds < BED_MIN_SECONDS {
        bail!(
            "room-tone bed is {:.3} s; record at least {BED_MIN_SECONDS} s (30 s is typical)",
            bed_timing.decoded_duration_seconds
        );
    }
    // The deliverable has exactly the dry speech's duration.
    let target_samples = samples(speech_timing.decoded_duration_seconds);
    if target_samples == 0 {
        bail!("speech rounds to zero samples");
    }
    let duration = target_samples as f64 / RATE as f64;
    let parent = options
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let workspace = tempfile::Builder::new()
        .prefix(".skywrite-audio-")
        .tempdir_in(parent)?;
    let _workspace_cleanup = crate::process::CleanupGuard::new(workspace.path())?;
    let work = |name: &str| workspace.path().join(name);

    // 1. Speech first: 48 kHz stereo, two-pass loudness normalization.
    filter_file(
        &speech,
        &work("speech.wav"),
        &format!("asetpts=N/SR/TB,aresample={RATE}"),
        deadline,
    )?;
    let Some(speech_stats) = two_pass_loudnorm(
        &work("speech.wav"),
        &work("speech-normalized.wav"),
        options.lufs,
        options.peak_db,
        options.loudness_range,
        deadline,
    )?
    else {
        if speech_timing.peak_dbfs.is_none() {
            bail!("speech is digital silence; there is nothing to put on a bed");
        }
        bail!(
            "speech loudness could not be measured: integrated loudness needs at least 0.4 s of signal above the gating threshold ({:.3} s decoded); use a longer take",
            speech_timing.decoded_duration_seconds
        );
    };
    let normalized = loudness_stats(
        &work("speech-normalized.wav"),
        options.lufs,
        options.peak_db,
        options.loudness_range,
        deadline,
    )?;

    // 2. Bed: same rate and layout, optional high-pass; its level is measured
    // before any 16-bit write so the gain never amplifies quantization noise.
    // Double precision and the output's stereo layout (a mono bed is upmixed at
    // -3 dB per channel) are fixed here, so the measured level is the mixed one.
    let mut bed_filter = format!(
        "aformat=sample_fmts=dbl,asetpts=N/SR/TB,aresample={RATE},aformat=sample_fmts=dbl:channel_layouts=stereo"
    );
    if let Some(hz) = options.bed_highpass {
        bed_filter.push_str(&format!(",highpass=f={hz}:poles=2"));
    }
    // Level only what is mixed: a bed longer than the speech is trimmed, so
    // its discarded tail must not set the gain. A looped bed is mixed whole.
    let bed_available = samples(bed_timing.decoded_duration_seconds);
    let measured_filter = if bed_available > target_samples {
        format!("{bed_filter},atrim=end_sample={target_samples}")
    } else {
        bed_filter.clone()
    };
    let (bed_rms, bed_peak) = levels(&bed, &measured_filter, deadline)?;
    let bed_rms =
        bed_rms.context("room-tone bed is digital silence; record the room itself, not silence")?;
    if bed_rms > BED_MAX_INPUT_DBFS {
        bail!(
            "room-tone bed RMS is {bed_rms:.1} dBFS, too loud for room tone (limit {BED_MAX_INPUT_DBFS} dBFS): it likely holds speech, music or a loud noise. Record the room in silence"
        );
    }
    let gain = options.bed_dbfs - bed_rms;
    if gain > BED_MAX_GAIN_DB {
        bail!(
            "room-tone bed RMS is {bed_rms:.1} dBFS; reaching {} dBFS needs {gain:+.1} dB (limit +{BED_MAX_GAIN_DB} dB), which would mostly amplify converter noise. Record closer to the room's real level or lower --bed-dbfs",
            options.bed_dbfs
        );
    }
    filter_file(
        &bed,
        &work("bed.wav"),
        &format!("{bed_filter},volume={gain:.6}dB"),
        deadline,
    )?;
    let bed_prepared = checked_timing(&work("bed.wav"))?;
    let crossfade_samples = samples(options.crossfade);
    let repetitions = repetitions_for(bed_prepared.samples, target_samples, crossfade_samples)?;
    let mut bed_source = work("bed.wav");
    if repetitions > 1 {
        loop_file(
            &bed_source,
            &work("bed-looped.wav"),
            repetitions,
            bed_prepared.samples,
            (crossfade_samples, Seam::EqualPower),
            duration,
            deadline,
        )?;
        bed_source = work("bed-looped.wav");
    }
    filter_file(
        &bed_source,
        &work("bed-fitted.wav"),
        &format!("apad,atrim=end_sample={target_samples},asetpts=N/SR/TB"),
        deadline,
    )?;
    let bed_fitted = checked_timing(&work("bed-fitted.wav"))?;
    let Some(fitted_level) = bed_fitted.rms_dbfs else {
        bail!("the fitted room-tone bed is digital silence; record the room itself");
    };

    // 3. Sum without normalization; no loudness pass after the mix.
    let mut args = base_args();
    input(&mut args, &work("speech-normalized.wav"));
    input(&mut args, &work("bed-fitted.wav"));
    args.extend(crate::process::args(&[
        "-filter_complex_threads",
        "1",
        "-filter_complex",
        "[0:a:0][1:a:0]amix=inputs=2:duration=first:dropout_transition=0:normalize=0[out]",
        "-map",
        "[out]",
    ]));
    wav_output(&mut args, &work("mixed.wav"));
    run(&args, deadline)?;
    let final_path = work(&format!("finished.{extension}"));
    encode_final(
        &work("mixed.wav"),
        &final_path,
        &extension,
        target_samples,
        (0, 0),
        deadline,
    )?;
    let mixed = loudness_stats(
        &final_path,
        options.lufs,
        options.peak_db,
        options.loudness_range,
        deadline,
    )?;
    let true_peak = stat(&mixed, "input_tp");
    let mut warnings = Vec::new();
    if let Some(tp) = true_peak
        && tp > options.peak_db + 0.1
    {
        warnings.push(format!(
            "mixed true peak {tp:.2} dBTP is above the {} dBTP ceiling (the bed adds to speech peaks; MP3 encoding can add more)",
            options.peak_db
        ));
    }
    if (fitted_level - options.bed_dbfs).abs() > 1.0 {
        warnings.push(format!(
            "the bed measures {fitted_level:.1} dBFS in the mix, not {} dBFS: its level changes over time (or 16-bit rounding of a very quiet bed); listen, or record a steadier bed",
            options.bed_dbfs
        ));
    }
    if repetitions > 1 {
        warnings.push(format!(
            "the {:.1} s bed was looped {repetitions} times with {} s crossfades to cover {duration:.1} s; a longer recording avoids an audible pattern",
            bed_timing.decoded_duration_seconds, options.crossfade
        ));
    }
    let (output_timing, codec) = publish_verified(
        &final_path,
        &extension,
        target_samples,
        &[
            (speech.clone(), speech_timing.source_sha256.clone()),
            (bed.clone(), bed_timing.source_sha256.clone()),
        ],
        &options.output,
        options.overwrite,
        parent,
    )?;
    Ok(json!({
        "version":1,"operation":"roomtone","output":options.output,"format":extension,"codec":codec,
        "sample_rate":RATE,"channels":2,
        "speech":{
            "source":speech,"source_sha256":speech_timing.source_sha256,"source_timing":speech_timing,
            "normalization":{"method":"two_pass_loudnorm","target_lufs":options.lufs,"true_peak_ceiling_db":options.peak_db,"input_measurements":speech_stats},
            "measured_lufs":stat(&normalized, "input_i"),"measured_true_peak_dbtp":stat(&normalized, "input_tp"),
        },
        "bed":{
            "source":bed,"source_sha256":bed_timing.source_sha256,"source_timing":bed_timing,
            "highpass_hz":options.bed_highpass,"measured_rms_dbfs":bed_rms,"measured_peak_dbfs":bed_peak,
            "target_rms_dbfs":options.bed_dbfs,"applied_gain_db":gain,"level_in_mix_rms_dbfs":bed_fitted.rms_dbfs,
            "loop_repetitions":repetitions,"crossfade_samples":crossfade_samples,
            "trimmed":repetitions == 1 && bed_prepared.samples > target_samples,
        },
        "mix":{"method":"amix normalize=0 (plain sum); no loudness normalization after mixing","integrated_lufs":stat(&mixed, "input_i"),"true_peak_dbtp":true_peak},
        "rounded_target_samples":target_samples,"actual_duration_seconds":output_timing.decoded_duration_seconds,
        "output_timing":output_timing,"full_output_decode_executed":true,"warnings":warnings,
        "note":"Speech is normalized before the bed is added, so integrated loudness rises slightly in pauses-heavy takes; the output has exactly the speech's duration. No reverb or impulse response is applied."
    }))
}
/// The 48-kHz stereo deliverable: padded, then cut to exactly `target_samples`,
/// with optional sample-counted fades.
fn encode_final(
    source: &Path,
    output: &Path,
    extension: &str,
    target_samples: u64,
    (fade_in, fade_out): (u64, u64),
    deadline: Instant,
) -> Result<()> {
    // Pad before trim: tempo/filter tails cannot leave 179.2s for a 180s request.
    let mut final_filter =
        format!("aresample={RATE},apad,atrim=end_sample={target_samples},asetpts=N/SR/TB");
    if fade_in > 0 {
        final_filter.push_str(&format!(",afade=t=in:ss=0:ns={fade_in}"));
    }
    if fade_out > 0 {
        final_filter.push_str(&format!(
            ",afade=t=out:ss={}:ns={fade_out}",
            target_samples - fade_out
        ));
    }
    let mut args = base_args();
    input(&mut args, source);
    args.extend(crate::process::args(&[
        "-map",
        "0:a:0",
        "-vn",
        "-sn",
        "-dn",
        "-filter_threads",
        "1",
        "-af",
        &final_filter,
        "-threads",
        "1",
        "-ar",
        "48000",
        "-ac",
        "2",
        "-fs",
        "1073741824",
    ]));
    if extension == "wav" {
        args.extend(crate::process::args(&["-c:a", "pcm_s16le", "-f", "wav"]));
    } else {
        args.extend(crate::process::args(&[
            "-c:a",
            "libmp3lame",
            "-b:a",
            "192k",
            "-write_xing",
            "1",
            "-f",
            "mp3",
        ]));
    }
    args.push(output.as_os_str().to_owned());
    run(&args, deadline)?;
    Ok(())
}
/// Decodes the finished file afresh, checks its sample count and codec, rechecks
/// every consumed input's identity, then publishes it atomically.
fn publish_verified(
    finished: &Path,
    extension: &str,
    target_samples: u64,
    identities: &[(PathBuf, String)],
    output: &Path,
    overwrite: bool,
    parent: &Path,
) -> Result<(crate::media::AudioTiming, Option<String>)> {
    // Publication requires a fresh decode, even if these exact bytes were cached before.
    let output_timing = crate::timing::measure_audio(&crate::media::probe(finished)?)?;
    if output_timing.samples != target_samples || output_timing.sample_rate != RATE {
        bail!(
            "decoded output duration mismatch: requested {target_samples} samples, decoded {} at {} Hz",
            output_timing.samples,
            output_timing.sample_rate
        );
    }
    let output_info = crate::media::probe(finished)?;
    let codec = output_info.audio().and_then(|a| a.codec_name.clone());
    if (extension == "wav" && codec.as_deref() != Some("pcm_s16le"))
        || (extension == "mp3" && codec.as_deref() != Some("mp3"))
    {
        bail!("encoded audio format does not match requested extension");
    }
    // Recheck every source identity before the single publication boundary.
    for (path, hash) in identities {
        if !source_identity_matches(path, hash, MAX_BYTES)? {
            bail!("input {} changed during audio finishing", path.display());
        }
    }
    let consumed: Vec<PathBuf> = identities.iter().map(|(p, _)| p.clone()).collect();
    crate::io::protect_inputs(output, &consumed)?;
    crate::io::publish_copy(finished, output, overwrite, parent)?;
    Ok((output_timing, codec))
}
fn source_identity_matches(path: &std::path::Path, expected: &str, limit: u64) -> Result<bool> {
    Ok(crate::media::file_sha256_limited(path, limit)? == expected)
}
#[cfg(test)]
mod hash_boundary_tests {
    use super::*;
    #[test]
    fn finishing_recheck_enforces_source_and_sidechain_hash_cap() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["source", "sidechain"] {
            let path = temp.path().join(name);
            std::fs::write(&path, b"abcdefgh").unwrap();
            let hash = crate::media::file_sha256(&path).unwrap();
            assert!(source_identity_matches(&path, &hash, 16).unwrap());
            // An original admission saw8bytes; source grew before final recheck.
            std::fs::write(&path, vec![42; 2 * 1024 * 1024]).unwrap();
            crate::media::hash_tests::READ_BYTES.with(|n| n.set(0));
            assert!(
                source_identity_matches(&path, &hash, 16)
                    .unwrap_err()
                    .to_string()
                    .contains("admitted byte limit")
            );
            assert_eq!(crate::media::hash_tests::READ_BYTES.with(|n| n.get()), 0);
        }
    }
}
