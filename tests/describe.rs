//! Frame descriptions are cached per exact frame and need no key when cached.
use serde_json::{Value, json};
use std::process::Command;

fn run(args: &[&str], cache: &std::path::Path, home: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .env_remove("OPENROUTER_API_KEY")
        .env("SKYWRITE_HOME", home)
        .env("SKYWRITE_CACHE_DIR", cache)
        .args(args)
        .arg("--cache-dir")
        .arg(cache)
        .output()
        .unwrap()
}

fn fixture_video(path: &std::path::Path) {
    let out = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=red:s=160x90:r=30:d=1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn exact_image(cache: &std::path::Path, video: &std::path::Path) -> std::path::PathBuf {
    cache
        .join("assets")
        .join(skywrite::media::file_sha256(video).unwrap())
        .join("exact-frames")
        .join(format!("{:015.6}-fit768.jpg", 0.0))
}

fn dry_frames(
    video: &std::path::Path,
    cache: &std::path::Path,
    home: &std::path::Path,
) -> std::process::Output {
    run(
        &[
            "describe",
            "frames",
            video.to_str().unwrap(),
            "--every",
            "30",
            "--model",
            "fixture/frames",
            "--dry-run",
        ],
        cache,
        home,
    )
}

#[test]
fn frames_replace_truncated_cache_images() {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("v.mp4");
    fixture_video(&video);
    let cache = dir.path().join("cache");
    let image = exact_image(&cache, &video);
    std::fs::create_dir_all(image.parent().unwrap()).unwrap();
    std::fs::write(&image, b"\xff\xd8").unwrap();
    let out = dry_frames(&video, &cache, &dir.path().join("home"));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let bytes = std::fs::read(&image).unwrap();
    assert!(bytes.len() > 2);
    assert!(bytes.starts_with(b"\xff\xd8"));
    assert!(bytes.ends_with(b"\xff\xd9"));
}

#[cfg(unix)]
#[test]
fn frames_publish_private_images_and_directories() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("v.mp4");
    fixture_video(&video);
    let cache = dir.path().join("cache");
    let image = exact_image(&cache, &video);
    // Check creation, then repair of pre-existing public cache permissions.
    for repair in [false, true] {
        if repair {
            std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o644)).unwrap();
            for path in image.ancestors().skip(1).take(3) {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let out = dry_frames(&video, &cache, &dir.path().join("home"));
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            std::fs::metadata(&image).unwrap().permissions().mode() & 0o777,
            0o600
        );
        for path in image.ancestors().skip(1).take(3) {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn frames_replace_symlinked_cache_images_without_touching_targets() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("v.mp4");
    fixture_video(&video);
    let cache = dir.path().join("cache");
    let image = exact_image(&cache, &video);
    std::fs::create_dir_all(image.parent().unwrap()).unwrap();
    let target = dir.path().join("private.txt");
    for exists in [true, false] {
        if exists {
            std::fs::write(&target, b"private input").unwrap();
        } else {
            std::fs::remove_file(&target).unwrap();
            std::fs::remove_file(&image).unwrap();
        }
        symlink(&target, &image).unwrap();
        let out = dry_frames(&video, &cache, &dir.path().join("home"));
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(std::fs::symlink_metadata(&image).unwrap().is_file());
        assert!(std::fs::read(&image).unwrap().starts_with(b"\xff\xd8"));
        if exists {
            assert_eq!(std::fs::read(&target).unwrap(), b"private input");
        } else {
            assert!(!target.exists(), "dangling symlink target was created");
        }
    }
}

#[cfg(unix)]
#[test]
fn frames_reject_symlinked_cache_directories_before_extraction() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("v.mp4");
    fixture_video(&video);
    for level in 0..3 {
        let cache = dir.path().join(format!("cache-{level}"));
        let image = exact_image(&cache, &video);
        let linked = image.ancestors().nth(level + 1).unwrap();
        std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
        let outside = dir.path().join(format!("outside-{level}"));
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, linked).unwrap();
        let out = dry_frames(&video, &cache, &dir.path().join("home"));
        assert!(!out.status.success());
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(error.contains("symlink"), "{error}");
        assert!(error.contains(linked.to_str().unwrap()), "{error}");
        assert!(error.contains("--cache-dir"), "{error}");
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }
}

#[test]
fn describe_outputs_cannot_replace_cast_or_transcript_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("v.mp4");
    fixture_video(&video);
    let cast = dir.path().join("cast.json");
    let words = dir.path().join("words.json");
    let cast_bytes = serde_json::to_vec(
        &json!({"version":1,"people":[{"name":"Host","description":"red jacket"}]}),
    )
    .unwrap();
    let words_bytes = serde_json::to_vec(&json!({"version":1,"source":video,"duration":1,
        "text":"hello","words":[{"text":"hello","start":0,"end":0.5}],
        "usage":{"source_sha256":skywrite::media::file_sha256(&video).unwrap()}}))
    .unwrap();
    std::fs::write(&cast, &cast_bytes).unwrap();
    std::fs::write(&words, &words_bytes).unwrap();
    for (command, input, original) in [
        ("frames", &cast, &cast_bytes),
        ("frames", &words, &words_bytes),
        ("video", &cast, &cast_bytes),
    ] {
        let cache = dir.path().join(format!(
            "{command}-{}",
            input.file_stem().unwrap().to_str().unwrap()
        ));
        let mut args = vec![
            "describe",
            command,
            video.to_str().unwrap(),
            "--model",
            "fixture/describe",
            "--cast",
            cast.to_str().unwrap(),
            "--output",
            input.to_str().unwrap(),
            "--overwrite",
            "--dry-run",
        ];
        if command == "frames" {
            args.extend(["--every", "30", "--transcript", words.to_str().unwrap()]);
        } else {
            args.extend(["--duration", "1"]);
        }
        let out = run(&args, &cache, &dir.path().join("home"));
        assert!(!out.status.success());
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(
            error.contains("output would overwrite an input file"),
            "{error}"
        );
        assert_eq!(&std::fs::read(input).unwrap(), original);
        assert!(!cache.exists(), "cache work preceded input protection");
    }
}

#[test]
fn frames_require_matching_transcript_fingerprint_before_extraction_or_requests() {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("v.mp4");
    let out = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=red:s=160x90:r=30:d=1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&video)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Age the media so the stat-keyed hash cache is written as well as checked.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&video)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
        .unwrap();
    // ctime cannot be backdated: let the file settle so its hash is cached.
    std::thread::sleep(std::time::Duration::from_millis(2100));
    let cache = dir.path().join("cache");
    let home = dir.path().join("home");
    let words = dir.path().join("words.json");
    // The transcript's original path is advisory; matching bytes may be relocated.
    let mut transcript = json!({"version":1,"source":"original.mp4","duration":1,
        "text":"hello","words":[{"text":"hello","start":0,"end":0.5}],
        "usage":{"source_sha256":"0".repeat(64)}});
    let args = [
        "describe",
        "frames",
        video.to_str().unwrap(),
        "--every",
        "30",
        "--model",
        "fixture/frames",
        "--transcript",
        words.to_str().unwrap(),
    ];
    for (usage, message) in [
        (
            json!({"source_sha256":"0".repeat(64)}),
            "transcript source content has changed",
        ),
        (Value::Null, "no source fingerprint"),
        (
            json!({"source_sha256":"g".repeat(64)}),
            "invalid transcript source fingerprint",
        ),
    ] {
        transcript["usage"] = usage;
        std::fs::write(&words, serde_json::to_vec(&transcript).unwrap()).unwrap();
        // No dry-run: validation must precede even constructing the API client.
        let out = run(&args, &cache, &home);
        assert!(!out.status.success());
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(error.contains(message), "{error}");
        assert!(error.contains("transcribe"), "{error}");
        assert!(
            !cache.join("assets").exists(),
            "frames extracted before verification"
        );
    }
    transcript["usage"] =
        json!({"source_sha256":skywrite::media::file_sha256(&video).unwrap().to_uppercase()});
    std::fs::write(&words, serde_json::to_vec(&transcript).unwrap()).unwrap();
    let mut dry_args = args.to_vec();
    dry_args.push("--dry-run");
    let out = run(&dry_args, &cache, &home);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["frames"], 1);
    assert_eq!(report["requests"], 0);
    assert!(cache.join("assets").is_dir());
    assert!(cache.join("media-sha256-v1").is_dir());
}

#[test]
fn seeded_frame_descriptions_are_reused_across_overlapping_passes() {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("v.mp4");
    assert!(
        Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=s=160x90:r=30:d=3",
                "-pix_fmt",
                "yuv420p"
            ])
            .arg(&video)
            .status()
            .unwrap()
            .success()
    );
    let cache = dir.path().join("cache");
    let home = dir.path().join("home");
    let sha = skywrite::media::file_sha256(&video).unwrap();
    let model = "fixture/frames";
    // Seed one second-spaced frames: 0, 1, 2 seconds.
    for second in 0..3 {
        let key = skywrite::describe::frame_cache_key(&sha, second as f64, model, None).unwrap();
        let path = cache.join("frame-descriptions").join(format!("{key}.json"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({"version":1,"model":model,
            "description":{"summary":format!("second {second}")},"usage":{"cost":0.001}}))
            .unwrap(),
        )
        .unwrap();
    }
    let source = video.to_str().unwrap();
    let out = run(
        &[
            "describe", "frames", source, "--every", "30", "--model", model,
        ],
        &cache,
        &home,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["counts"]["cached"], 3);
    assert_eq!(report["cost_usd"], 0.0);
    assert_eq!(report["frames"][1]["description"]["summary"], "second 1");
    assert_eq!(report["frames"][1]["frame"], 30);
    assert!(std::path::Path::new(report["frames"][1]["image"].as_str().unwrap()).is_file());

    // A dense pass around 1s shares the seeded frame at exactly 1.0s.
    let dry = run(
        &[
            "describe",
            "frames",
            source,
            "--around",
            "1",
            "--window",
            "0.2",
            "--model",
            model,
            "--dry-run",
        ],
        &cache,
        &home,
    );
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let dry: Value = serde_json::from_slice(&dry.stdout).unwrap();
    assert_eq!(dry["frames"], 3);
    assert_eq!(dry["cached"], 1);
    assert_eq!(dry["requests"], 0);

    // Uncached frames need a key, and the frame budget is enforced first.
    let budget = run(
        &[
            "describe",
            "frames",
            source,
            "--every",
            "2",
            "--model",
            model,
            "--max-frames",
            "5",
        ],
        &cache,
        &home,
    );
    assert!(!budget.status.success());
    assert!(String::from_utf8_lossy(&budget.stderr).contains("--max-frames"));

    // A cast changes the cache identity.
    let cast = dir.path().join("cast.json");
    std::fs::write(
        &cast,
        r#"{"version":1,"people":[{"name":"Host","description":"red jacket"}]}"#,
    )
    .unwrap();
    let with_cast = run(
        &[
            "describe",
            "frames",
            source,
            "--every",
            "30",
            "--model",
            model,
            "--cast",
            cast.to_str().unwrap(),
            "--dry-run",
        ],
        &cache,
        &home,
    );
    let with_cast: Value = serde_json::from_slice(&with_cast.stdout).unwrap();
    assert_eq!(with_cast["cached"], 0);
    std::fs::write(
        &cast,
        r#"{"version":1,"people":[{"name":"unknown","description":"x"}]}"#,
    )
    .unwrap();
    let invalid = run(
        &[
            "describe",
            "frames",
            source,
            "--every",
            "30",
            "--model",
            model,
            "--cast",
            cast.to_str().unwrap(),
            "--dry-run",
        ],
        &cache,
        &home,
    );
    assert!(!invalid.status.success());
}
