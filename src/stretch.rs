//! Audio speed changes: which FFmpeg filter plays a clip at `speed` and how its
//! timing is kept exact. A command resolves this once, as explicit state, before
//! it builds any filter graph.
//!
//! - Speed 1 is a passthrough.
//! - Speeds within [`RESAMPLE_SPEED_LIMIT`] of 1 (clock-drift conform) resample
//!   at an exact rational ratio, sample-accurately.
//! - Larger (creative) speeds keep pitch with a time-stretcher: FFmpeg's
//!   `rubberband` when the build has it, else `atempo` (see [`StretcherChoice`]).
//!
//! FFmpeg's `rubberband` filter does not remove the library's processing
//! latency, so its output lands off by an amount that depends on speed (10 ms
//! clicks, short window, FFmpeg 8.0.1, librubberband 4.0: 14.7 ms early at 0.5x,
//! 6.1 ms late at 2x). Skywrite pads the input
//! with [`RUBBERBAND_PAD`] samples at each end (before: the clip's own first
//! samples, time-reversed; after: silence) and drops the leading output samples
//! that a click calibration measured for that exact speed and graph on this
//! FFmpeg. `atempo` (WSOLA) cannot be corrected the same way: its
//! offset moves by up to 16 ms between events (3-36 ms early at 0.5-2x, up to
//! about 60 ms early at 0.125-0.25x).
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::time::Duration;

const RATE: u32 = 48_000;
/// Largest |speed - 1| applied by resampling instead of time-stretching.
/// Covers clock-drift conform (1000 ppm); pitch moves by at most 1.8 cents.
pub const RESAMPLE_SPEED_LIMIT: f64 = 0.001;
/// Nominal rate for conform resampling; both relabeled rates fit FFmpeg's i32.
const RESAMPLE_RATE: u64 = RATE as u64 * 10_000;
const _: () = assert!(RESAMPLE_RATE * 1001 / 1000 <= i32::MAX as u64);
/// Rubberband settings chosen by measurement on speech and clicks: the short
/// window has half the latency and the steadiest timing; `channels=together`
/// keeps stereo channels phase-coherent (lower spectral distortion in mono).
/// `pitchq` has no effect at pitch 1; transients stay at the default `crisp`.
pub const RUBBERBAND_OPTIONS: &str = "window=short:channels=together";
/// Samples (48 kHz) added before and after rubberband's input. It must exceed
/// the latency at every speed so the leading drop is never negative and the
/// stretcher flushes all real content: 16384 / 8 = 2048 output samples at the
/// fastest speed, against a measured offset below 700.
///
/// The pad before the content is the content's own first samples in reverse
/// (content shorter than the pad is repeated to fill it first), so the signal
/// is continuous where the content starts. A silent pad gave every stretched clip an
/// artificial onset there, and rubberband's phase vocoder answers an onset after
/// silence differently by library version: at 0.5x, pink noise came out 3.5 dB
/// quieter in its first 150 ms than with this pad on every FFmpeg checked
/// (5.1.2 to 9.1), and a steady 440 Hz tone 15 dB low for 150 ms (the
/// librubberband in static 5.1.2 to 7.1.1) or for the whole clip (static 8.0.1
/// and 8.1.2). The pad after the content is silence, which only flushes the
/// stretcher.
pub const RUBBERBAND_PAD: u64 = 16_384;
/// Calibration bursts: 50 ms Hann-windowed 2 kHz tone, one per second from 0.5 s
/// (long enough to survive an 8x stretch; 10 ms bursts lose most of their energy).
const BURST_SAMPLES: usize = 2_400;
const BURST_COUNT: usize = 5;
const BURST_FIRST: usize = 24_000;
const BURST_SPACING: usize = 48_000;
/// Largest disagreement between the calibration bursts (10 ms).
const MAX_CALIBRATION_SPREAD: u64 = 480;
/// Distinct creative speeds one command may calibrate (each is one short FFmpeg
/// run of about 0.1-1.5 s).
pub const MAX_CALIBRATED_SPEEDS: usize = 64;

/// Which time-stretcher plays creative speeds (beyond ±0.1%).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum StretcherChoice {
    /// rubberband if this FFmpeg has it, else atempo with a warning.
    #[default]
    Auto,
    /// rubberband, or fail if this FFmpeg lacks it.
    Rubberband,
    /// Always atempo (3-36 ms early at 0.5-2x, more below 0.5x; varies between events).
    Atempo,
}

/// The time-stretcher actually used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stretcher {
    Rubberband,
    Atempo,
}

/// Measured rubberband timing for one speed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Calibration {
    pub speed: f64,
    /// Leading output samples dropped after the padded rubberband filter.
    pub dropped_samples: u64,
    /// Disagreement between the calibration bursts.
    pub spread_samples: u64,
}

/// The resolved speed handling for one command run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SpeedFilters {
    requested: StretcherChoice,
    /// `None` when no clip needed a time-stretcher, so FFmpeg was not checked.
    rubberband_available: Option<bool>,
    /// `None` when every speed is 1 or within the resample limit.
    stretcher: Option<Stretcher>,
    calibrations: Vec<Calibration>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

/// True when `speed` needs a time-stretcher rather than passthrough or resampling.
pub fn is_creative(speed: f64) -> bool {
    // The tolerance keeps 0.999 and 1.001 on the same side: 0.999 - 1 is
    // 1.0000000000000009e-3 in f64.
    speed != 1.0 && (speed - 1.0).abs() > RESAMPLE_SPEED_LIMIT + 1e-12
}

/// Whether this FFmpeg build has the `rubberband` filter (`ffmpeg -filters`).
pub fn rubberband_available() -> Result<bool> {
    Ok(crate::media::has_filter(
        &crate::media::ffmpeg_filters()?,
        "rubberband",
    ))
}

/// Decide the stretcher from the request and what this FFmpeg offers.
pub fn choose(requested: StretcherChoice, available: bool) -> Result<(Stretcher, Option<String>)> {
    match (requested, available) {
        (StretcherChoice::Atempo, _) => Ok((Stretcher::Atempo, None)),
        (_, true) => Ok((Stretcher::Rubberband, None)),
        (StretcherChoice::Rubberband, false) => bail!(
            "--stretcher rubberband: this FFmpeg build has no rubberband filter (see ffmpeg -hide_banner -filters); install an FFmpeg built with librubberband, or choose --stretcher atempo"
        ),
        (StretcherChoice::Auto, false) => Ok((
            Stretcher::Atempo,
            Some("this FFmpeg has no rubberband filter, so creative-speed audio uses atempo and may land tens of ms early (3-36 ms at 0.5-2x, more below); install FFmpeg with librubberband, or pass --stretcher atempo to accept this".into()),
        )),
    }
}

impl SpeedFilters {
    /// Resolve the stretcher for these clip speeds: check FFmpeg once if any
    /// speed needs stretching, and calibrate rubberband at each distinct speed.
    pub fn resolve(
        requested: StretcherChoice,
        speeds: impl IntoIterator<Item = f64>,
    ) -> Result<Self> {
        let mut creative: Vec<f64> = speeds.into_iter().filter(|s| is_creative(*s)).collect();
        creative.sort_by(f64::total_cmp);
        creative.dedup();
        let mut resolved = Self {
            requested,
            rubberband_available: None,
            stretcher: None,
            calibrations: Vec::new(),
            warnings: Vec::new(),
        };
        if creative.is_empty() {
            return Ok(resolved);
        }
        ensure!(
            creative.len() <= MAX_CALIBRATED_SPEEDS,
            "{} distinct clip speeds need time-stretching; at most {MAX_CALIBRATED_SPEEDS} are supported per command (round speeds to fewer values)",
            creative.len()
        );
        let available = rubberband_available()?;
        let (stretcher, warning) = choose(requested, available)?;
        resolved.rubberband_available = Some(available);
        resolved.stretcher = Some(stretcher);
        if let Some(warning) = warning {
            eprintln!("warning: {warning}");
            resolved.warnings.push(warning);
        }
        if stretcher == Stretcher::Rubberband {
            for speed in creative {
                eprintln!("calibrating rubberband latency at speed {speed}");
                resolved.calibrations.push(calibrate(speed)?);
            }
        }
        Ok(resolved)
    }

    /// FFmpeg filters that play 48 kHz audio of `input_samples` samples at
    /// `speed` source seconds per output second, keeping 48 kHz. The rubberband
    /// graph outputs exactly `round(input_samples / speed)` samples; it branches
    /// internally, using link labels that start with `label` (unique per graph).
    pub fn filter(&self, speed: f64, input_samples: u64, label: &str) -> Result<String> {
        if speed == 1.0 {
            return Ok("anull".into());
        }
        if !is_creative(speed) {
            let relabeled = (speed * RESAMPLE_RATE as f64).round() as u64;
            return Ok(format!(
                "asetrate={relabeled},aresample={RESAMPLE_RATE},asetrate={RATE}"
            ));
        }
        match self.stretcher {
            Some(Stretcher::Atempo) => Ok(atempo(speed)),
            Some(Stretcher::Rubberband) => {
                let calibration = self
                    .calibrations
                    .iter()
                    .find(|c| c.speed == speed)
                    .with_context(|| format!("speed {speed} was not calibrated for rubberband"))?;
                let start = calibration.dropped_samples;
                let end = start + (input_samples as f64 / speed).round() as u64;
                Ok(format!(
                    "{},atrim=start_sample={start}:end_sample={end},asetpts=N/SR/TB",
                    rubberband_core(speed, label, input_samples)
                ))
            }
            None => bail!("speed {speed} needs a time-stretcher that was not resolved"),
        }
    }

    /// One-line description for exported scripts.
    pub fn summary(&self) -> String {
        match self.stretcher {
            None => {
                "none (every clip speed is 1 or within 0.1% of 1, so no time-stretching)".into()
            }
            Some(Stretcher::Atempo) => format!(
                "atempo (requested {}); creative-speed audio may land tens of ms early",
                serde_json::to_value(self.requested)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default()
            ),
            Some(Stretcher::Rubberband) => format!(
                "rubberband ({RUBBERBAND_OPTIONS}); latency compensation measured with this machine's FFmpeg: {}; this script needs an FFmpeg with the rubberband filter",
                self.calibrations
                    .iter()
                    .map(|c| format!("speed {} drops {} samples", c.speed, c.dropped_samples))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// What the command's own `warnings` should repeat (also in [`Self::report`]).
    pub fn warnings(&self) -> Vec<String> {
        self.warnings.clone()
    }

    pub fn report(&self) -> Value {
        let mut value = json!(self);
        if self.stretcher == Some(Stretcher::Rubberband) {
            value["rubberband_options"] = json!(RUBBERBAND_OPTIONS);
            value["rubberband_pad_samples"] = json!(RUBBERBAND_PAD);
        }
        value
    }
}

fn atempo(speed: f64) -> String {
    let mut remaining = speed;
    let mut filters = Vec::new();
    while remaining < 0.5 {
        filters.push("atempo=0.5".to_owned());
        remaining /= 0.5;
    }
    while remaining > 2.0 {
        filters.push("atempo=2".to_owned());
        remaining /= 2.0;
    }
    filters.push(format!("atempo={remaining:.12}"));
    filters.join(",")
}

/// Padded rubberband stretch; calibration measures exactly this graph. Above
/// 2x it chains 2x stages: one stage at 4-8x drops most of a short event and
/// moves it by up to 28 ms, a chain of 2x stages by under 5 ms. The leading pad
/// is the first [`RUBBERBAND_PAD`] of the `input_samples` reversed (see there);
/// shorter input is looped to that length first, so a 25-ms clip is not
/// stretched after a mostly silent pad.
fn rubberband_core(speed: f64, label: &str, input_samples: u64) -> String {
    let mut stages = Vec::new();
    let mut remaining = speed;
    while remaining > 2.0 {
        stages.push(format!("rubberband=tempo=2:{RUBBERBAND_OPTIONS}"));
        remaining /= 2.0;
    }
    stages.push(format!(
        "rubberband=tempo={remaining:.12}:{RUBBERBAND_OPTIONS}"
    ));
    // aloop=loop=k plays k + 1 copies (and loops what it has if the stream ends
    // early); apad keeps the pad length exact in that case.
    let fill = if input_samples >= RUBBERBAND_PAD {
        String::new()
    } else {
        let n = input_samples.max(1);
        format!("aloop=loop={}:size={n},", RUBBERBAND_PAD.div_ceil(n) - 1)
    };
    format!(
        "asplit=2[{label}_rbin][{label}_rbhead];[{label}_rbhead]{fill}atrim=end_sample={RUBBERBAND_PAD},apad=whole_len={RUBBERBAND_PAD},areverse[{label}_rbpre];[{label}_rbpre][{label}_rbin]concat=n=2:v=0:a=1,apad=pad_len={RUBBERBAND_PAD},{}",
        stages.join(",")
    )
}

fn burst() -> Vec<f64> {
    let n = BURST_SAMPLES as f64;
    (0..BURST_SAMPLES)
        .map(|i| {
            let i = i as f64;
            (2.0 * std::f64::consts::PI * 2000.0 * i / RATE as f64).sin()
                * 0.5
                * (1.0 - (2.0 * std::f64::consts::PI * i / (n - 1.0)).cos())
        })
        .collect()
}

/// Measure where rubberband puts bursts at `speed` and return the leading
/// output samples to drop so each burst's energy centroid lands on
/// `centroid / speed`.
fn calibrate(speed: f64) -> Result<Calibration> {
    let template = burst();
    let source_len = BURST_FIRST + BURST_SPACING * BURST_COUNT;
    let mut source = vec![0f32; source_len];
    for k in 0..BURST_COUNT {
        let at = BURST_FIRST + BURST_SPACING * k;
        for (i, v) in template.iter().enumerate() {
            source[at + i] = (0.5 * v) as f32;
        }
    }
    let dir = tempfile::Builder::new()
        .prefix("skywrite-stretch-")
        .tempdir()?;
    let input = dir.path().join("bursts.f32");
    std::fs::write(
        &input,
        source
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>(),
    )?;
    let mut args: Vec<OsString> = crate::process::args(&[
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-threads",
        "1",
        "-f",
        "f32le",
        "-ar",
        "48000",
        "-ac",
        "1",
        "-i",
    ]);
    args.push(input.into_os_string());
    let graph = format!(
        "aformat=sample_fmts=fltp:channel_layouts=stereo,{}",
        rubberband_core(speed, "calibration", source_len as u64)
    );
    args.extend(crate::process::args(&[
        "-filter_threads",
        "1",
        "-af",
        &graph,
        "-ac",
        "1",
        "-f",
        "f32le",
        "-",
    ]));
    let expected_bytes =
        ((source_len as u64 + 2 * RUBBERBAND_PAD) as f64 / speed * 4.0).ceil() as usize;
    let out = crate::process::run(
        "ffmpeg",
        &args,
        Duration::from_secs(120),
        expected_bytes + 1024 * 1024,
    )
    .with_context(|| format!("rubberband calibration at speed {speed} failed"))?;
    let output: Vec<f64> = out
        .stdout
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b) as f64)
        .collect();
    // Energy centroids are independent of how long the stretched burst is and of
    // its carrier phase; the output is silent between bursts.
    let source_energy: f64 = template.iter().map(|v| 0.25 * v * v).sum();
    let half = (BURST_SAMPLES - 1) as f64 / 2.0;
    let pad_out = RUBBERBAND_PAD as f64 / speed;
    let window = (0.06 + 0.06 / speed).min(0.45 / speed) * RATE as f64;
    let mut drops = Vec::with_capacity(BURST_COUNT);
    for k in 0..BURST_COUNT {
        let expected = ((BURST_FIRST + BURST_SPACING * k) as f64 + half) / speed;
        let guess = expected + pad_out;
        let from = (guess - window).max(0.0) as usize;
        let to = ((guess + window) as usize).min(output.len());
        let (mut energy, mut moment) = (0.0, 0.0);
        for (i, v) in output.iter().enumerate().take(to).skip(from) {
            energy += v * v;
            moment += i as f64 * v * v;
        }
        ensure!(
            energy >= 0.01 * source_energy / speed,
            "rubberband calibration at speed {speed} did not find burst {k}; use --stretcher atempo"
        );
        let drop = (moment / energy - expected).round();
        ensure!(
            drop >= 0.0,
            "rubberband latency at speed {speed} exceeds the {RUBBERBAND_PAD}-sample pad"
        );
        drops.push(drop as u64);
    }
    drops.sort_unstable();
    let spread = drops[drops.len() - 1] - drops[0];
    ensure!(
        spread <= MAX_CALIBRATION_SPREAD,
        "rubberband timing at speed {speed} varied by {spread} samples between calibration bursts; use --stretcher atempo"
    );
    Ok(Calibration {
        speed,
        dropped_samples: drops[drops.len() / 2],
        spread_samples: spread,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(stretcher: Option<Stretcher>, calibrations: Vec<Calibration>) -> SpeedFilters {
        SpeedFilters {
            requested: StretcherChoice::Auto,
            rubberband_available: stretcher.map(|s| s == Stretcher::Rubberband),
            stretcher,
            calibrations,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn unit_and_drift_speeds_ignore_the_stretcher() {
        for stretcher in [None, Some(Stretcher::Atempo), Some(Stretcher::Rubberband)] {
            let filters = fixed(stretcher, Vec::new());
            assert_eq!(filters.filter(1.0, 48_000, "a").unwrap(), "anull");
            assert_eq!(
                filters.filter(1.0005003119303504, 48_000, "a").unwrap(),
                "asetrate=480240150,aresample=480000000,asetrate=48000"
            );
            assert_eq!(
                filters.filter(0.999999, 48_000, "a").unwrap(),
                "asetrate=479999520,aresample=480000000,asetrate=48000"
            );
            assert_eq!(
                filters
                    .filter(1.0 + RESAMPLE_SPEED_LIMIT, 48_000, "a")
                    .unwrap(),
                "asetrate=480480000,aresample=480000000,asetrate=48000"
            );
        }
        assert!(!is_creative(1.0 + RESAMPLE_SPEED_LIMIT));
        assert!(!is_creative(1.0 - RESAMPLE_SPEED_LIMIT));
        assert!(!is_creative(0.999) && !is_creative(1.001));
        assert!(is_creative(1.0011) && is_creative(0.9989));
    }

    #[test]
    fn atempo_chains_across_the_full_range() {
        let filters = fixed(Some(Stretcher::Atempo), Vec::new());
        assert_eq!(
            filters.filter(1.01, 1, "a").unwrap(),
            "atempo=1.010000000000"
        );
        assert_eq!(
            filters.filter(8.0, 1, "a").unwrap(),
            "atempo=2,atempo=2,atempo=2.000000000000"
        );
        assert_eq!(
            filters.filter(0.125, 1, "a").unwrap(),
            "atempo=0.5,atempo=0.5,atempo=0.500000000000"
        );
    }

    #[test]
    fn rubberband_pads_and_drops_the_calibrated_latency_with_exact_length() {
        let filters = fixed(
            Some(Stretcher::Rubberband),
            vec![Calibration {
                speed: 2.0,
                dropped_samples: 8_500,
                spread_samples: 20,
            }],
        );
        assert_eq!(
            filters.filter(2.0, 48_001, "a3_1").unwrap(),
            "asplit=2[a3_1_rbin][a3_1_rbhead];[a3_1_rbhead]atrim=end_sample=16384,apad=whole_len=16384,areverse[a3_1_rbpre];[a3_1_rbpre][a3_1_rbin]concat=n=2:v=0:a=1,apad=pad_len=16384,rubberband=tempo=2.000000000000:window=short:channels=together,atrim=start_sample=8500:end_sample=32501,asetpts=N/SR/TB"
        );
        let chained = fixed(
            Some(Stretcher::Rubberband),
            vec![Calibration {
                speed: 6.0,
                dropped_samples: 3_000,
                spread_samples: 100,
            }],
        );
        assert_eq!(
            chained.filter(6.0, 6, "t").unwrap(),
            "asplit=2[t_rbin][t_rbhead];[t_rbhead]aloop=loop=2730:size=6,atrim=end_sample=16384,apad=whole_len=16384,areverse[t_rbpre];[t_rbpre][t_rbin]concat=n=2:v=0:a=1,apad=pad_len=16384,rubberband=tempo=2:window=short:channels=together,rubberband=tempo=2:window=short:channels=together,rubberband=tempo=1.500000000000:window=short:channels=together,atrim=start_sample=3000:end_sample=3001,asetpts=N/SR/TB"
        );
        assert!(filters.filter(1.5, 1, "a").is_err());
        assert!(fixed(None, Vec::new()).filter(1.5, 1, "a").is_err());
    }

    #[test]
    fn choice_follows_the_request_and_ffmpeg() {
        use StretcherChoice::*;
        assert_eq!(choose(Auto, true).unwrap(), (Stretcher::Rubberband, None));
        let (used, warning) = choose(Auto, false).unwrap();
        assert_eq!(used, Stretcher::Atempo);
        assert!(warning.unwrap().contains("3-36 ms at 0.5-2x"));
        assert_eq!(choose(Rubberband, true).unwrap().0, Stretcher::Rubberband);
        let error = choose(Rubberband, false).unwrap_err().to_string();
        assert!(error.contains("no rubberband filter"), "{error}");
        assert_eq!(choose(Atempo, true).unwrap(), (Stretcher::Atempo, None));
        assert_eq!(choose(Atempo, false).unwrap(), (Stretcher::Atempo, None));
    }

    #[test]
    fn no_creative_speed_checks_nothing() {
        let filters =
            SpeedFilters::resolve(StretcherChoice::Rubberband, [1.0, 1.0005, 0.9995, 0.999])
                .unwrap();
        assert_eq!(filters.rubberband_available, None);
        assert_eq!(filters.stretcher, None);
    }

    #[test]
    fn rubberband_calibration_is_consistent_when_available() {
        if !rubberband_available().unwrap() {
            eprintln!(
                "skipping rubberband_calibration_is_consistent_when_available: this FFmpeg has no rubberband filter"
            );
            return;
        }
        for speed in [0.125, 0.5, 0.8, 1.25, 2.0, 3.0, 8.0] {
            let calibration = calibrate(speed).unwrap();
            let pad = (RUBBERBAND_PAD as f64 / speed).round() as u64;
            // The drop is the pad plus the stretcher's own offset (about 15 ms * |1 - 1/speed| here).
            assert!(
                calibration.dropped_samples.abs_diff(pad) < 8_000,
                "{speed}: {calibration:?}"
            );
            assert!(calibration.spread_samples <= MAX_CALIBRATION_SPREAD);
        }
    }

    /// Stretch `input` (mono f32 at 48 kHz) through `graph`; mono f32 out.
    fn stretch_raw(dir: &std::path::Path, input: &[f32], graph: &str) -> Vec<f32> {
        let path = dir.join("in.f32");
        std::fs::write(
            &path,
            input
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let mut args = crate::process::args(&[
            "-hide_banner",
            "-nostdin",
            "-loglevel",
            "error",
            "-threads",
            "1",
            "-f",
            "f32le",
            "-ar",
            "48000",
            "-ac",
            "1",
            "-i",
        ]);
        args.push(path.into_os_string());
        let graph = format!("aformat=sample_fmts=fltp:channel_layouts=stereo,{graph}");
        args.extend(crate::process::args(&[
            "-filter_threads",
            "1",
            "-af",
            &graph,
            "-ac",
            "1",
            "-f",
            "f32le",
            "-",
        ]));
        let out = crate::process::run("ffmpeg", &args, Duration::from_secs(120), 64 << 20).unwrap();
        let samples = out.stdout.as_chunks::<4>().0.iter();
        samples.map(|b| f32::from_le_bytes(*b)).collect()
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt()
    }

    fn rubberband_at(speed: f64) -> SpeedFilters {
        SpeedFilters {
            requested: StretcherChoice::Rubberband,
            rubberband_available: Some(true),
            stretcher: Some(Stretcher::Rubberband),
            calibrations: vec![calibrate(speed).unwrap()],
            warnings: Vec::new(),
        }
    }

    /// A clip stretched on its own keeps the level its samples have inside a
    /// long stretch. Before the reversed pre-roll, the silent pad made a 25-ms
    /// 440 Hz clip 9 dB quiet at 0.5x and 6 dB at 0.125x (25-ms pink noise 3 dB
    /// at 0.5x); a reversed pad left silent where the clip is shorter than it
    /// still lost 10 dB on the 25-ms tone. Slowed-down clips are checked: at 2x
    /// and above, short pure tones vary by several dB with the phase at the cut
    /// whatever the pre-roll (docs/validation.md), so only noise is checked at 2x.
    #[test]
    fn short_clips_keep_their_level_through_rubberband() {
        if !rubberband_available().unwrap() {
            eprintln!(
                "skipping short_clips_keep_their_level_through_rubberband: this FFmpeg has no rubberband filter"
            );
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let noise_file = dir.path().join("noise.s16");
        let mut args = crate::process::args(&[
            "-hide_banner",
            "-nostdin",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "anoisesrc=color=pink:amplitude=0.25:sample_rate=48000:duration=2:seed=11",
            "-ac",
            "1",
            "-f",
            "s16le",
        ]);
        args.push(noise_file.clone().into_os_string());
        crate::process::run("ffmpeg", &args, Duration::from_secs(60), 1 << 20).unwrap();
        let noise: Vec<f32> = std::fs::read(&noise_file)
            .unwrap()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b) as f32 / 32768.0)
            .collect();
        let tone: Vec<f32> = (0..96_000)
            .map(|i| {
                let v = 0.25 * (std::f64::consts::TAU * 440.0 * i as f64 / 48_000.0).sin();
                ((v * 32768.0).round() / 32768.0) as f32
            })
            .collect();
        // (speed, signals, largest level difference in dB)
        for (speed, signals, limit) in [
            (0.125, vec![("noise", &noise), ("tone", &tone)], 2.0),
            (0.5, vec![("noise", &noise), ("tone", &tone)], 2.0),
            (2.0, vec![("noise", &noise)], 3.0),
        ] {
            let filters = rubberband_at(speed);
            for (name, source) in signals {
                let graph = filters.filter(speed, source.len() as u64, "t").unwrap();
                let long = stretch_raw(dir.path(), source, &graph);
                assert_eq!(
                    long.len() as u64,
                    (source.len() as f64 / speed).round() as u64
                );
                let at = 48_000;
                for n in [1_200usize, 4_800, 19_200] {
                    let out_range = |a: usize| (a as f64 / speed).round() as usize;
                    let reference = &long[out_range(at)..out_range(at + n)];
                    let graph = filters.filter(speed, n as u64, "t").unwrap();
                    let out = stretch_raw(dir.path(), &source[at..at + n], &graph);
                    assert_eq!(out.len(), reference.len(), "{name} {speed}x {n}");
                    let db = 20.0 * (rms(&out) / rms(reference)).log10();
                    let head = out.len().min(7_200);
                    let head_db = 20.0 * (rms(&out[..head]) / rms(&reference[..head])).log10();
                    assert!(
                        db.abs() <= limit && head_db.abs() <= limit,
                        "{name} at {speed}x, {n} samples: {db:+.2} dB overall, {head_db:+.2} dB in the first 150 ms"
                    );
                }
            }
        }
    }

    /// A burst inside a clip shorter than the pad lands where the calibration
    /// says, at every speed (the pre-roll is the clip itself, looped).
    #[test]
    fn short_clip_bursts_land_on_time_through_rubberband() {
        if !rubberband_available().unwrap() {
            eprintln!(
                "skipping short_clip_bursts_land_on_time_through_rubberband: this FFmpeg has no rubberband filter"
            );
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let template = burst();
        let mut clip = vec![0f32; 9_600];
        let at = 4_800;
        for (i, v) in template.iter().enumerate() {
            clip[at + i] = (0.5 * v) as f32;
        }
        let half = (BURST_SAMPLES - 1) as f64 / 2.0;
        for speed in [0.125, 0.5, 2.0, 8.0] {
            let filters = rubberband_at(speed);
            let graph = filters.filter(speed, clip.len() as u64, "t").unwrap();
            let out = stretch_raw(dir.path(), &clip, &graph);
            assert_eq!(out.len() as u64, (clip.len() as f64 / speed).round() as u64);
            let (energy, moment) = out.iter().enumerate().fold((0.0, 0.0), |(e, m), (i, v)| {
                let p = (*v as f64).powi(2);
                (e + p, m + i as f64 * p)
            });
            let error = moment / energy - (at as f64 + half) / speed;
            assert!(
                error.abs() <= 192.0,
                "{speed}x: burst {error:+.0} samples off"
            );
        }
    }
}
