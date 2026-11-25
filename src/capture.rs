//! Capture devices and room-tone recording through the system FFmpeg. Nothing
//! recorded here is uploaded; `record-bed` records only with explicit consent.
use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

const RATE: u32 = 48_000;
/// Lead recorded before the kept audio and discarded. Measured, not assumed:
/// the built-in microphone of the Linux test machine (PipeWire,
/// `alsa_input.pci-0000_00_1f.3.analog-stereo`) starts with a DC transient of
/// -0.84 FS at 0.5 s that decays below 0.001 FS only by about 3.5 s.
pub const LEAD_SECONDS: f64 = 4.0;
/// DC offset in the kept audio above this means the device was still settling.
const DC_SETTLED: f64 = 0.01;
/// Room tone is quiet: louder than this is probably not silence.
const LOUD_DBFS: f64 = -40.0;
/// Share of power below 150 Hz that counts as strong hum.
const HUM_SHARE: f64 = 0.5;
/// A 50-ms window this far above the median window level is a transient.
const TRANSIENT_DB: f64 = 15.0;
/// A window this far above the median counts toward speech-likeness.
const ACTIVE_DB: f64 = 10.0;
const WINDOW_SAMPLES: u32 = 2_400;

#[derive(Debug, Args)]
pub struct RecordBedArgs {
    /// Seconds of room tone to keep, 5 through 120.
    #[arg(long, default_value_t = 30)]
    pub seconds: u32,
    /// Capture device ID from `skywrite audio inputs`; defaults to the system default input.
    #[arg(long)]
    pub device: Option<String>,
    #[arg(short, long)]
    pub output: PathBuf,
    #[arg(long)]
    pub overwrite: bool,
    /// Required: recording captures the user's real room through their microphone.
    #[arg(long)]
    pub confirm: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub default: bool,
    pub backend: &'static str,
    /// PulseAudio/PipeWire monitor of an output, not a microphone.
    pub monitor: bool,
}
impl Device {
    fn json(&self) -> Value {
        json!({"id":self.id,"name":self.name,"default":self.default,"backend":self.backend,"monitor":self.monitor})
    }
}

/// `ffmpeg -sources pulse|alsa`: `[*] ID [NAME] (none)`, `*` marking the default.
pub fn parse_sources(text: &str, backend: &'static str) -> Vec<Device> {
    let mut devices = Vec::new();
    for line in text.lines() {
        if line.starts_with("Auto-detected sources") || line.trim().is_empty() {
            continue;
        }
        let default = line.starts_with('*');
        let rest = line.trim_start_matches('*').trim();
        let Some((id, tail)) = rest.split_once(" [") else {
            continue;
        };
        let name = tail.rsplit_once(']').map_or(tail, |(name, _)| name);
        let id = id.trim();
        if id.is_empty() {
            continue;
        }
        devices.push(Device {
            id: id.to_owned(),
            name: name.to_owned(),
            default,
            backend,
            monitor: backend == "pulse" && id.ends_with(".monitor"),
        });
    }
    devices
}

/// `ffmpeg -f avfoundation -list_devices true -i ""`: audio devices follow
/// "AVFoundation audio devices:" as `[N] NAME  [uid:UID]`. FFmpeg marks no
/// default; the ID is the index, which `-i :N` selects.
pub fn parse_avfoundation(text: &str) -> Vec<Device> {
    let mut devices = Vec::new();
    let mut audio = false;
    for line in text.lines() {
        // Strip the "[AVFoundation indev @ 0x...] " log prefix.
        let body = match line.find("] ") {
            Some(at) if line.starts_with('[') && line[..at].contains('@') => &line[at + 2..],
            _ => line,
        };
        if body.contains("AVFoundation video devices:") {
            audio = false;
            continue;
        }
        if body.contains("AVFoundation audio devices:") {
            audio = true;
            continue;
        }
        if !audio {
            continue;
        }
        let Some(rest) = body.strip_prefix('[') else {
            continue;
        };
        let Some((index, name)) = rest.split_once("] ") else {
            continue;
        };
        if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let name = name.split("  [uid:").next().unwrap_or(name).trim();
        devices.push(Device {
            id: index.to_owned(),
            name: name.to_owned(),
            default: false,
            backend: "avfoundation",
            monitor: false,
        });
    }
    devices
}

/// macOS's current default input, as FFmpeg's avfoundation `-i :default`.
pub fn avfoundation_default() -> Device {
    Device {
        id: "default".into(),
        name: "System default input (System Settings > Sound > Input)".into(),
        default: true,
        backend: "avfoundation",
        monitor: false,
    }
}

/// `ffmpeg -list_devices true -f dshow -i dummy`: `"NAME" (audio)` lines; the
/// following `Alternative name "..."` line is the stable ID.
pub fn parse_dshow(text: &str) -> Vec<Device> {
    let mut devices: Vec<Device> = Vec::new();
    for line in text.lines() {
        let body = match line.find("] ") {
            Some(at) if line.starts_with('[') => &line[at + 2..],
            _ => line,
        };
        if let Some(rest) = body.trim_start().strip_prefix("Alternative name \"") {
            if let (Some(last), Some(id)) = (devices.last_mut(), rest.strip_suffix('"'))
                && last.id == last.name
            {
                last.id = id.to_owned();
            }
            continue;
        }
        let Some(rest) = body.strip_prefix('"') else {
            continue;
        };
        let Some((name, kinds)) = rest.split_once("\" (") else {
            continue;
        };
        if !kinds.split([',', ')']).any(|k| k.trim() == "audio") {
            continue;
        }
        devices.push(Device {
            id: name.to_owned(),
            name: name.to_owned(),
            default: false,
            backend: "dshow",
            monitor: false,
        });
    }
    devices
}

/// Listing output of FFmpeg, complete up to 256 KiB: listing modes exit
/// nonzero by design, so the exit status is not an error here.
fn ffmpeg_text(args: &[&str]) -> Result<String> {
    let out = crate::process::run_any_status(
        "ffmpeg",
        &crate::process::args(args),
        Duration::from_secs(20),
        256 * 1024,
    )?;
    Ok(format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

/// Capture devices of this system's FFmpeg, per backend, plus notes about
/// backends that listed nothing.
pub fn list_devices() -> Result<(Vec<Device>, Vec<String>)> {
    let mut devices = Vec::new();
    let mut notes = Vec::new();
    match std::env::consts::OS {
        "macos" => {
            let found = parse_avfoundation(&ffmpeg_text(&[
                "-hide_banner",
                "-f",
                "avfoundation",
                "-list_devices",
                "true",
                "-i",
                "",
            ])?);
            if found.is_empty() {
                notes.push("avfoundation listed no audio devices".into());
            } else {
                // FFmpeg marks no default in its listing, but its avfoundation
                // input resolves `default` to the system default input.
                devices.push(avfoundation_default());
            }
            devices.extend(found);
        }
        "windows" => {
            let found = parse_dshow(&ffmpeg_text(&[
                "-hide_banner",
                "-list_devices",
                "true",
                "-f",
                "dshow",
                "-i",
                "dummy",
            ])?);
            if found.is_empty() {
                notes.push("dshow listed no audio devices".into());
            }
            devices.extend(found);
        }
        _ => {
            for backend in ["pulse", "alsa"] {
                let found = parse_sources(
                    &ffmpeg_text(&["-hide_banner", "-sources", backend])?,
                    backend,
                );
                if found.is_empty() {
                    notes.push(format!(
                        "{backend} listed no sources (not built into this FFmpeg, or no server)"
                    ));
                }
                devices.extend(found);
            }
        }
    }
    Ok((devices, notes))
}

pub fn inputs() -> Result<Value> {
    let (devices, notes) = list_devices()?;
    Ok(json!({
        "devices":devices.iter().map(Device::json).collect::<Vec<_>>(),
        "notes":notes,
        "recording":false,
        "hint":"Pass an id to `skywrite audio record-bed --device ID`; monitor sources capture playback, not the room. Ask the user before recording.",
    }))
}

/// What `record-bed` reads from: a real device, or (in tests) a fixed lavfi
/// source through the same code path.
#[derive(Debug, Clone)]
pub enum Source {
    Device(Device),
    #[cfg(test)]
    Lavfi(&'static str),
}
impl Source {
    fn input_args(&self) -> Vec<OsString> {
        match self {
            Self::Device(device) => {
                let input = match device.backend {
                    "avfoundation" => format!(":{}", device.id),
                    "dshow" => format!("audio={}", device.id),
                    _ => device.id.clone(),
                };
                let mut args = crate::process::args(&["-f", device.backend, "-i"]);
                args.push(input.into());
                args
            }
            #[cfg(test)]
            Self::Lavfi(graph) => crate::process::args(&["-f", "lavfi", "-i", graph]),
        }
    }
    fn label(&self) -> String {
        match self {
            Self::Device(d) => format!("{} ({}, {})", d.name, d.backend, d.id),
            #[cfg(test)]
            Self::Lavfi(graph) => format!("lavfi {graph}"),
        }
    }
    fn backend(&self) -> &str {
        match self {
            Self::Device(d) => d.backend,
            #[cfg(test)]
            Self::Lavfi(_) => "lavfi",
        }
    }
}

/// macOS gives no permission error of its own through FFmpeg: a denied or
/// never-granted microphone either fails to open the capture input or
/// delivers digital silence. Returns what the user must do.
pub fn macos_permission_problem(diagnostic: Option<&str>, silent: bool) -> Option<String> {
    const FIX: &str = "macOS has not given microphone access to the app that runs skywrite (Terminal, iTerm, your editor or agent app). Open System Settings > Privacy & Security > Microphone and turn it on for that app (if it is not listed, run `skywrite audio record-bed` once from that app in a desktop session and answer the prompt), then quit and reopen the app and rerun";
    if let Some(text) = diagnostic {
        let lower = text.to_ascii_lowercase();
        if lower.contains("failed to create av capture input device")
            || lower.contains("not authorized")
            || lower.contains("cannot use")
        {
            return Some(format!("{FIX}. FFmpeg said: {}", text.trim()));
        }
        return None;
    }
    silent.then(|| {
        format!(
            "the capture is digital silence, which on macOS usually means the microphone permission is denied or was never granted. {FIX}"
        )
    })
}

fn resolve(device: Option<&str>) -> Result<Device> {
    let (devices, notes) = list_devices()?;
    let chosen = match device {
        Some(id) => devices
            .iter()
            .find(|d| d.id == id)
            .cloned()
            .with_context(|| {
                format!(
                    "no capture device with id {id:?}; `skywrite audio inputs` lists them{}",
                    if notes.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", notes.join("; "))
                    }
                )
            })?,
        // Only a default the backend itself names; never a guess such as the
        // first device, which may be the wrong microphone.
        None => default_device(&devices)?,
    };
    Ok(chosen)
}

fn default_device(devices: &[Device]) -> Result<Device> {
    if devices.is_empty() {
        bail!("no capture device found; `skywrite audio inputs` lists what FFmpeg sees");
    }
    devices
        .iter()
        .find(|d| d.default && !d.monitor)
        .cloned()
        .context(
            "FFmpeg names no default input here; pass --device ID from `skywrite audio inputs`",
        )
}

pub fn record_bed(args: RecordBedArgs) -> Result<Value> {
    if !args.confirm {
        bail!(
            "record-bed records the user's real room through their microphone. Ask the user first; when they agree, rerun with --confirm"
        );
    }
    check_request(args.seconds, &args.output, args.overwrite)?;
    let source = Source::Device(resolve(args.device.as_deref())?);
    record_from(&source, args.seconds, &args.output, args.overwrite)
}

fn check_request(seconds: u32, output: &Path, overwrite: bool) -> Result<()> {
    if !(5..=120).contains(&seconds) {
        bail!("--seconds must be 5 through 120");
    }
    if !output
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("wav"))
    {
        bail!("record-bed writes a .wav file");
    }
    if std::fs::symlink_metadata(output).is_ok() && !overwrite {
        bail!("output exists; use --overwrite to replace it");
    }
    Ok(())
}

pub(crate) fn record_from(
    source: &Source,
    seconds: u32,
    output: &Path,
    overwrite: bool,
) -> Result<Value> {
    check_request(seconds, output, overwrite)?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let workspace = tempfile::Builder::new()
        .prefix(".skywrite-record-")
        .tempdir_in(parent)?;
    let _cleanup = crate::process::CleanupGuard::new(workspace.path())?;
    let raw = workspace.path().join("bed.wav");
    let lead = (LEAD_SECONDS * RATE as f64) as u64;
    let keep = u64::from(seconds) * u64::from(RATE);
    eprintln!(
        "skywrite: recording {seconds} s of room tone from {} (plus a discarded {LEAD_SECONDS} s start-up lead); keep the room silent",
        source.label()
    );
    let mut ffmpeg =
        crate::process::args(&["-hide_banner", "-nostdin", "-loglevel", "error", "-y"]);
    ffmpeg.extend(source.input_args());
    ffmpeg.extend(crate::process::args(&[
        "-t",
        &format!("{}", LEAD_SECONDS + f64::from(seconds) + 0.5),
        "-af",
        &format!(
            "aresample={RATE},pan=mono|c0=c0,atrim=start_sample={lead}:end_sample={},asetpts=N/SR/TB",
            lead + keep
        ),
        "-ar",
        "48000",
        "-ac",
        "1",
        "-c:a",
        "pcm_s16le",
        "-f",
        "wav",
    ]));
    ffmpeg.push(raw.as_os_str().to_owned());
    let budget = Duration::from_secs_f64(LEAD_SECONDS + f64::from(seconds) + 30.0);
    if let Err(error) = crate::process::run("ffmpeg", &ffmpeg, budget, 1024 * 1024) {
        let text = format!("{error:#}");
        if source.backend() == "avfoundation"
            && let Some(problem) = macos_permission_problem(Some(&text), false)
        {
            bail!(problem);
        }
        if text.contains("timed out") && source.backend() == "avfoundation" {
            bail!(
                "the capture produced nothing within {} s; macOS may be waiting for a microphone permission prompt. Answer it (System Settings > Privacy & Security > Microphone) and rerun",
                budget.as_secs()
            );
        }
        return Err(error.context("recording room tone"));
    }
    let timing = crate::timing::measure_audio(&crate::media::probe(&raw)?)?;
    if timing.samples != keep {
        bail!(
            "the device delivered {} of the {keep} samples requested; nothing was written",
            timing.samples
        );
    }
    let report = measure(&raw)?;
    let silent = report["rms_dbfs"].is_null();
    if silent && source.backend() == "avfoundation" {
        bail!(macos_permission_problem(None, true).unwrap());
    }
    crate::io::publish_copy(&raw, output, overwrite, parent)?;
    let mut out = json!({
        "version":1,"operation":"record-bed","output":output,"device":source.label(),"backend":source.backend(),
        "duration_seconds":seconds,"samples":timing.samples,"sample_rate":RATE,"channels":1,"format":"pcm_s16le",
        "discarded_lead_seconds":LEAD_SECONDS,"uploaded":false,
        "next":"skywrite audio roomtone SPEECH --bed THIS_FILE -o OUT (add --bed-highpass 120 if hum is reported)",
    });
    if let (Some(object), Value::Object(measured)) = (out.as_object_mut(), report) {
        object.extend(measured);
    }
    Ok(out)
}

/// Per-window RMS levels (dBFS) of 50-ms windows in the speech band
/// (200 Hz-4 kHz, so hum modulation does not count); silence is `None`.
fn window_levels(path: &Path) -> Result<Vec<Option<f64>>> {
    let mut args = crate::process::args(&["-hide_banner", "-nostdin", "-loglevel", "error", "-i"]);
    args.push(path.as_os_str().to_owned());
    args.extend(crate::process::args(&[
        "-af",
        &format!(
            "aformat=sample_fmts=dbl,highpass=f=200:poles=2,lowpass=f=4000:poles=2,asetnsamples=n={WINDOW_SAMPLES}:p=0,astats=metadata=1:reset=1:measure_perchannel=none:measure_overall=RMS_level,ametadata=print:key=lavfi.astats.Overall.RMS_level:file=-"
        ),
        "-f",
        "null",
        "-",
    ]));
    let out = crate::process::run("ffmpeg", &args, Duration::from_secs(120), 4 * 1024 * 1024)?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("lavfi.astats.Overall.RMS_level="))
        .map(|v| v.trim().parse::<f64>().ok().filter(|v| v.is_finite()))
        .collect())
}

fn overall(path: &Path, filter: &str) -> Result<(Option<f64>, Option<f64>, Option<f64>)> {
    let mut args = crate::process::args(&["-hide_banner", "-nostdin", "-i"]);
    args.push(path.as_os_str().to_owned());
    args.extend(crate::process::args(&[
        "-af",
        &format!(
            "aformat=sample_fmts=dbl,{filter}astats=metadata=0:measure_perchannel=none:measure_overall=RMS_level+Peak_level+DC_offset"
        ),
        "-f",
        "null",
        "-",
    ]));
    let out = crate::process::run("ffmpeg", &args, Duration::from_secs(120), 1024 * 1024)?;
    let text = String::from_utf8_lossy(&out.stderr);
    let value = |name: &str| {
        text.lines()
            .filter_map(|l| l.split_once(name).map(|(_, v)| v.trim()))
            .filter_map(|v| v.parse::<f64>().ok())
            .next_back()
            .filter(|v| v.is_finite())
    };
    Ok((
        value("RMS level dB:"),
        value("Peak level dB:"),
        value("DC offset:"),
    ))
}

/// Room-tone measurements and warnings for a recording.
pub fn measure(path: &Path) -> Result<Value> {
    let (rms, peak, dc) = overall(path, "")?;
    let (low, _, _) = overall(path, "lowpass=f=150:poles=2,lowpass=f=150:poles=2,")?;
    let windows = window_levels(path)?;
    analyse(rms, peak, dc, low, &windows)
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    Some(values[values.len() / 2])
}

/// Pure analysis of the measured levels, so thresholds are testable.
pub fn analyse(
    rms: Option<f64>,
    peak: Option<f64>,
    dc: Option<f64>,
    low_rms: Option<f64>,
    windows: &[Option<f64>],
) -> Result<Value> {
    let mut warnings = Vec::new();
    let Some(rms) = rms else {
        warnings.push(
            "the recording is digital silence: the device delivered zeros (muted, wrong device, or no permission)"
                .to_owned(),
        );
        return Ok(json!({"rms_dbfs":null,"peak_dbfs":peak,"warnings":warnings}));
    };
    let low_share = low_rms.map_or(0.0, |low| 10f64.powf((low - rms) / 10.0).min(1.0));
    let crest = peak.map(|p| p - rms);
    let mut levels: Vec<f64> = windows.iter().flatten().copied().collect();
    let floor = median(&mut levels).unwrap_or(rms);
    let mut transients = 0;
    let mut bursts = 0;
    let mut active = 0;
    let (mut in_transient, mut run) = (false, 0usize);
    for level in windows.iter().map(|w| w.unwrap_or(f64::NEG_INFINITY)) {
        let loud = level > floor + TRANSIENT_DB;
        if loud && !in_transient {
            transients += 1;
        }
        in_transient = loud;
        if level > floor + ACTIVE_DB {
            active += 1;
            run += 1;
        } else {
            // Syllable-to-phrase length active runs: 100-800 ms.
            if (2..=16).contains(&run) {
                bursts += 1;
            }
            run = 0;
        }
    }
    if (2..=16).contains(&run) {
        bursts += 1;
    }
    let active_share = active as f64 / windows.len().max(1) as f64;
    let speech = if active_share > 0.1 && bursts >= 3 {
        "likely"
    } else if active_share > 0.02 && bursts >= 1 {
        "possible"
    } else {
        "unlikely"
    };
    if rms > LOUD_DBFS {
        warnings.push(format!(
            "RMS {rms:.1} dBFS is loud for room tone (quiet rooms usually measure -60 to -45 dBFS): check for fans, machines, traffic or a gain set too high"
        ));
    }
    if low_share > HUM_SHARE {
        warnings.push(format!(
            "{:.0}% of the power is below 150 Hz (hum, machines or wind): use `audio roomtone --bed-highpass 120`",
            low_share * 100.0
        ));
    }
    if speech != "unlikely" || transients >= 3 {
        warnings.push(format!(
            "{transients} transients and speech-likeness {speech}: re-record with nobody talking or moving and no notifications"
        ));
    }
    if dc.is_some_and(|d| d.abs() > DC_SETTLED) {
        warnings.push(format!(
            "DC offset {:.3} FS: the device was still settling; record again or use a longer recording",
            dc.unwrap()
        ));
    }
    Ok(json!({
        "rms_dbfs":rms,"peak_dbfs":peak,"crest_factor_db":crest,"dc_offset":dc,
        "low_frequency_share_below_150_hz":low_share,"window_seconds":f64::from(WINDOW_SAMPLES) / f64::from(RATE),
        "median_window_rms_dbfs":floor,"transient_count":transients,"active_window_share":active_share,
        "speech_bursts":bursts,"speech_likeness":speech,"warnings":warnings,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PULSE: &str = "Auto-detected sources for pulse:\n  alsa_output.pci-0000_00_1f.3.analog-stereo.monitor [Monitor of Built-in Audio Analog Stereo] (none)\n* alsa_input.pci-0000_00_1f.3.analog-stereo [Built-in Audio Analog Stereo] (none)\n";
    const ALSA: &str = "Auto-detected sources for alsa:\n  null [Discard all samples (playback) or generate zero samples (capture)] (none)\n  pipewire [PipeWire Sound Server] (none)\n* default [Default ALSA Output (currently PipeWire Media Server)] (none)\n  hw:CARD=PCH,DEV=0 [Direct hardware device without any conversions] (none)\n";
    // Format from FFmpeg libavdevice/avfoundation.m (avf_log_device_entry).
    const AVFOUNDATION: &str = "[AVFoundation indev @ 0x7f9d4c004e00] AVFoundation video devices:\n[AVFoundation indev @ 0x7f9d4c004e00] [0] FaceTime HD Camera  [uid:0x1420000005ac8600]\n[AVFoundation indev @ 0x7f9d4c004e00] [1] Capture screen 0\n[AVFoundation indev @ 0x7f9d4c004e00] AVFoundation audio devices:\n[AVFoundation indev @ 0x7f9d4c004e00] [0] MacBook Pro Microphone  [uid:BuiltInMicrophoneDevice]\n[AVFoundation indev @ 0x7f9d4c004e00] [1] USB Audio [Line In]  [uid:AppleUSBAudioEngine:1] [serial:42]\n: Input/output error\n";
    // Format from FFmpeg libavdevice/dshow.c (dshow_cycle_devices).
    const DSHOW: &str = "[dshow @ 000001c] \"Integrated Camera\" (video)\n[dshow @ 000001c]   Alternative name \"@device_pnp_\\\\?\\usb#vid_04f2\"\n[dshow @ 000001c] \"Microphone Array (Realtek(R) Audio)\" (audio)\n[dshow @ 000001c]   Alternative name \"@device_cm_{33D9A762}\\wave_{6E1E2D1C}\"\ndummy: Immediate exit requested\n";

    #[test]
    fn device_lists_parse_per_backend() {
        let pulse = parse_sources(PULSE, "pulse");
        assert_eq!(pulse.len(), 2);
        assert!(pulse[0].monitor && !pulse[0].default);
        assert_eq!(pulse[1].id, "alsa_input.pci-0000_00_1f.3.analog-stereo");
        assert_eq!(pulse[1].name, "Built-in Audio Analog Stereo");
        assert!(pulse[1].default && !pulse[1].monitor);
        let alsa = parse_sources(ALSA, "alsa");
        assert_eq!(alsa.len(), 4);
        assert_eq!(alsa[3].id, "hw:CARD=PCH,DEV=0");
        assert_eq!(
            alsa[0].name,
            "Discard all samples (playback) or generate zero samples (capture)"
        );
        assert!(alsa[2].default && alsa[2].id == "default");
        let mac = parse_avfoundation(AVFOUNDATION);
        assert_eq!(mac.len(), 2, "{mac:?}");
        assert_eq!(
            (mac[0].id.as_str(), mac[0].name.as_str()),
            ("0", "MacBook Pro Microphone")
        );
        assert_eq!(mac[1].name, "USB Audio [Line In]");
        assert!(
            mac.iter()
                .all(|d| d.backend == "avfoundation" && !d.default)
        );
        let windows = parse_dshow(DSHOW);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].name, "Microphone Array (Realtek(R) Audio)");
        assert_eq!(windows[0].id, "@device_cm_{33D9A762}\\wave_{6E1E2D1C}");
        assert!(parse_sources("", "pulse").is_empty());
        assert!(parse_avfoundation("ffmpeg: avfoundation not found").is_empty());
    }

    #[test]
    fn default_input_is_only_what_the_backend_names() {
        let pulse = parse_sources(PULSE, "pulse");
        assert_eq!(
            default_device(&pulse).unwrap().id,
            "alsa_input.pci-0000_00_1f.3.analog-stereo"
        );
        // A monitor marked default is never chosen.
        let monitor_only = parse_sources(
            "* out.monitor [Monitor] (none)\n  mic [Mic] (none)\n",
            "pulse",
        );
        assert!(
            default_device(&monitor_only)
                .unwrap_err()
                .to_string()
                .contains("--device")
        );
        // avfoundation lists no default; `default` is FFmpeg's system-default input.
        let mut mac = vec![avfoundation_default()];
        mac.extend(parse_avfoundation(AVFOUNDATION));
        let chosen = default_device(&mac).unwrap();
        assert_eq!(chosen.id, "default");
        assert_eq!(
            Source::Device(chosen).input_args(),
            crate::process::args(&["-f", "avfoundation", "-i", ":default"])
        );
        assert!(
            default_device(&parse_dshow(DSHOW))
                .unwrap_err()
                .to_string()
                .contains("--device")
        );
        assert!(default_device(&[]).is_err());
    }

    #[test]
    fn long_listings_are_parsed_completely() {
        // A long video section must not hide the audio devices that follow.
        let mut text = String::from("[AVFoundation indev @ 0x1] AVFoundation video devices:\n");
        for i in 0..200 {
            text.push_str(&format!(
                "[AVFoundation indev @ 0x1] [{i}] Virtual Camera {i}  [uid:cam-{i}]\n"
            ));
        }
        text.push_str("[AVFoundation indev @ 0x1] AVFoundation audio devices:\n[AVFoundation indev @ 0x1] [0] Studio Mic  [uid:mic]\n");
        assert!(text.len() > 8192);
        assert_eq!(parse_avfoundation(&text)[0].name, "Studio Mic");
        // The process wrapper keeps a nonzero exit's whole output.
        let out = crate::process::run_any_status(
            "sh",
            &crate::process::args(&["-c", "head -c 10000 /dev/zero | tr '\\0' x >&2; exit 1"]),
            Duration::from_secs(10),
            64 * 1024,
        )
        .unwrap();
        assert!(!out.status.success());
        assert_eq!(out.stderr.len(), 10000);
    }

    #[test]
    fn macos_permission_states_say_what_to_allow() {
        let denied = macos_permission_problem(
            Some("[AVFoundation indev @ 0x1] Failed to create AV capture input device: Cannot use MacBook Pro Microphone"),
            false,
        )
        .unwrap();
        assert!(denied.contains("Privacy & Security > Microphone"));
        assert!(denied.contains("Cannot use MacBook Pro Microphone"));
        // Never granted / denied without an error: zeros.
        let silent = macos_permission_problem(None, true).unwrap();
        assert!(silent.contains("never granted") && silent.contains("System Settings"));
        assert!(macos_permission_problem(Some("Audio device not found"), false).is_none());
        assert!(macos_permission_problem(None, false).is_none());
    }

    #[test]
    fn device_inputs_never_reach_a_filter_graph() {
        let device = Device {
            id: "x,volume=0;[a]".into(),
            name: "odd".into(),
            default: false,
            backend: "pulse",
            monitor: false,
        };
        let args = Source::Device(device).input_args();
        assert_eq!(
            args,
            crate::process::args(&["-f", "pulse", "-i", "x,volume=0;[a]"])
        );
    }

    fn quiet(level: f64, n: usize) -> Vec<Option<f64>> {
        vec![Some(level); n]
    }

    #[test]
    fn thresholds_flag_loud_hum_transients_speech_and_settling() {
        let clean = analyse(
            Some(-55.0),
            Some(-42.0),
            Some(0.0),
            Some(-62.0),
            &quiet(-55.0, 600),
        )
        .unwrap();
        assert_eq!(clean["warnings"].as_array().unwrap().len(), 0, "{clean}");
        assert_eq!(clean["speech_likeness"], "unlikely");
        assert!((clean["crest_factor_db"].as_f64().unwrap() - 13.0).abs() < 1e-9);
        let loud = analyse(
            Some(-32.0),
            Some(-17.0),
            Some(0.0),
            Some(-33.0),
            &quiet(-32.0, 600),
        )
        .unwrap();
        let text = loud["warnings"].to_string();
        assert!(
            text.contains("loud for room tone") && text.contains("--bed-highpass 120"),
            "{text}"
        );
        assert!(loud["low_frequency_share_below_150_hz"].as_f64().unwrap() > 0.7);
        // Three isolated 50-ms clicks 25 dB above the floor.
        let mut clicks = quiet(-55.0, 600);
        for at in [100, 300, 500] {
            clicks[at] = Some(-30.0);
        }
        let clicked = analyse(Some(-54.0), Some(-28.0), Some(0.0), Some(-62.0), &clicks).unwrap();
        assert_eq!(clicked["transient_count"], 3);
        assert!(clicked["warnings"].to_string().contains("re-record"));
        // Speech: 300-ms bursts 20 dB up, covering a quarter of the time.
        let mut talk = quiet(-55.0, 600);
        for start in (0..600).step_by(24) {
            for w in talk.iter_mut().skip(start).take(6) {
                *w = Some(-35.0);
            }
        }
        let spoken = analyse(Some(-41.0), Some(-20.0), Some(0.0), Some(-50.0), &talk).unwrap();
        assert_eq!(spoken["speech_likeness"], "likely");
        assert!(spoken["speech_bursts"].as_u64().unwrap() >= 20);
        let settling = analyse(
            Some(-50.0),
            Some(-30.0),
            Some(0.2),
            None,
            &quiet(-50.0, 100),
        )
        .unwrap();
        assert!(settling["warnings"].to_string().contains("still settling"));
        let zeros = analyse(None, None, None, None, &[None; 10]).unwrap();
        assert!(zeros["rms_dbfs"].is_null());
        assert!(zeros["warnings"].to_string().contains("digital silence"));
    }

    #[test]
    fn record_bed_runs_the_capture_path_on_a_synthetic_source() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("bed.wav");
        // A loud 1-s start-up burst (discarded with the lead), then quiet pink
        // noise: measured like a real device capture.
        let source = Source::Lavfi(
            "anoisesrc=color=pink:amplitude=0.003:sample_rate=44100:seed=5,volume=enable='lt(t,1)':volume=300",
        );
        let report = record_from(&source, 5, &output, false).unwrap();
        assert_eq!(report["samples"], 5 * 48_000);
        assert_eq!(report["channels"], 1);
        assert_eq!(report["uploaded"], false);
        let timing = crate::timing::measure_audio(&crate::media::probe(&output).unwrap()).unwrap();
        assert_eq!((timing.samples, timing.sample_rate), (240_000, 48_000));
        let info = crate::media::probe(&output).unwrap();
        assert_eq!(
            info.audio().unwrap().codec_name.as_deref(),
            Some("pcm_s16le")
        );
        let rms = report["rms_dbfs"].as_f64().unwrap();
        // The burst would have put the level far above this; it was dropped.
        assert!(rms < -50.0 && rms > -70.0, "{rms}");
        assert!(report["peak_dbfs"].as_f64().unwrap() < -30.0, "{report}");
        // Existing output and bounds are refused before recording.
        assert!(record_from(&source, 5, &output, false).is_err());
        assert!(record_from(&source, 4, &dir.path().join("b.wav"), false).is_err());
        assert!(record_from(&source, 121, &dir.path().join("b.wav"), false).is_err());
        assert!(record_from(&source, 5, &dir.path().join("b.mp3"), false).is_err());
        let error = record_bed(RecordBedArgs {
            seconds: 30,
            device: None,
            output: dir.path().join("c.wav"),
            overwrite: false,
            confirm: false,
        })
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("Ask the user first") && error.contains("--confirm"),
            "{error}"
        );
        assert!(!dir.path().join("c.wav").exists());
    }
}
