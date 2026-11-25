//! The built-in `ondevice` provider through the CLI: selection, listing,
//! consent-only downloads and removal, all without network or keys. The one
//! test that runs the real engine needs a downloaded model and is skipped
//! unless SKYWRITE_TEST_ONDEVICE_CACHE names a cache that holds it.
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MODEL: &str = "ondevice:parakeet-tdt-0.6b-v3";

struct Env {
    dir: tempfile::TempDir,
}
impl Env {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn cache(&self) -> PathBuf {
        self.path().join("cache")
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_skywrite"))
            .env_remove("OPENROUTER_API_KEY")
            .env_remove("SKYWRITE_TRANSCRIPTION_MODEL")
            .env_remove("SKYWRITE_ANALYSIS_MODEL")
            .env_remove("SKYWRITE_FRAME_DESCRIPTION_MODEL")
            .env("SKYWRITE_HOME", self.path().join("home"))
            .env("SKYWRITE_CACHE_DIR", self.cache())
            // Any accidental network use fails fast instead of reaching out.
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .args(args)
            .output()
            .unwrap()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn fails(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "{args:?} unexpectedly succeeded");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
    fn media(&self, name: &str, filter: &str) -> String {
        let path = self.path().join(name);
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-f", "lavfi", "-i", filter])
            .args(["-c:a", "pcm_s16le"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        path.to_str().unwrap().to_owned()
    }
}

#[test]
fn ondevice_is_a_reserved_built_in_transcription_provider() {
    let env = Env::new();
    let error = env.fails(&[
        "providers",
        "add",
        "ondevice",
        "--base-url",
        "http://127.0.0.1:8080/v1",
    ]);
    assert!(error.contains("reserved"), "{error}");
    let listed = env.ok(&["providers", "list"]);
    let ondevice = listed["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "ondevice")
        .expect("ondevice listed");
    assert_eq!(ondevice["tasks"], serde_json::json!(["transcription"]));
    assert_eq!(ondevice["builtin"], true);
    assert!(
        env.fails(&["providers", "remove", "ondevice"])
            .contains("cannot be removed")
    );
    // Selected like any transcription model; refused for other tasks.
    let saved = env.ok(&["defaults", "transcription", "set", MODEL]);
    assert_eq!(saved["model"], MODEL, "{saved}");
    let error = env.fails(&["defaults", "analysis", "set", MODEL]);
    assert!(
        error.contains("on-device transcription models only"),
        "{error}"
    );
    let video = env.media("tone.wav", "sine=f=440:d=1");
    let error = env.fails(&[
        "describe", "frames", &video, "--every", "1", "--model", MODEL,
    ]);
    assert!(
        error.contains("on-device transcription models only"),
        "{error}"
    );
    let error = env.fails(&["transcribe", &video, "--model", "ondevice:whisper"]);
    assert!(error.contains("unknown on-device model"), "{error}");
}

#[test]
fn listing_shows_status_size_and_attribution_without_network() {
    let env = Env::new();
    let listed = env.ok(&["models", "list", "--provider", "ondevice"]);
    assert_eq!(listed["network_requests"], false);
    let model = &listed["models"][0];
    assert_eq!(model["reference"], MODEL);
    assert_eq!(model["downloaded"], false);
    assert_eq!(model["bytes"], 739_508_576u64);
    assert_eq!(model["license"], "CC-BY-4.0");
    assert!(model["attribution"].as_str().unwrap().contains("NVIDIA"));
    assert!(
        model["path"]
            .as_str()
            .unwrap()
            .starts_with(env.cache().to_str().unwrap())
    );
    let other = env.ok(&[
        "models",
        "list",
        "--provider",
        "ondevice",
        "--input",
        "image",
    ]);
    assert_eq!(other["models"], serde_json::json!([]));
    let error = env.fails(&[
        "models",
        "list",
        "--provider",
        "ondevice",
        "--task",
        "analysis",
    ]);
    assert!(error.contains("transcription models only"), "{error}");
    let removed = env.ok(&["models", "remove", MODEL]);
    assert_eq!(removed["was_present"], false);
    assert!(
        env.fails(&["models", "remove", "vendor/model"])
            .contains("not an on-device model")
    );
}

#[test]
fn transcribing_never_downloads_without_consent() {
    let env = Env::new();
    let audio = env.media("tone.wav", "sine=f=440:d=2");
    let error = env.fails(&["transcribe", &audio, "--model", MODEL]);
    match skywrite::ondevice::engine_unavailable() {
        // An engine that cannot run here is reported before any model check.
        Some(reason) => {
            assert!(error.contains(&reason), "{error}");
            let error = env.fails(&["transcribe", &audio, "--model", MODEL, "--download"]);
            assert!(error.contains(&reason), "{error}");
        }
        None => {
            assert!(error.contains("is not downloaded"), "{error}");
            assert!(error.contains("skywrite models download ondevice:parakeet-tdt-0.6b-v3"));
            assert!(error.contains("--download"));
        }
    }
    // Nothing was fetched or created for the model.
    assert!(!env.cache().join("ondevice").exists());
    // The default model (hosted) with --download is a usage error before any request.
    let error = env.fails(&["transcribe", &audio, "--download"]);
    assert!(
        error.contains("--download applies only to on-device models"),
        "{error}"
    );
}

#[test]
fn download_never_deletes_the_media_being_transcribed() {
    if skywrite::ondevice::engine_unavailable().is_some() {
        eprintln!("skipping: the on-device engine is unavailable in this build or CPU");
        return;
    }
    let env = Env::new();
    // Media placed exactly where the (damaged) model file would be.
    let dir = env.cache().join("ondevice/parakeet-tdt-0.6b-v3");
    std::fs::create_dir_all(&dir).unwrap();
    let model = dir.join("parakeet-tdt-0.6b-v3-Q8_0.gguf");
    let media = env.media("tone.wav", "sine=f=440:d=1");
    std::fs::rename(&media, &model).unwrap();
    let before = std::fs::read(&model).unwrap();
    for input in [
        model.clone(),
        dir.join("parakeet-tdt-0.6b-v3-Q8_0.gguf.part"),
    ] {
        if input != model {
            std::fs::rename(&model, &input).unwrap();
        }
        let error = env.fails(&[
            "transcribe",
            input.to_str().unwrap(),
            "--model",
            MODEL,
            "--download",
        ]);
        assert!(
            error.contains("is the on-device model cache file"),
            "{error}"
        );
        assert_eq!(std::fs::read(&input).unwrap(), before);
    }
}

/// Real engine on synthetic audio: digital silence yields no words, a
/// sub-second tail chunk works, and the transcript keeps its usual shape.
#[test]
fn real_engine_on_silence_and_a_short_tail() {
    let Some(cache) = std::env::var_os("SKYWRITE_TEST_ONDEVICE_CACHE") else {
        eprintln!("skipping real on-device engine test: SKYWRITE_TEST_ONDEVICE_CACHE is not set");
        return;
    };
    let env = Env::new();
    let audio = env.media("silence.wav", "anullsrc=r=16000:cl=mono:d=10.4");
    let out = Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .env_remove("OPENROUTER_API_KEY")
        .env("SKYWRITE_HOME", env.path().join("home"))
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .args([
            "transcribe",
            &audio,
            "--model",
            MODEL,
            "--chunk-seconds",
            "10",
            "--cache-dir",
        ])
        .arg(&cache)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let transcript: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(transcript["model"], MODEL);
    assert_eq!(transcript["usage"]["total_chunks"], 2);
    assert_eq!(transcript["words"], serde_json::json!([]));
    let chunk = &transcript["usage"]["chunks"][0]["usage"];
    assert_eq!(chunk["cost"], 0);
    assert_eq!(chunk["model_sha256"].as_str().unwrap().len(), 64);
}

#[test]
fn hosted_transcription_listing_adds_on_device_models_only_when_filters_allow() {
    let env = Env::new();
    let endpoint = "models?output_modalities=transcription";
    let key = skywrite::digest::cache_key(&[b"catalog-v1", endpoint.as_bytes()]);
    skywrite::io::write_json(
        &env.cache().join("models").join(format!("{key}.json")),
        &serde_json::json!({"version":1,"endpoint":endpoint,"fetched_at":1,"body":{"data":[
            {"id":"vendor/asr","architecture":{"input_modalities":["audio"],"output_modalities":["text"]}}
        ]}}),
        false,
    )
    .unwrap();
    let listed = env.ok(&["models", "list", "--task", "transcription", "--offline"]);
    assert_eq!(listed["models"][0]["id"], "vendor/asr");
    assert_eq!(listed["ondevice_models"][0]["reference"], MODEL);
    assert_eq!(listed["ondevice_models"][0]["license"], "CC-BY-4.0");
    for filter in [
        ["--input", "image"],
        ["--output", "speech"],
        ["--provider", "openrouter"],
    ] {
        let mut args = vec!["models", "list", "--task", "transcription", "--offline"];
        args.extend(filter);
        let listed = env.ok(&args);
        assert!(listed.get("ondevice_models").is_none(), "{filter:?}");
    }
}
