//! Captions generated from source transcripts follow the edit's cuts.
use serde_json::{Value, json};
use skywrite::{export, timeline};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, SystemTime},
};

#[test]
fn point_words_convert_and_load_as_json_caption_sources() {
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("words.json");
    skywrite::io::write_json(
        &transcript,
        &json!({"version":1,"source":"speech.wav","duration":1,"text":"hello",
            "words":[{"text":"hello","start":0.5,"end":0.5}]}),
        false,
    )
    .unwrap();
    let subtitle = dir.path().join("words.srt");
    let output = Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .args(["captions", "convert"])
        .arg(&transcript)
        .arg("-o")
        .arg(&subtitle)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_to_string(&subtitle)
            .unwrap()
            .contains("00:00:00,500 --> 00:00:00,501")
    );
    let edit = timeline::parse(json!({"version":1,"duration":1,
        "captions":[{"source":"words.json"}]}))
    .unwrap();
    let validated = timeline::validate(&edit, dir.path()).unwrap();
    assert_eq!(validated.summary()["caption_track_count"], 1);
}

#[test]
fn point_words_at_24_hour_boundary_are_rejected() {
    // Mapping an otherwise valid point word can also land at the boundary.
    for (point, offset) in [(86400.0, 0.0), (0.5, 86399.5)] {
        let transcript: skywrite::transcript::Transcript = serde_json::from_value(json!({
            "version":1,"source":"speech.wav","duration":86400,"text":"hello",
            "words":[{"text":"hello","start":point,"end":point}]
        }))
        .unwrap();
        let options = skywrite::captions::ConvertOptions {
            offset,
            ..Default::default()
        };
        let error = skywrite::captions::words(&transcript, &options)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "mapped word timestamp outside positive 24-hour timeline"
        );
    }
}

#[test]
fn transcript_captions_follow_cuts_mutes_and_exports() {
    // Isolate the environment for library calls without mutating it while
    // other tests run on parallel threads.
    const CHILD: &str = "SKYWRITE_CAPTIONS_EDIT_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let cache = tempfile::tempdir().unwrap();
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "transcript_captions_follow_cuts_mutes_and_exports",
                "--test-threads=3",
            ])
            .env(CHILD, "1")
            .env("SKYWRITE_CACHE_DIR", cache.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("talk.mp4");
    assert!(
        Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=red:s=160x90:r=30:d=6",
                "-f",
                "lavfi",
                "-i",
                "sine=d=6",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-shortest"
            ])
            .arg(&video)
            .status()
            .unwrap()
            .success()
    );
    let words = ["one", "two", "three", "four", "five"]
        .iter()
        .enumerate()
        .map(|(i, w)| json!({"text":w,"start":0.2 + i as f64,"end":0.4 + i as f64}))
        .collect::<Vec<_>>();
    std::fs::write(
        dir.path().join("talk.json"),
        serde_json::to_vec(&json!({"version":1,"source":"talk.mp4","duration":6.0,"text":"one two three four five","words":words,"usage":{"source_sha256":skywrite::media::file_sha256(&video).unwrap()}})).unwrap(),
    )
    .unwrap();
    let edit = dir.path().join("edit.json");
    std::fs::write(
        &edit,
        serde_json::to_vec(
            &json!({"version":1,"name":"Captions","width":160,"height":90,"fps":{"num":30,"den":1},
        "video":[{"clips":[
            {"start":0,"source":"talk.mp4","source_in":0,"duration":1.5},
            {"start":1.5,"source":"talk.mp4","source_in":3,"duration":1.5},
            {"start":3,"source":"talk.mp4","source_in":2,"duration":1,"audio":{"volume":0}}
        ]}],
        "captions":[{"from_transcripts":["talk.json"],"language":"eng"}]}),
        )
        .unwrap(),
    )
    .unwrap();
    let validated = timeline::load_and_validate(&edit).unwrap();

    let srt = dir.path().join("out.srt");
    let out = Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .args(["edit", "captions"])
        .arg(&edit)
        .arg("-o")
        .arg(&srt)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::fs::read_to_string(&srt).unwrap();
    assert!(text.contains("one two"), "{text}");
    assert!(text.contains("four five"), "{text}");
    assert!(
        !text.contains("three"),
        "muted or cut words must not be captioned: {text}"
    );
    // "four" (source 3.2s) plays at 1.5 + 0.2 = 1.7s on the output timeline.
    assert!(text.contains("00:00:01,700"), "{text}");

    let xml = dir.path().join("premiere.xml");
    let result = export::export(&validated, "premiere", &xml, false).unwrap();
    let sidecar = dir.path().join("premiere.eng.srt");
    assert_eq!(result["caption_files"][0], json!(sidecar));
    assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), text);
    // Sidecars follow the same no-clobber rule as the export.
    assert!(
        export::export(
            &validated,
            "premiere",
            &dir.path().join("premiere2.xml"),
            false
        )
        .is_ok()
    );
    std::fs::remove_file(dir.path().join("premiere2.xml")).unwrap();
    assert!(
        export::export(
            &validated,
            "premiere",
            &dir.path().join("premiere2.xml"),
            false
        )
        .is_err()
    );

    let fcpxml = dir.path().join("edit.fcpxml");
    export::export(&validated, "fcpxml", &fcpxml, false).unwrap();
    let body = std::fs::read_to_string(&fcpxml).unwrap();
    assert_eq!(body.matches("<caption lane=").count(), 2);
    assert!(body.contains(">four five<"));

    let bad = dir.path().join("bad.json");
    std::fs::write(
        &bad,
        serde_json::to_vec(&json!({"version":1,"duration":1,
        "captions":[{"source":"x.srt","from_transcripts":["talk.json"]}]}))
        .unwrap(),
    )
    .unwrap();
    assert!(timeline::load_and_validate(&bad).is_err());
}

fn caption_fixture(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let media = dir.join("talk.wav");
    let out = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-nostdin",
            "-n",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=16000:duration=1",
            "-c:a",
            "pcm_s16le",
        ])
        .arg(&media)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let transcript = dir.join("talk.json");
    fs::write(
        &transcript,
        serde_json::to_vec(&json!({
            "version":1,"source":"talk.wav","duration":1,"text":"hello",
            "words":[{"text":"hello","start":0.1,"end":0.5}],
            "usage":{"source_sha256":skywrite::media::file_sha256(&media).unwrap()}
        }))
        .unwrap(),
    )
    .unwrap();
    let edit = dir.join("edit.json");
    fs::write(
        &edit,
        serde_json::to_vec(&json!({
            "version":1,"duration":1,
            "audio":[{"clips":[{"source":"talk.wav","start":0,"duration":1}]}],
            "captions":[{"from_transcripts":["talk.json"]}]
        }))
        .unwrap(),
    )
    .unwrap();
    (media, transcript, edit)
}

fn validate_captions(edit: &Path, cache: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .env("SKYWRITE_CACHE_DIR", cache)
        .args(["edit", "validate"])
        .arg(edit)
        .output()
        .unwrap()
}

fn update_fingerprint(transcript: &Path, fingerprint: Option<String>) {
    let mut value: Value = serde_json::from_slice(&fs::read(transcript).unwrap()).unwrap();
    value["usage"] = match fingerprint {
        Some(hash) => json!({"source_sha256":hash}),
        None => json!({}),
    };
    fs::write(transcript, serde_json::to_vec(&value).unwrap()).unwrap();
}

fn assert_changed(out: Output, media: &Path) {
    assert!(!out.status.success());
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(
        error.contains("transcript source content has changed"),
        "{error}"
    );
    assert!(error.contains("transcribe"), "{error}");
    assert!(error.contains(media.to_str().unwrap()), "{error}");
}

#[test]
fn transcript_captions_reject_mismatched_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    let (media, transcript, edit) = caption_fixture(dir.path());
    update_fingerprint(&transcript, Some("0".repeat(64)));
    assert_changed(validate_captions(&edit, &dir.path().join("cache")), &media);
}

#[test]
fn transcript_captions_reject_missing_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    let (_, transcript, edit) = caption_fixture(dir.path());
    update_fingerprint(&transcript, None);
    let out = validate_captions(&edit, &dir.path().join("cache"));
    assert!(!out.status.success());
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(error.contains("no source fingerprint"), "{error}");
}

#[test]
fn transcript_captions_accept_matching_uppercase_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    let (media, transcript, edit) = caption_fixture(dir.path());
    update_fingerprint(
        &transcript,
        Some(
            skywrite::media::file_sha256(&media)
                .unwrap()
                .to_ascii_uppercase(),
        ),
    );
    let out = validate_captions(&edit, &dir.path().join("cache"));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn transcript_captions_cache_invalidates_same_length_rewrite_with_restored_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let (media, _, edit) = caption_fixture(dir.path());
    fs::OpenOptions::new()
        .write(true)
        .open(&media)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
    // ctime cannot be backdated: let the file settle so its hash is cached.
    std::thread::sleep(Duration::from_millis(2100));
    let before = fs::metadata(&media).unwrap();
    let cache = dir.path().join("cache");
    let out = validate_captions(&edit, &cache);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let entries = fs::read_dir(cache.join("media-sha256-v1"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert!(entries[0].is_file());

    let mut changed = fs::read(&media).unwrap();
    *changed.last_mut().unwrap() ^= 1;
    fs::write(&media, changed).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&media)
        .unwrap()
        .set_modified(before.modified().unwrap())
        .unwrap();
    let after = fs::metadata(&media).unwrap();
    assert_eq!(before.len(), after.len());
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    assert_changed(validate_captions(&edit, &cache), &media);
}
