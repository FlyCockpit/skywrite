//! Edit and XML files are trusted input, but exports list every source outside
//! the edit's directory, and symlinked or special-file sources are refused.
#![cfg(unix)]
use serde_json::{Value, json};
use skywrite::{export, timeline};
use std::{
    ffi::CString,
    os::unix::{ffi::OsStrExt, fs::symlink},
    path::{Path, PathBuf},
    process::Command,
};

const TARGETS: [&str; 5] = ["fcpxml", "premiere", "ffmpeg", "hyperframes", "remotion"];

fn tone(path: &Path) {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
        .arg("sine=frequency=440:sample_rate=48000:duration=1")
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn fifo(path: &Path) {
    let name = CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
}

fn write_edit(path: &Path, sources: &[&str]) {
    let clips: Vec<Value> = sources
        .iter()
        .enumerate()
        .map(|(i, s)| json!({"start": i as f64 * 0.5, "source": s, "duration": 0.5}))
        .collect();
    let edit = json!({"version": 1, "name": "trust", "width": 32, "height": 32,
        "fps": {"num": 10, "den": 1}, "audio": [{"clips": clips}]});
    std::fs::write(path, serde_json::to_vec(&edit).unwrap()).unwrap();
}

fn export_all(edit: &Path, out: &Path) -> Vec<Value> {
    let validated = timeline::load_and_validate(edit).unwrap();
    TARGETS
        .iter()
        .map(|target| {
            let output = out.join(match *target {
                "fcpxml" => "cut.fcpxml".into(),
                "premiere" => "cut.xml".into(),
                "ffmpeg" => "render.py".into(),
                other => format!("{other}-project"),
            });
            export::export(&validated, target, &output, false).unwrap()
        })
        .collect()
}

fn listed(report: &Value) -> Vec<PathBuf> {
    report["external_sources"]
        .as_array()
        .unwrap_or_else(|| panic!("external_sources missing: {report}"))
        .iter()
        .map(|p| PathBuf::from(p.as_str().unwrap()))
        .collect()
}

#[test]
fn inside_sources_are_not_listed_and_exports_are_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let edits = dir.path().join("edits");
    std::fs::create_dir_all(edits.join("media")).unwrap();
    tone(&edits.join("media/a.wav"));
    tone(&edits.join("b.wav"));
    let edit = edits.join("edit.json");
    write_edit(&edit, &["media/a.wav", "b.wav"]);
    for report in export_all(&edit, dir.path()) {
        assert_eq!(listed(&report), Vec::<PathBuf>::new(), "{report}");
    }
    // The project copies both sources in clip order, as before.
    let media = dir.path().join("remotion-project/public/media");
    let mut copied: Vec<_> = std::fs::read_dir(&media)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    copied.sort();
    assert_eq!(copied, ["asset-0000.wav", "asset-0001.wav"]);
    assert_eq!(
        std::fs::read(media.join("asset-0000.wav")).unwrap(),
        std::fs::read(edits.join("media/a.wav")).unwrap()
    );
}

#[test]
fn outside_sources_are_listed_by_every_export() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let edits = dir.path().join("edits");
    std::fs::create_dir_all(&edits).unwrap();
    tone(&edits.join("inside.wav"));
    tone(&dir.path().join("parent.wav"));
    let absolute = other.path().join("private.wav");
    tone(&absolute);
    let edit = edits.join("edit.json");
    write_edit(
        &edit,
        &[
            "inside.wav",
            "../parent.wav",
            absolute.to_str().unwrap(),
            "../parent.wav",
        ],
    );
    let mut expected = vec![
        dir.path().join("parent.wav").canonicalize().unwrap(),
        absolute.canonicalize().unwrap(),
    ];
    expected.sort();
    for report in export_all(&edit, dir.path()) {
        assert_eq!(listed(&report), expected, "{report}");
    }
}

#[test]
fn symlinked_and_special_sources_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real.wav");
    tone(&real);
    symlink(&real, dir.path().join("link.wav")).unwrap();
    fifo(&dir.path().join("pipe.wav"));
    let edit = dir.path().join("edit.json");
    for (source, needle) in [("link.wav", "symlink"), ("pipe.wav", "not a regular file")] {
        write_edit(&edit, &[source]);
        let error = format!("{:#}", timeline::load_and_validate(&edit).unwrap_err());
        assert!(error.contains(needle), "{source}: {error}");
    }
    // A directory symlink above the file is fine (system temp dirs often are).
    let aliased = dir.path().join("aliased");
    symlink(dir.path(), &aliased).unwrap();
    write_edit(&aliased.join("edit.json"), &["real.wav"]);
    timeline::load_and_validate(&aliased.join("edit.json")).unwrap();
}

#[test]
fn project_copy_refuses_a_source_swapped_for_a_symlink_after_validation() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real.wav");
    let secret = dir.path().join("secret.wav");
    tone(&real);
    tone(&secret);
    let edit = dir.path().join("edit.json");
    write_edit(&edit, &["real.wav"]);
    let validated = timeline::load_and_validate(&edit).unwrap();
    std::fs::remove_file(&real).unwrap();
    symlink(&secret, &real).unwrap();
    for target in ["hyperframes", "remotion"] {
        let output = dir.path().join(target);
        let error = format!(
            "{:#}",
            export::export(&validated, target, &output, false).unwrap_err()
        );
        assert!(error.contains("symlink"), "{error}");
        assert!(!output.exists());
    }
}

#[test]
fn xml_import_refuses_symlinked_and_special_media() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real.wav");
    tone(&real);
    symlink(&real, dir.path().join("link.wav")).unwrap();
    fifo(&dir.path().join("pipe.wav"));
    for (name, needle) in [
        ("link.wav", "symlink"),
        ("pipe.wav", "not a regular file"),
        ("real.wav", ""),
        ("missing.wav", ""),
    ] {
        let url = url::Url::from_file_path(dir.path().join(name)).unwrap();
        let xml = format!(
            r#"<xmeml><file id="f"><pathurl>{url}</pathurl></file>
            <sequence><rate><timebase>30</timebase></rate><duration>30</duration>
            <media><audio><track><clipitem id="c"><start>0</start><end>15</end><in>0</in>
            <out>15</out><file id="f"/></clipitem></track></audio></media></sequence></xmeml>"#
        );
        let input = dir.path().join("timeline.xml");
        std::fs::write(&input, xml).unwrap();
        let output = dir.path().join(format!("{name}.json"));
        let out = Command::new(env!("CARGO_BIN_EXE_skywrite"))
            .args(["edit", "import"])
            .arg(&input)
            .arg("-o")
            .arg(&output)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        if needle.is_empty() {
            assert!(out.status.success(), "{name}: {stderr}");
        } else {
            assert!(!out.status.success(), "{name} was imported");
            assert!(stderr.contains(needle), "{name}: {stderr}");
            assert!(!output.exists());
        }
    }
}

fn write_edit_with(path: &Path, clip: &str, extra: Value) {
    let mut edit = json!({"version": 1, "name": "trust", "width": 32, "height": 32,
        "fps": {"num": 10, "den": 1},
        "audio": [{"clips": [{"start": 0, "source": clip, "duration": 1}]}]});
    edit.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    std::fs::write(path, serde_json::to_vec(&edit).unwrap()).unwrap();
}

#[test]
fn caption_files_and_declared_sources_follow_the_same_rules() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    tone(&dir.path().join("a.wav"));
    let unused = other.path().join("unused.wav");
    tone(&unused);
    let srt = "1\n00:00:00,000 --> 00:00:00,500\nHello\n";
    let outside_srt = other.path().join("subs.srt");
    std::fs::write(&outside_srt, srt).unwrap();
    std::fs::write(dir.path().join("real.srt"), srt).unwrap();
    symlink(dir.path().join("real.srt"), dir.path().join("link.srt")).unwrap();
    let edit = dir.path().join("edit.json");

    // A symlinked caption file is refused like a symlinked clip.
    write_edit_with(
        &edit,
        "a.wav",
        json!({"captions": [{"source": "link.srt"}]}),
    );
    let error = format!("{:#}", timeline::load_and_validate(&edit).unwrap_err());
    assert!(error.contains("symlink"), "{error}");

    // Outside caption files and declared sources are listed.
    write_edit_with(
        &edit,
        "a.wav",
        json!({"captions": [{"source": outside_srt}], "sources": [{"path": unused}]}),
    );
    let validated = timeline::load_and_validate(&edit).unwrap();
    let report =
        export::export(&validated, "fcpxml", &dir.path().join("cut.fcpxml"), false).unwrap();
    let mut expected = vec![
        outside_srt.canonicalize().unwrap(),
        unused.canonicalize().unwrap(),
    ];
    expected.sort();
    assert_eq!(listed(&report), expected, "{report}");
}

#[test]
fn project_copy_refuses_a_parent_directory_swapped_after_validation() {
    let dir = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("media")).unwrap();
    tone(&dir.path().join("media/a.wav"));
    tone(&private.path().join("a.wav"));
    let edit = dir.path().join("edit.json");
    write_edit(&edit, &["media/a.wav"]);
    let validated = timeline::load_and_validate(&edit).unwrap();
    std::fs::rename(dir.path().join("media"), dir.path().join("old")).unwrap();
    symlink(private.path(), dir.path().join("media")).unwrap();
    let output = dir.path().join("project");
    let error = format!(
        "{:#}",
        export::export(&validated, "remotion", &output, false).unwrap_err()
    );
    assert!(error.contains("changed since validation"), "{error}");
    assert!(!output.exists());
}
