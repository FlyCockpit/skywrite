//! Signal exit keeps its Unix status while reclaiming render and review scratch space.
#![cfg(unix)]
use serde_json::json;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            // Also ask the supervisor to kill and reap its FFmpeg group on failure.
            unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.0.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
fn has_prefix(dir: &Path, prefix: &str) -> bool {
    std::fs::read_dir(dir).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(prefix)
    })
}

#[test]
fn interrupted_render_removes_temporary_outputs_and_sidecar_directories() {
    interrupted_render(false);
}

#[test]
fn interrupted_review_removes_temporary_render_and_sidecar_directories() {
    interrupted_render(true);
}

fn interrupted_render(review: bool) {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.mkv");
    let fixture = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=blue:size=32x32:rate=10:duration=180",
            "-c:v",
            "ffv1",
            "-threads",
            "1",
        ])
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        fixture.status.success(),
        "{}",
        String::from_utf8_lossy(&fixture.stderr)
    );
    let edit = dir.path().join("edit.json");
    std::fs::write(
        &edit,
        serde_json::to_vec(&json!({
            "version":1, "name":"Interrupt render", "width":1280, "height":720,
            "fps":{"num":30,"den":1},
            "video":[{"clips":[
                {"start":0,"source":source,"duration":90},
                {"start":90,"source":source,"source_in":90,"duration":90}
            ]}]
        }))
        .unwrap(),
    )
    .unwrap();
    let child_tmp = dir.path().join("child-tmp");
    std::fs::create_dir(&child_tmp).unwrap();
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let output = dir.path().join(if review {
            format!("cuts-{signal}")
        } else {
            "output.mp4".to_owned()
        });
        let mut command = Command::new(env!("CARGO_BIN_EXE_skywrite"));
        if review {
            command
                .args(["review", "cuts"])
                .arg(&edit)
                .arg("--output-dir");
        } else {
            command.args(["edit", "render"]).arg(&edit).arg("--output");
        }
        let mut child = Running(
            command
                .arg(&output)
                .env("TMPDIR", &child_tmp)
                .env("SKYWRITE_CACHE_DIR", dir.path().join("cache"))
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let scratch_dir = if review { &output } else { dir.path() };
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if scratch_dir.is_dir()
                && has_prefix(scratch_dir, ".skywrite-render-")
                && has_prefix(&child_tmp, "skywrite-sidecars-")
                && std::fs::read_dir(scratch_dir).unwrap().any(|entry| {
                    let entry = entry.unwrap();
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".skywrite-render-")
                        && entry.metadata().unwrap().len() > 0
                })
            {
                break;
            }
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "render exited before signal"
            );
            assert!(
                Instant::now() < deadline,
                "render did not create its scratch files"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // SAFETY: this PID belongs to the live child owned above.
        assert_eq!(unsafe { libc::kill(child.0.id() as i32, signal) }, 0);
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "render did not exit after signal"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.signal(), Some(signal));
        assert!(!has_prefix(scratch_dir, ".skywrite-render-"));
        assert!(!has_prefix(scratch_dir, ".skywrite-mux-"));
        assert!(!has_prefix(scratch_dir, ".skywrite-review-"));
        assert!(!has_prefix(&child_tmp, "skywrite-sidecars-"));
        if review {
            assert_eq!(std::fs::read_dir(&output).unwrap().count(), 0);
        } else {
            assert!(!output.exists());
        }
    }
}
