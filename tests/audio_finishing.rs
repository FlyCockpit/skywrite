use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

fn ffmpeg(args: &[&str], output: &Path) {
    let mut args = skywrite::process::args(args);
    args.push(output.as_os_str().to_owned());
    skywrite::process::run("ffmpeg", &args, Duration::from_secs(30), 1024 * 1024).unwrap();
}
fn tone(path: &Path, duration: f64) {
    ffmpeg(
        &[
            "-v",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:sample_rate=48000:duration={duration}"),
            "-ac",
            "2",
            "-c:a",
            "pcm_s16le",
        ],
        path,
    );
}
fn cli(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .args(args)
        .env("SKYWRITE_CACHE_DIR", root.join("cache"))
        .env("SKYWRITE_HOME", root.join("home"))
        .env_remove("OPENROUTER_API_KEY")
        .output()
        .unwrap()
}
fn success(root: &Path, args: &[&str]) -> Value {
    let out = cli(root, args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}
fn timing(path: &Path) -> skywrite::media::AudioTiming {
    skywrite::timing::measure_audio(&skywrite::media::probe(path).unwrap()).unwrap()
}
// Bounded short PCM windows for waveform assertions, never entire long assets.
fn pcm(path: &Path, start: f64, duration: f64) -> Vec<f64> {
    let mut args = skywrite::process::args(&["-v", "error", "-nostdin", "-i"]);
    args.push(path.as_os_str().to_owned());
    args.extend(skywrite::process::args(&[
        "-af",
        &format!("atrim=start={start}:duration={duration},asetpts=PTS-STARTPTS"),
        "-t",
        &duration.to_string(),
        "-ar",
        "48000",
        "-ac",
        "1",
        "-f",
        "s16le",
        "-",
    ]));
    let out =
        skywrite::process::run("ffmpeg", &args, Duration::from_secs(10), 1024 * 1024).unwrap();
    out.stdout
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b) as f64 / 32768.0)
        .collect()
}
fn rms(values: &[f64]) -> f64 {
    (values.iter().map(|x| x * x).sum::<f64>() / values.len() as f64).sqrt()
}
fn lufs(path: &Path) -> f64 {
    let mut args = skywrite::process::args(&["-hide_banner", "-nostdin", "-i"]);
    args.push(path.as_os_str().to_owned());
    args.extend(skywrite::process::args(&[
        "-af",
        "loudnorm=I=-18:TP=-2:LRA=7:print_format=json",
        "-f",
        "null",
        "-",
    ]));
    let out =
        skywrite::process::run("ffmpeg", &args, Duration::from_secs(30), 1024 * 1024).unwrap();
    let s = String::from_utf8(out.stderr).unwrap();
    let v: Value = serde_json::from_str(&s[s.rfind('{').unwrap()..=s.rfind('}').unwrap()]).unwrap();
    v["input_i"].as_str().unwrap().parse().unwrap()
}
#[test]
fn real_loudness_peak_tempo_and_encoded_formats() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    let src = p.join("-témoignage.wav");
    tone(&src, 3.0);
    let hash = skywrite::media::file_sha256(&src).unwrap();
    let normalized = p.join("normalized.wav");
    let r = success(
        p,
        &[
            "audio",
            "normalize",
            src.to_str().unwrap(),
            "-o",
            normalized.to_str().unwrap(),
            "--lufs",
            "-18",
        ],
    );
    assert_eq!(r["normalization"]["method"], "two_pass_loudnorm");
    assert!((lufs(&normalized) + 18.0).abs() < 0.4);
    assert!(lufs(&normalized) > lufs(&src) + 3.0);
    assert!(timing(&normalized).peak_dbfs.unwrap() <= -1.8);
    let peak = p.join("peak.wav");
    success(
        p,
        &[
            "audio",
            "normalize",
            src.to_str().unwrap(),
            "-o",
            peak.to_str().unwrap(),
            "--peak-db",
            "-6",
        ],
    );
    assert!((timing(&peak).peak_dbfs.unwrap() + 6.0).abs() < 0.03);
    let fast = p.join("fast.mp3");
    let r = success(
        p,
        &[
            "audio",
            "tempo",
            src.to_str().unwrap(),
            "-o",
            fast.to_str().unwrap(),
            "--tempo",
            "1.5",
        ],
    );
    let actual = timing(&fast);
    assert!((actual.decoded_duration_seconds - 2.0).abs() < 0.05);
    assert_eq!(r["output_timing"]["samples"], actual.samples);
    assert_eq!(
        skywrite::media::probe(&fast)
            .unwrap()
            .audio()
            .unwrap()
            .codec_name
            .as_deref(),
        Some("mp3")
    );
    assert_eq!(skywrite::media::file_sha256(&src).unwrap(), hash);
}
#[test]
fn loops_crossfade_ducking_recovers_and_short_control_does_not_truncate() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    let src = p.join("music.wav");
    tone(&src, 1.0);
    let looped = p.join("looped.wav");
    let r = success(
        p,
        &[
            "audio",
            "loop",
            src.to_str().unwrap(),
            "-o",
            looped.to_str().unwrap(),
            "--duration",
            "4",
            "--crossfade",
            "0.2",
        ],
    );
    assert_eq!(r["loop_repetitions"], 5);
    assert_eq!(timing(&looped).samples, 192000);
    let boundary = pcm(&looped, 0.77, 0.28);
    assert!(rms(&boundary) > 0.04, "crossfade introduced an empty gap");
    let min_window = boundary.chunks(240).map(rms).fold(f64::INFINITY, f64::min);
    assert!(min_window > 0.03, "5ms silent window at loop boundary");
    let control = p.join("voice.wav");
    ffmpeg(
        &[
            "-v",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "aevalsrc=if(between(t\\,0.6\\,1.3)\\,0.8*sin(2*PI*220*t)\\,0):s=48000:d=1.8",
            "-ac",
            "2",
            "-c:a",
            "pcm_s16le",
        ],
        &control,
    );
    let ducked = p.join("ducked.wav");
    success(
        p,
        &[
            "audio",
            "duck",
            looped.to_str().unwrap(),
            "-o",
            ducked.to_str().unwrap(),
            "--sidechain",
            control.to_str().unwrap(),
            "--release-ms",
            "200",
        ],
    );
    assert_eq!(timing(&ducked).samples, 192000);
    let before = rms(&pcm(&ducked, 0.1, 0.2));
    let during = rms(&pcm(&ducked, 0.8, 0.2));
    let after = rms(&pcm(&ducked, 3.0, 0.2));
    assert!(
        during < before * 0.4,
        "voice failed to attenuate music: {before} {during}"
    );
    assert!(
        after > before * 0.9,
        "music failed to recover: {before} {after}"
    );
}
#[test]
fn ducking_gives_the_same_samples_on_every_run() {
    // FFmpeg 7.0+ splits the two inputs into frames by thread timing, and
    // sidechaincompress output depended on that split near the voice's end.
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    let music = p.join("music.wav");
    ffmpeg(
        &[
            "-v",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=f=220:d=8:r=48000,volume=0.3",
            "-c:a",
            "pcm_s16le",
        ],
        &music,
    );
    let voice = p.join("voice.wav");
    ffmpeg(
        &[
            "-v",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "anoisesrc=d=8:c=white:seed=3:r=48000,volume='0.6*gt(sin(t*3),0)':eval=frame",
            "-c:a",
            "pcm_s16le",
        ],
        &voice,
    );
    let mut first: Option<Vec<u8>> = None;
    for run in 0..6 {
        let ducked = p.join(format!("ducked-{run}.wav"));
        success(
            p,
            &[
                "audio",
                "duck",
                music.to_str().unwrap(),
                "-o",
                ducked.to_str().unwrap(),
                "--sidechain",
                voice.to_str().unwrap(),
            ],
        );
        let samples = std::fs::read(&ducked).unwrap();
        match &first {
            None => first = Some(samples),
            Some(expected) => assert!(*expected == samples, "run {run} ducked differently"),
        }
    }
}
#[test]
fn crossfaded_loop_of_a_short_source_matches_the_exact_sum_of_faded_copies() {
    // FFmpeg 7.x's acrossfade cut short loops of a 1 s source at random
    // points; every output sample must be the linear-crossfaded copies' sum.
    // 437 Hz is 0.6 cycle out of phase after the 0.8 s stride, so overlapping
    // copies differ and a loop without crossfades fails too.
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    let src = p.join("music.wav");
    ffmpeg(
        &[
            "-v",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=437:sample_rate=48000:duration=1",
            "-ac",
            "2",
            "-c:a",
            "pcm_s16le",
        ],
        &src,
    );
    let looped = p.join("looped.wav");
    let r = success(
        p,
        &[
            "audio",
            "loop",
            src.to_str().unwrap(),
            "-o",
            looped.to_str().unwrap(),
            "--duration",
            "6",
            "--crossfade",
            "0.2",
        ],
    );
    assert_eq!(r["loop_repetitions"], 8);
    assert_eq!(timing(&looped).samples, 288_000);
    let source = pcm(&src, 0.0, 1.0);
    assert_eq!(source.len(), 48_000);
    let (content, fade) = (48_000usize, 9_600usize);
    let stride = content - fade;
    let mut expected = vec![0.0; 288_000];
    for copy in 0..8 {
        for (k, value) in source.iter().enumerate() {
            let Some(slot) = expected.get_mut(copy * stride + k) else {
                break;
            };
            let mut gain = 1.0;
            if copy > 0 && k < fade {
                gain *= k as f64 / fade as f64;
            }
            if copy < 7 && k >= content - fade {
                gain *= (content - k) as f64 / fade as f64;
            }
            *slot += value * gain;
        }
    }
    let actual = pcm(&looped, 0.0, 6.0);
    assert_eq!(actual.len(), expected.len());
    let (worst, at) = actual
        .iter()
        .zip(&expected)
        .map(|(a, e)| (a - e).abs())
        .enumerate()
        .fold(
            (0.0, 0),
            |best, (i, diff)| {
                if diff > best.0 { (diff, i) } else { best }
            },
        );
    assert!(
        worst <= 3.0 / 32768.0,
        "loop differs from the crossfaded copies by {:.1} LSB at sample {at}",
        worst * 32768.0
    );
}
#[test]
fn final_pad_trim_fades_and_selection_use_decoded_sample_grid() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    let src = p.join("short.wav");
    tone(&src, 1.2);
    // Padding 1.2 s of source to 10 s exercises the same pad-then-trim path
    // as a long request (no code depends on the length) in a fraction of the
    // decode time.
    let long = p.join("padded.wav");
    success(
        p,
        &[
            "audio",
            "finish",
            src.to_str().unwrap(),
            "-o",
            long.to_str().unwrap(),
            "--duration",
            "10",
            "--fade-in",
            "0.1",
            "--fade-out",
            "0.1",
        ],
    );
    assert_eq!(timing(&long).samples, 10 * 48000);
    assert!(rms(&pcm(&long, 0.0, 0.01)) < rms(&pcm(&long, 0.2, 0.01)) * 0.2);
    assert_eq!(rms(&pcm(&long, 9.8, 0.1)), 0.0);
    let short = p.join("shortened.mp3");
    let r = success(
        p,
        &[
            "audio",
            "finish",
            src.to_str().unwrap(),
            "-o",
            short.to_str().unwrap(),
            "--start",
            "0.1",
            "--length",
            "0.5",
            "--duration",
            "0.30001",
            "--fade-out",
            "0.1",
        ],
    );
    assert_eq!(r["source_selection"]["start_samples_48000"], 4800);
    assert_eq!(timing(&short).samples, 14400);
    assert!(rms(&pcm(&short, 0.29, 0.01)) < rms(&pcm(&short, 0.05, 0.01)) * 0.2);
}
#[test]
fn invalid_options_inputs_and_overwrites_preserve_files() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    let src = p.join("src.wav");
    tone(&src, 0.5);
    let source = std::fs::read(&src).unwrap();
    let out = p.join("existing.wav");
    std::fs::write(&out, b"old output").unwrap();
    for tail in [
        vec!["--tempo", "NaN"],
        vec!["--tempo", "0"],
        vec!["--duration", "inf"],
        vec!["--start", "2"],
        vec!["--length", "2"],
        vec!["--loop", "--duration", "10", "--crossfade", "0.4"],
        vec!["--fade-out", "2"],
        vec!["--sidechain", p.to_str().unwrap()],
    ] {
        let mut args = vec![
            "audio",
            "finish",
            src.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--overwrite",
        ];
        args.extend(tail);
        assert!(!cli(p, &args).status.success());
        assert_eq!(std::fs::read(&out).unwrap(), b"old output");
    }
    assert!(
        !cli(
            p,
            &[
                "audio",
                "finish",
                src.to_str().unwrap(),
                "-o",
                out.to_str().unwrap()
            ]
        )
        .status
        .success()
    );
    assert!(
        !cli(
            p,
            &[
                "audio",
                "finish",
                src.to_str().unwrap(),
                "-o",
                src.to_str().unwrap(),
                "--overwrite"
            ]
        )
        .status
        .success()
    );
    assert!(
        !cli(
            p,
            &[
                "audio",
                "duck",
                src.to_str().unwrap(),
                "-o",
                src.to_str().unwrap(),
                "--sidechain",
                src.to_str().unwrap(),
                "--overwrite"
            ]
        )
        .status
        .success()
    );
    let sidechain = p.join("sidechain.wav");
    tone(&sidechain, 0.4);
    let sidechain_bytes = std::fs::read(&sidechain).unwrap();
    assert!(
        !cli(
            p,
            &[
                "audio",
                "duck",
                src.to_str().unwrap(),
                "-o",
                sidechain.to_str().unwrap(),
                "--sidechain",
                sidechain.to_str().unwrap(),
                "--overwrite"
            ]
        )
        .status
        .success()
    );
    assert_eq!(std::fs::read(&sidechain).unwrap(), sidechain_bytes);
    assert_eq!(std::fs::read(&src).unwrap(), source);
    success(
        p,
        &[
            "audio",
            "finish",
            src.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--overwrite",
        ],
    );
    assert_eq!(timing(&out).samples, 24000);
    let wrong = p.join("text.wav");
    std::fs::write(&wrong, b"wrong media").unwrap();
    let old = std::fs::read(&out).unwrap();
    assert!(
        !cli(
            p,
            &[
                "audio",
                "finish",
                wrong.to_str().unwrap(),
                "-o",
                out.to_str().unwrap(),
                "--overwrite"
            ]
        )
        .status
        .success()
    );
    assert_eq!(std::fs::read(&out).unwrap(), old);
}
#[test]
fn silence_zero_crossfade_and_nonzero_source_origin() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    let src = p.join("src.wav");
    tone(&src, 0.8);
    let shifted = p.join("shifted.mka");
    ffmpeg(
        &[
            "-v",
            "error",
            "-nostdin",
            "-y",
            "-itsoffset",
            "5",
            "-i",
            src.to_str().unwrap(),
            "-c:a",
            "copy",
        ],
        &shifted,
    );
    let output = p.join("selection.wav");
    let r = success(
        p,
        &[
            "audio",
            "finish",
            shifted.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--start",
            "0.1",
            "--length",
            "0.4",
        ],
    );
    assert!(
        (r["source_selection"]["audio_stream_start_seconds"]
            .as_f64()
            .unwrap()
            - 5.0)
            .abs()
            < 0.01
    );
    assert_eq!(timing(&output).samples, 19200);
    assert!(rms(&pcm(&output, 0.1, 0.1)) > 0.04);
    let concatenated = p.join("concat.wav");
    success(
        p,
        &[
            "audio",
            "loop",
            src.to_str().unwrap(),
            "-o",
            concatenated.to_str().unwrap(),
            "--duration",
            "1.5",
            "--crossfade",
            "0",
        ],
    );
    assert_eq!(timing(&concatenated).samples, 72000);
    let silent = p.join("silence.wav");
    ffmpeg(
        &[
            "-v",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=48000:cl=stereo",
            "-t",
            "0.2",
            "-c:a",
            "pcm_s16le",
        ],
        &silent,
    );
    let r = success(
        p,
        &[
            "audio",
            "normalize",
            silent.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--overwrite",
        ],
    );
    assert_eq!(r["normalization"]["target_achievable"], false);
    assert_eq!(timing(&output).peak_dbfs, None);
}
#[cfg(target_os = "linux")]
#[test]
fn cancellation_reaps_real_ffmpeg_and_preserves_existing_publication() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    use std::time::Instant;
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    let src = p.join("source.wav");
    tone(&src, 3.0);
    let out = p.join("output.wav");
    std::fs::write(&out, b"previous output").unwrap();
    let tools = p.join("tools");
    std::fs::create_dir(&tools).unwrap();
    let marker = p.join("child.pid");
    let wrapper = tools.join("ffmpeg");
    // Only the preparation stage runs in real time. All probes remain real FFmpeg.
    std::fs::write(&wrapper,"#!/bin/sh\nfor arg do\n case \"$arg\" in */prepared.wav) printf '%s' \"$$\" > \"$SKYWRITE_AUDIO_CHILD_PID\"; exec /usr/bin/ffmpeg -re \"$@\";; esac\ndone\nexec /usr/bin/ffmpeg \"$@\"\n").unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .args([
            "audio",
            "finish",
            src.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--overwrite",
        ])
        .env("PATH", format!("{}:/usr/bin:/bin", tools.display()))
        .env("SKYWRITE_AUDIO_CHILD_PID", &marker)
        .env("SKYWRITE_CACHE_DIR", p.join("cache"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    // The wrapper's redirection creates the file before printf writes the PID.
    let pid: i32 = loop {
        if let Some(pid) = std::fs::read_to_string(&marker)
            .ok()
            .and_then(|text| text.parse().ok())
        {
            break pid;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGTERM));
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "real FFmpeg was not reaped"
    );
    assert_eq!(std::fs::read(&out).unwrap(), b"previous output");
    assert_eq!(timing(&src).samples, 144000);
}
