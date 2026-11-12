//! NLE timelines come back into edits: Premiere XML round trip and .prproj reading.
use serde_json::{Value, json};
use std::{io::Write, process::Command};

fn skywrite(args: &[&str]) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn xml_clip(id: &str, file: &str, timing: (i32, i32, i32), channel: u32, extra: &str) -> String {
    let (start, end, input) = timing;
    let length = if start < 0 || end < 0 {
        60
    } else {
        end - start
    };
    let channel = if channel == 0 {
        String::new()
    } else {
        format!("<sourcetrack><trackindex>{channel}</trackindex></sourcetrack>")
    };
    format!(
        r#"<clipitem id="{id}"><start>{start}</start><end>{end}</end><in>{input}</in>
        <out>{}</out><file id="{file}"/>{channel}{extra}</clipitem>"#,
        input + length,
    )
}

fn import_xml(video: &str, audio: &[String]) -> Value {
    import_xml_with_report(video, audio).0
}

fn import_xml_with_report(video: &str, audio: &[String]) -> (Value, Value) {
    let dir = tempfile::tempdir().unwrap();
    let camera = dir.path().join("camera.mp4");
    let sound = dir.path().join("sound.wav");
    for args in [
        vec![
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
            "-ac",
            "2",
            "-shortest",
            camera.to_str().unwrap(),
        ],
        vec![
            "-f",
            "lavfi",
            "-i",
            "sine=d=6",
            "-ac",
            "2",
            sound.to_str().unwrap(),
        ],
    ] {
        let out = Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let audio: String = audio
        .iter()
        .map(|item| format!("<track>{item}</track>"))
        .collect();
    let xml = format!(
        r#"<xmeml><file id="camera"><pathurl>{}</pathurl></file>
        <file id="sound"><pathurl>{}</pathurl></file>
        <sequence><rate><timebase>30</timebase></rate><duration>180</duration>
        <media><video><format><samplecharacteristics><width>160</width><height>90</height>
        </samplecharacteristics></format><track>{video}</track></video><audio>{audio}</audio>
        </media></sequence></xmeml>"#,
        url::Url::from_file_path(&camera).unwrap(),
        url::Url::from_file_path(&sound).unwrap(),
    );
    let input = dir.path().join("timeline.xml");
    let output = dir.path().join("edit.json");
    std::fs::write(&input, xml).unwrap();
    let report = skywrite(&[
        "edit",
        "import",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ]);
    assert_eq!(report["validation"]["valid"], true, "{report}");
    (
        serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap(),
        report,
    )
}

fn audio_clips(edit: &Value) -> Vec<&Value> {
    edit["audio"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|track| track["clips"].as_array().unwrap())
        .collect()
}

#[test]
fn xmeml_linked_stereo_channels_merge() {
    let edit = import_xml(
        "",
        &[
            xml_clip(
                "left",
                "sound",
                (0, 60, 0),
                1,
                "<link><linkclipref>right</linkclipref></link>",
            ),
            xml_clip(
                "right",
                "sound",
                (0, 60, 0),
                2,
                "<link><linkclipref>left</linkclipref></link>",
            ),
        ],
    );
    assert_eq!(audio_clips(&edit).len(), 1, "{edit}");
}

#[test]
fn xmeml_distinct_video_links_keep_each_channels_mix() {
    let level = "<filter><effect><effectid>audiolevels</effectid><parameter><parameterid>level</parameterid><value>0.5</value></parameter></effect></filter>";
    let automated = "<filter><effect><effectid>audiolevels</effectid><parameter><parameterid>level</parameterid><value>0.5</value><keyframe><when>0</when><value>0.5</value></keyframe><keyframe><when>30</when><value>0.25</value></keyframe></parameter></effect></filter>";
    for second_mix in [level, automated] {
        // Put overlapping video items on separate tracks.
        let video = format!(
            "{}</track><track>{}",
            xml_clip("video-a", "camera", (0, 60, 0), 1, ""),
            xml_clip("video-b", "camera", (0, 60, 0), 1, ""),
        );
        let edit = import_xml(
            &video,
            &[
                xml_clip(
                    "left",
                    "camera",
                    (0, 60, 0),
                    1,
                    &format!("{level}<link><linkclipref>video-a</linkclipref></link>"),
                ),
                xml_clip(
                    "right",
                    "camera",
                    (0, 60, 0),
                    2,
                    &format!("{second_mix}<link><linkclipref>video-b</linkclipref></link>"),
                ),
            ],
        );
        assert!(audio_clips(&edit).is_empty(), "{edit}");
        assert_eq!(
            edit["video"][0]["clips"][0]["audio"]["volume"], 0.5,
            "{edit}"
        );
        let second_audio = &edit["video"][1]["clips"][0]["audio"];
        if second_mix == level {
            assert_eq!(second_audio["volume"], 0.5, "{edit}");
        } else {
            assert_eq!(
                second_audio["volume_keyframes"],
                json!([
                    {"time": 0.0, "volume": 0.5}, {"time": 1.0, "volume": 0.25}
                ]),
                "{edit}"
            );
        }
    }
}

#[test]
fn xmeml_video_linked_stereo_channels_merge() {
    let extra = "<link><linkclipref>video</linkclipref></link><filter><effect><effectid>audiolevels</effectid><parameter><parameterid>level</parameterid><value>0.5</value></parameter></effect></filter>";
    let edit = import_xml(
        &xml_clip("video", "camera", (0, 60, 0), 1, ""),
        &[
            xml_clip("left", "camera", (0, 60, 0), 1, extra),
            xml_clip("right", "camera", (0, 60, 0), 2, extra),
        ],
    );
    assert!(audio_clips(&edit).is_empty(), "{edit}");
    assert_eq!(
        edit["video"][0]["clips"][0]["audio"]["volume"], 0.5,
        "{edit}"
    );
}

#[test]
fn xmeml_stereo_merge_preserves_all_video_links() {
    let video = format!(
        "{}</track><track>{}</track><track>{}",
        xml_clip("video-a", "camera", (0, 60, 0), 1, ""),
        xml_clip("video-b", "camera", (0, 60, 0), 1, ""),
        xml_clip("video-c", "camera", (0, 60, 0), 1, ""),
    );
    let level = "<filter><effect><effectid>audiolevels</effectid><parameter><parameterid>level</parameterid><value>0.5</value></parameter></effect></filter>";
    let edit = import_xml(
        &video,
        &[
            xml_clip(
                "left",
                "camera",
                (0, 60, 0),
                1,
                &format!(
                    "{level}<link><linkclipref>right</linkclipref></link><link><linkclipref>video-a</linkclipref></link>"
                ),
            ),
            xml_clip(
                "right",
                "camera",
                (0, 60, 0),
                2,
                &format!(
                    "{level}<link><linkclipref>left</linkclipref></link><link><linkclipref>video-b</linkclipref></link><link><linkclipref>video-c</linkclipref></link>"
                ),
            ),
        ],
    );
    assert!(audio_clips(&edit).is_empty(), "{edit}");
    for (index, track) in edit["video"].as_array().unwrap().iter().enumerate() {
        assert_eq!(
            track["clips"][0]["audio"]["volume"].as_f64().unwrap(),
            if index == 0 { 0.5 } else { 0.0 },
            "{edit}"
        );
    }
}

#[test]
fn xmeml_channel_partners_with_distinct_mixes_use_first_mix_and_note() {
    let level = "<filter><effect><effectid>audiolevels</effectid><parameter><parameterid>level</parameterid><value>0.5</value></parameter></effect></filter>";
    let automated = "<filter><effect><effectid>audiolevels</effectid><parameter><parameterid>level</parameterid><value>0.5</value><keyframe><when>0</when><value>0.5</value></keyframe><keyframe><when>30</when><value>0.25</value></keyframe></parameter></effect></filter>";
    let effect = "<filter><effect><effectid>highpass</effectid></effect></filter>";
    for (first_mix, second_mix) in [
        ("", level),
        ("", automated),
        (level, automated),
        ("", effect),
    ] {
        for linked in [false, true] {
            let link = if linked {
                "<link><linkclipref>video</linkclipref></link>"
            } else {
                ""
            };
            let (edit, report) = import_xml_with_report(
                &xml_clip("video", "camera", (0, 60, 0), 1, ""),
                &[
                    xml_clip(
                        "left",
                        "camera",
                        (0, 60, 0),
                        1,
                        &format!("{first_mix}{link}"),
                    ),
                    xml_clip(
                        "right",
                        "camera",
                        (0, 60, 0),
                        2,
                        &format!("{second_mix}{link}"),
                    ),
                ],
            );
            let clips = audio_clips(&edit);
            assert_eq!(clips.len(), usize::from(!linked), "{edit}");
            let mix = if linked {
                &edit["video"][0]["clips"][0]["audio"]
            } else {
                assert_eq!(edit["video"][0]["clips"][0]["audio"]["volume"], 0);
                clips[0]
            };
            assert_eq!(
                mix["volume"].as_f64().unwrap_or(1.0),
                if first_mix.is_empty() { 1.0 } else { 0.5 },
                "{edit}"
            );
            assert!(mix["volume_keyframes"].is_null(), "{edit}");
            let notes = report["notes"].as_array().unwrap();
            assert_eq!(notes.len(), 1, "{report}");
            let note = notes[0].as_str().unwrap();
            assert!(note.contains("camera.mp4"), "{note}");
            assert!(
                note.contains("per-channel mixes are not representable"),
                "{note}"
            );
            assert!(note.contains("first channel's mix was used"), "{note}");
        }
    }
}

#[test]
fn xmeml_linked_sound_folds_once_into_first_exact_video() {
    for (timings, folded) in [
        ([(0, 60, 0), (0, 60, 0), (0, 60, 0)], Some(0)),
        ([(30, 90, 30), (0, 60, 0), (0, 60, 0)], Some(1)),
        ([(30, 90, 30), (0, 60, 0), (0, 90, 0)], Some(1)),
        ([(30, 90, 30), (0, 90, 0), (0, 60, 30)], None),
    ] {
        for stereo in [false, true] {
            let video = ["video-a", "video-b", "video-c"]
                .into_iter()
                .zip(timings)
                .map(|(id, timing)| xml_clip(id, "camera", timing, 1, ""))
                .collect::<Vec<_>>()
                .join("</track><track>");
            let links = "<link><linkclipref>video-a</linkclipref></link><link><linkclipref>video-b</linkclipref></link><link><linkclipref>video-c</linkclipref></link>";
            let level = "<filter><effect><effectid>audiolevels</effectid><parameter><parameterid>level</parameterid><value>0.5</value></parameter></effect></filter>";
            let mut audio = vec![xml_clip(
                "left",
                "camera",
                (0, 60, 0),
                1,
                &format!("{level}{links}"),
            )];
            if stereo {
                audio.push(xml_clip(
                    "right",
                    "camera",
                    (0, 60, 0),
                    2,
                    &format!("{level}{links}"),
                ));
            }
            let edit = import_xml(&video, &audio);
            let clips = audio_clips(&edit);
            assert_eq!(clips.len(), usize::from(folded.is_none()), "{edit}");
            if folded.is_none() {
                assert_eq!(clips[0]["volume"], 0.5, "{edit}");
            }
            for (index, track) in edit["video"].as_array().unwrap().iter().enumerate() {
                assert_eq!(
                    track["clips"][0]["audio"]["volume"].as_f64().unwrap(),
                    if folded == Some(index) { 0.5 } else { 0.0 },
                    "{edit}"
                );
            }
        }
    }
}

#[test]
fn xmeml_channel_partner_set_is_enabled_if_any_partner_is_enabled() {
    for linked in [false, true] {
        for first_enabled in [false, true] {
            let link = if linked {
                "<link><linkclipref>video</linkclipref></link>"
            } else {
                ""
            };
            let level = "<filter><effect><effectid>audiolevels</effectid><parameter><parameterid>level</parameterid><value>0.5</value></parameter></effect></filter>";
            let (edit, report) = import_xml_with_report(
                &xml_clip("video", "camera", (0, 60, 0), 1, ""),
                &[
                    xml_clip(
                        "left",
                        "camera",
                        (0, 60, 0),
                        1,
                        &format!(
                            "{level}{link}<enabled>{}</enabled>",
                            if first_enabled { "TRUE" } else { "FALSE" }
                        ),
                    ),
                    xml_clip(
                        "right",
                        "camera",
                        (0, 60, 0),
                        2,
                        &format!(
                            "{level}{link}<enabled>{}</enabled>",
                            if first_enabled { "FALSE" } else { "TRUE" }
                        ),
                    ),
                ],
            );
            let clips = audio_clips(&edit);
            assert_eq!(clips.len(), usize::from(!linked), "{edit}");
            let mix = if linked {
                &edit["video"][0]["clips"][0]["audio"]
            } else {
                assert_eq!(edit["video"][0]["clips"][0]["audio"]["volume"], 0);
                clips[0]
            };
            assert_eq!(mix["volume"], 0.5, "{edit}");
            assert_eq!(report["notes"].as_array().unwrap().len(), 1, "{report}");
        }
    }
}

#[test]
fn xmeml_audio_item_limit_is_checked_before_deduplication() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("timeline.xml");
    let limit = skywrite::timeline::MAX_AUDIO_CLIPS * 8;
    // Distinct channels would all merge into one playback without the work bound.
    let items: String = (0..=limit)
        .map(|index| {
            xml_clip(
                &format!("audio-{index}"),
                "sound",
                (0, 60, 0),
                index as u32 + 1,
                "",
            )
        })
        .collect();
    let xml = format!(
        r#"<xmeml><file id="sound"><pathurl>file:///sound.wav</pathurl></file>
        <sequence><rate><timebase>30</timebase></rate><media><audio><track>{}</track>
        </audio></media></sequence></xmeml>"#,
        items,
    );
    std::fs::write(&input, xml).unwrap();
    let error = skywrite::import::import(&input).unwrap_err().to_string();
    assert_eq!(
        error,
        format!("XML sequence may contain at most {limit} raw audio clipitems")
    );
}

#[test]
fn xmeml_video_item_work_limit_is_checked_before_import() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("timeline.xml");
    let limit = skywrite::timeline::MAX_VIDEO_CLIPS * 8;
    // Disabled items produce no clips, but still count toward the work bound.
    let item = xml_clip("video", "camera", (0, 60, 0), 1, "<enabled>FALSE</enabled>");
    let xml = format!(
        r#"<xmeml><sequence><rate><timebase>30</timebase></rate><media>
        <video><track>{}</track></video></media></sequence></xmeml>"#,
        item.repeat(limit + 1),
    );
    std::fs::write(&input, xml).unwrap();
    let error = skywrite::import::import(&input).unwrap_err().to_string();
    assert_eq!(
        error,
        format!("XML sequence may contain at most {limit} raw video clipitems")
    );
}

#[test]
fn xmeml_more_than_audio_clip_limit_raw_items_fold_into_video() {
    let mut video = String::new();
    let mut audio = [String::new(), String::new()];
    for index in 0..600 {
        // Four tracks of 150 one-frame clips fit the six-second sequence.
        if index > 0 && index % 150 == 0 {
            video.push_str("</track><track>");
        }
        let start = index % 150;
        let timing = (start, start + 1, 0);
        let video_id = format!("video-{index}");
        video.push_str(&xml_clip(&video_id, "camera", timing, 1, ""));
        for (channel, track) in audio.iter_mut().enumerate() {
            track.push_str(&xml_clip(
                &format!("audio-{index}-{channel}"),
                "camera",
                timing,
                channel as u32 + 1,
                &format!("<link><linkclipref>{video_id}</linkclipref></link>"),
            ));
        }
    }
    let edit = import_xml(&video, &audio);
    assert!(audio_clips(&edit).is_empty(), "{edit}");
    let clips: Vec<_> = edit["video"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|track| track["clips"].as_array().unwrap())
        .collect();
    assert_eq!(clips.len(), 600);
    assert!(clips.iter().all(|clip| clip["audio"].is_null()), "{edit}");
}

#[test]
fn xmeml_channel_partner_merge_keeps_repeated_channel_layers_separate() {
    for linked in [false, true] {
        let link = if linked {
            "<link><linkclipref>video</linkclipref></link>"
        } else {
            ""
        };
        let edit = import_xml(
            &xml_clip("video", "camera", (0, 60, 0), 1, ""),
            &[
                xml_clip("a", "camera", (0, 60, 0), 1, link),
                xml_clip("b", "camera", (0, 60, 0), 2, link),
                xml_clip("c", "camera", (0, 60, 0), 2, link),
            ],
        );
        assert_eq!(
            audio_clips(&edit).len(),
            if linked { 1 } else { 2 },
            "{edit}"
        );
    }
}

#[test]
fn xmeml_self_linked_only_channel_partners_merge() {
    let edit = import_xml(
        "",
        &[
            xml_clip(
                "left",
                "sound",
                (0, 60, 0),
                1,
                "<link><linkclipref>left</linkclipref></link>",
            ),
            xml_clip(
                "right",
                "sound",
                (0, 60, 0),
                2,
                "<link><linkclipref>right</linkclipref></link>",
            ),
            xml_clip(
                "layer",
                "sound",
                (0, 60, 0),
                2,
                "<link><linkclipref>layer</linkclipref></link>",
            ),
        ],
    );
    assert_eq!(audio_clips(&edit).len(), 2, "{edit}");
}

#[test]
fn xmeml_same_channel_mutually_linked_partners_merge_and_unlinked_layers_stay_separate() {
    let edit = import_xml(
        "",
        &[
            xml_clip(
                "a",
                "sound",
                (0, 60, 0),
                1,
                "<link><linkclipref>b</linkclipref></link>",
            ),
            xml_clip(
                "b",
                "sound",
                (0, 60, 0),
                1,
                "<link><linkclipref>a</linkclipref></link>",
            ),
            xml_clip(
                "c",
                "sound",
                (0, 90, 0),
                1,
                "<filter><effect><effectid>audiolevels</effectid><parameter><parameterid>level</parameterid><value>0.5</value></parameter></effect></filter>",
            ),
            xml_clip("d", "sound", (0, 60, 0), 1, ""),
        ],
    );
    let clips = audio_clips(&edit);
    assert_eq!(clips.len(), 3, "{edit}");
    assert_eq!(clips[1]["duration"], 3.0);
    assert_eq!(clips[1]["volume"], 0.5);
    assert_eq!(clips[2]["duration"], 2.0);
}

#[test]
fn xmeml_mutually_linked_audio_without_channel_metadata_merges() {
    let edit = import_xml(
        "",
        &[
            xml_clip(
                "left",
                "sound",
                (0, 60, 0),
                0,
                "<link><linkclipref>right</linkclipref></link>",
            ),
            xml_clip(
                "right",
                "sound",
                (0, 60, 0),
                0,
                "<link><linkclipref>left</linkclipref></link>",
            ),
            xml_clip("layer", "sound", (0, 60, 0), 0, ""),
        ],
    );
    assert_eq!(audio_clips(&edit).len(), 2, "{edit}");
}

#[test]
fn xmeml_channel_partners_require_equal_end_in_and_speed() {
    let edit = import_xml(
        "",
        &[
            xml_clip("a", "sound", (0, 60, 0), 1, ""),
            xml_clip("b", "sound", (0, 90, 0), 2, ""),
            xml_clip("c", "sound", (0, 60, 30), 2, ""),
            xml_clip(
                "d",
                "sound",
                (0, 60, 0),
                2,
                "<filter><effect><effectid>timeremap</effectid><parameter><parameterid>speed</parameterid><value>200</value></parameter></effect></filter>",
            ),
        ],
    );
    let clips = audio_clips(&edit);
    assert_eq!(clips.len(), 4, "{edit}");
    assert_eq!(clips[3]["speed"], 2.0);
}

#[test]
fn xmeml_audio_transition_edges_use_in_out_fallback() {
    let edit = import_xml(
        "",
        &[
            xml_clip("head", "sound", (-1, 60, 0), 1, ""),
            xml_clip("tail", "sound", (0, -1, 0), 1, ""),
        ],
    );
    let clips = audio_clips(&edit);
    assert_eq!(clips.len(), 2, "{edit}");
    for clip in clips {
        assert_eq!(clip["start"], 0.0);
        assert_eq!(clip["duration"], 2.0);
    }
}

#[test]
fn xmeml_linked_j_cut_keeps_its_timing_and_mutes_video() {
    let edit = import_xml(
        &xml_clip("video", "camera", (30, 90, 30), 1, ""),
        &[xml_clip(
            "audio",
            "camera",
            (0, 90, 0),
            1,
            "<link><linkclipref>video</linkclipref></link>",
        )],
    );
    assert_eq!(edit["video"][0]["clips"][0]["audio"]["volume"], 0);
    let clips = audio_clips(&edit);
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0]["start"], 0.0);
    assert_eq!(clips[0]["source_in"], 0.0);
    assert_eq!(clips[0]["duration"], 3.0);
}

#[test]
fn xmeml_disabled_linked_audio_mutes_video_without_level_effects() {
    let edit = import_xml(
        &xml_clip("video", "camera", (0, 60, 0), 1, ""),
        &[xml_clip(
            "audio",
            "camera",
            (0, 60, 0),
            1,
            "<enabled>FALSE</enabled><link><linkclipref>video</linkclipref></link>",
        )],
    );
    assert_eq!(edit["video"][0]["clips"][0]["audio"]["volume"], 0);
    assert!(audio_clips(&edit).is_empty());
}

#[test]
fn xmeml_linked_wav_keeps_its_source_and_mutes_camera_sound() {
    let edit = import_xml(
        &xml_clip("video", "camera", (0, 60, 0), 1, ""),
        &[xml_clip(
            "audio",
            "sound",
            (0, 60, 0),
            1,
            "<link><linkclipref>video</linkclipref></link>",
        )],
    );
    assert_eq!(edit["video"][0]["clips"][0]["audio"]["volume"], 0);
    let clips = audio_clips(&edit);
    assert_eq!(clips.len(), 1);
    assert!(clips[0]["source"].as_str().unwrap().ends_with("sound.wav"));
}

#[test]
fn xmeml_linked_audio_with_distinct_in_or_speed_stays_independent() {
    for (timing, extra, source_in, speed) in [
        ((0, 60, 30), "", 1.0, 1.0),
        (
            (0, 60, 0),
            "<filter><effect><effectid>timeremap</effectid><parameter><parameterid>speed</parameterid><value>200</value></parameter></effect></filter>",
            0.0,
            2.0,
        ),
    ] {
        let edit = import_xml(
            &xml_clip("video", "camera", (0, 60, 0), 1, ""),
            &[xml_clip(
                "audio",
                "camera",
                timing,
                1,
                &format!("{extra}<link><linkclipref>video</linkclipref></link>"),
            )],
        );
        assert_eq!(edit["video"][0]["clips"][0]["audio"]["volume"], 0);
        let clips = audio_clips(&edit);
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0]["source_in"], source_in);
        assert_eq!(clips[0]["speed"].as_f64().unwrap_or(1.0), speed);
    }
}

#[test]
fn xmeml_linked_audio_survives_disabled_or_unresolved_video() {
    for video in [
        xml_clip("video", "camera", (0, 60, 0), 1, "<enabled>FALSE</enabled>"),
        xml_clip("video", "missing", (0, 60, 0), 1, ""),
    ] {
        let edit = import_xml(
            &video,
            &[xml_clip(
                "audio",
                "camera",
                (0, 60, 0),
                1,
                "<link><linkclipref>video</linkclipref></link>",
            )],
        );
        assert!(edit["video"][0]["clips"].as_array().unwrap().is_empty());
        assert_eq!(audio_clips(&edit).len(), 1);
    }
}

#[test]
fn premiere_xml_round_trips_layers_speed_fades_and_levels() {
    let dir = tempfile::tempdir().unwrap();
    let p = |n: &str| dir.path().join(n).to_str().unwrap().to_owned();
    for (name, filter) in [
        ("a.mp4", "color=red:s=160x90:r=30:d=8"),
        ("b.mp4", "color=blue:s=160x90:r=30:d=8"),
    ] {
        assert!(
            Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    filter,
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=d=8",
                    "-c:v",
                    "libx264",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    "-shortest",
                    &p(name)
                ])
                .status()
                .unwrap()
                .success()
        );
    }
    assert!(
        Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=d=8",
                &p("tone.wav")
            ])
            .status()
            .unwrap()
            .success()
    );
    let edit = json!({"version":1,"name":"Round trip","width":160,"height":90,"fps":{"num":30,"den":1},
        "video":[{"clips":[{"start":0,"source":"a.mp4","source_in":1,"duration":2,"audio":{"volume":0.5}},
                           {"start":2,"source":"a.mp4","source_in":4,"duration":1.5,"speed":1.2}]},
                 {"clips":[{"start":0.5,"source":"b.mp4","source_in":2,"duration":1,"fade_in":0.5,"opacity":0.8,"audio":{"volume":0}}]}],
        "audio":[{"clips":[{"start":1,"source":"tone.wav","source_in":0.5,"duration":2,
            "volume_keyframes":[{"time":0,"volume":1},{"time":1,"volume":0.25}]}]}],
        "chapters":[{"start":0,"title":"Start"}]});
    std::fs::write(p("edit.json"), serde_json::to_vec(&edit).unwrap()).unwrap();
    skywrite(&[
        "edit",
        "export",
        &p("edit.json"),
        "--to",
        "premiere",
        "-o",
        &p("edit.xml"),
    ]);
    let report = skywrite(&["edit", "import", &p("edit.xml"), "-o", &p("back.json")]);
    assert_eq!(report["validation"]["valid"], true, "{report}");
    let back: Value = serde_json::from_slice(&std::fs::read(p("back.json")).unwrap()).unwrap();
    let v1 = &back["video"][0]["clips"];
    assert_eq!(v1[0]["start"], 0.0);
    assert_eq!(v1[0]["source_in"], 1.0);
    assert_eq!(v1[0]["audio"]["volume"], 0.5);
    assert!((v1[1]["speed"].as_f64().unwrap() - 1.2).abs() < 1e-9);
    assert!((v1[1]["source_in"].as_f64().unwrap() - 4.0).abs() < 0.05);
    assert!(v1[1]["audio"].is_null(), "{back}");
    let v2 = &back["video"][1]["clips"][0];
    assert_eq!(v2["fade_in"], 0.5);
    assert_eq!(v2["opacity"], 0.8);
    assert_eq!(v2["audio"]["volume"], 0);
    let clips = audio_clips(&back);
    assert_eq!(clips.len(), 1, "{back}");
    let tone = clips[0];
    assert!(tone["source"].as_str().unwrap().ends_with("tone.wav"));
    assert_eq!(tone["start"], 1.0);
    assert_eq!(tone["volume_keyframes"][1]["volume"], 0.25);
    assert_eq!(back["chapters"][0]["title"], "Start");
    assert!(
        back["video"][0]["clips"][0]["source"]
            .as_str()
            .unwrap()
            .ends_with("a.mp4")
    );
}

#[test]
fn premiere_projects_are_read_experimentally_and_bad_input_fails() {
    let dir = tempfile::tempdir().unwrap();
    let ticks = |seconds: f64| (seconds * 254_016_000_000.0) as u64;
    // Structure as described by a tester's working parser (object graph by IDs).
    let project = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<PremiereData Version="3">
  <VideoClipTrack ObjectID="10"><ClipTrack><ClipItems><TrackItems><TrackItem Index="0" ObjectRef="20"/></TrackItems></ClipItems></ClipTrack></VideoClipTrack>
  <VideoClipTrackItem ObjectID="20"><ClipTrackItem><TrackItem><Start>{}</Start><End>{}</End></TrackItem><SubClip ObjectRef="30"/></ClipTrackItem></VideoClipTrackItem>
  <SubClip ObjectID="30"><Clip ObjectRef="40"/><MasterClip ObjectURef="m-1"/></SubClip>
  <VideoClip ObjectID="40"><Clip><Source ObjectRef="50"/><InPoint>{}</InPoint><OutPoint>{}</OutPoint></Clip></VideoClip>
  <VideoMediaSource ObjectID="50"><MediaSource><Media ObjectURef="media-1"/></MediaSource></VideoMediaSource>
  <MasterClip ObjectUID="m-1"><Name>camera</Name></MasterClip>
  <Media ObjectUID="media-1"><ActualMediaFilePath>/footage/camera &amp; co.mxf</ActualMediaFilePath></Media>
</PremiereData>"#,
        ticks(1.0),
        ticks(3.0),
        ticks(2.0),
        ticks(4.0)
    );
    let path = dir.path().join("cut.prproj");
    let mut gz = flate2::write::GzEncoder::new(
        std::fs::File::create(&path).unwrap(),
        flate2::Compression::default(),
    );
    gz.write_all(project.as_bytes()).unwrap();
    gz.finish().unwrap();
    let out = dir.path().join("cut.json");
    let report = skywrite(&[
        "edit",
        "import",
        path.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(
        report["notes"][0]
            .as_str()
            .unwrap()
            .starts_with("Experimental")
    );
    assert_eq!(report["validation"]["valid"], false); // media is on another machine
    let edit: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    let clip = &edit["video"][0]["clips"][0];
    assert_eq!(clip["source"], "/footage/camera & co.mxf");
    assert_eq!(clip["start"], 1.0);
    assert_eq!(clip["duration"], 2.0);
    assert_eq!(clip["source_in"], 2.0);

    std::fs::write(dir.path().join("x.xml"), "<fcpxml version=\"1.10\"/>").unwrap();
    let rejected = Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .args([
            "edit",
            "import",
            dir.path().join("x.xml").to_str().unwrap(),
            "-o",
            dir.path().join("y.json").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
}
