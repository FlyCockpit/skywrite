//! External-reference formats are rejected while ordinary media remains usable.
use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Output},
};

fn inspect(root: &Path, input: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .arg("inspect")
        .arg(input)
        .env("HOME", root.join("home"))
        .env("XDG_CACHE_HOME", root.join("home/.cache"))
        .env("SKYWRITE_HOME", root.join("personal"))
        .env("SKYWRITE_CACHE_DIR", root.join("cache"))
        .output()
        .unwrap()
}

fn fixture(path: &Path, input: &str, encoding: &[&str]) {
    let output = Command::new("ffmpeg")
        .args([
            "-v", "error", "-nostdin", "-n", "-f", "lavfi", "-i", input, "-t", "1", "-threads", "1",
        ])
        .args(encoding)
        .arg(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_rejected(output: Output, demuxer: &str) {
    assert!(!output.status.success(), "{demuxer} was accepted");
    let error: Value = serde_json::from_slice(&output.stderr).expect("JSON error response");
    assert_eq!(error["error"]["code"], "operation_failed", "{error}");
    let message = error.to_string();
    assert!(
        message.contains(&format!("unsupported media container '{demuxer}'")),
        "{message}"
    );
    assert!(
        message.contains("use the underlying media files"),
        "{message}"
    );
}

#[test]
fn local_hls_playlist_referencing_real_segment_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    // Put the segment outside the playlist directory to cover local traversal.
    let segment = temp.path().join("segment.ts");
    fixture(
        &segment,
        "testsrc2=size=160x90:rate=25",
        &["-c:v", "mpeg2video", "-f", "mpegts"],
    );
    let playlists = temp.path().join("playlists");
    std::fs::create_dir(&playlists).unwrap();
    let playlist = playlists.join("playlist.m3u8");
    std::fs::write(
        &playlist,
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:1.0,\n../segment.ts\n#EXT-X-ENDLIST\n",
    )
    .unwrap();
    assert_rejected(inspect(temp.path(), &playlist), "hls");
}

#[test]
fn ffconcat_script_referencing_real_wav_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    fixture(&temp.path().join("a.wav"), "sine=frequency=440", &[]);
    let script = temp.path().join("playlist.ffconcat");
    std::fs::write(&script, "ffconcat version 1.0\nfile 'a.wav'\n").unwrap();
    assert_rejected(inspect(temp.path(), &script), "concat");
}

#[test]
fn normal_mp4_and_wav_are_still_inspected() {
    let temp = tempfile::tempdir().unwrap();
    let mp4 = temp.path().join("video.mp4");
    let wav = temp.path().join("audio.wav");
    fixture(
        &mp4,
        "testsrc2=size=160x90:rate=25",
        &["-c:v", "libx264", "-pix_fmt", "yuv420p"],
    );
    fixture(&wav, "sine=frequency=440", &[]);
    for (path, expected) in [(&mp4, "mp4"), (&wav, "wav")] {
        let output = inspect(temp.path(), path);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).expect("JSON response");
        assert!(
            value["media"][0]["format_name"]
                .as_str()
                .unwrap()
                .split(',')
                .any(|name| name == expected),
            "{value}"
        );
    }
}

#[test]
fn rejected_playlist_never_opens_the_files_it_names() {
    let temp = tempfile::tempdir().unwrap();
    // Opening a FIFO with no writer blocks, so ffprobe would hang until its
    // timeout if the HLS demuxer were allowed to read the playlist.
    let fifo = temp.path().join("blocking.ts");
    let made = Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(made.success());
    let playlist = temp.path().join("playlist.m3u8");
    std::fs::write(
        &playlist,
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXTINF:1.0,\nblocking.ts\n#EXT-X-ENDLIST\n",
    )
    .unwrap();
    let started = std::time::Instant::now();
    let output = inspect(temp.path(), &playlist);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "ffprobe opened the playlist entry"
    );
    assert_rejected(output, "hls");
}

#[test]
fn image_names_that_ffmpeg_reads_as_sequences_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let literal = temp.path().join("frame%03d.png");
    fixture(
        &temp.path().join("frame000.png"),
        "testsrc2=size=32x32",
        &["-frames:v", "1"],
    );
    std::fs::copy(temp.path().join("frame000.png"), &literal).unwrap();
    let output = inspect(temp.path(), &literal);
    assert!(!output.status.success(), "sequence pattern was accepted");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("containing '%'"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
