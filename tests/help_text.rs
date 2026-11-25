//! `--help` must carry what an agent with no other docs needs for the measured checks.
use std::process::Command;

fn help(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .args(args)
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?}");
    String::from_utf8(out.stdout).unwrap()
}

fn assert_has(text: &str, facts: &[&str]) {
    for fact in facts {
        assert!(text.contains(fact), "missing {fact:?} in:\n{text}");
    }
}

#[test]
fn top_level_help_says_to_measure_and_names_the_checks() {
    assert_has(
        &help(&[]),
        &[
            "You can't watch or hear the result: run the measured checks, quote the numbers, and say what was and wasn't measured.",
            "Before rendering, skywrite review lint",
            "after rendering, skywrite verify OUT.mp4 --ends-together --loudness --black --frozen",
        ],
    );
}

#[test]
fn review_help_says_lint_needs_transcripts() {
    assert_has(
        &help(&["review"]),
        &["pass --transcript WORDS.json for each dialogue source, or the word checks"],
    );
}

#[test]
fn lint_help_explains_endings_dead_air_and_their_flags() {
    assert_has(
        &help(&["review", "lint"]),
        &[
            "before skywrite edit render",
            "skywrite verify",
            "unfinished_ending",
            "no_sentence_final_punctuation",
            "next_word_continues",
            "open_list",
            "dead_air",
            "--dead-air-seconds <SECONDS>",
            "[default: 0.6]",
            "\"pauses\"",
            "excluded_by_pause",
            "J/L cuts",
            "Exit status is 0 with warnings",
            "not_checked",
            "Examples:",
            "skywrite review lint edit.json --transcript talk.transcript.json",
        ],
    );
}

#[test]
fn verify_help_gives_units_defaults_and_examples() {
    assert_has(
        &help(&["verify"]),
        &[
            "after skywrite edit render",
            "not_measured",
            "--ends-together",
            "--end-tolerance <SECONDS>",
            "[default: 0.05]",
            "--loudness",
            "--target-lufs <LUFS>",
            "[default: -16]",
            "--lufs-tolerance <LU>",
            "--max-true-peak <DBTP>",
            "[default: -1]",
            "EBU R128",
            "--black",
            "first_frame_black",
            "--black-min-seconds <SECONDS>",
            "[default: 0.5]",
            "blackdetect",
            "--frozen",
            "--frozen-min-seconds <SECONDS>",
            "[default: 2]",
            "freezedetect",
            "exits 1 if any requested check fails",
            "Examples:",
            "skywrite verify out.mp4 --ends-together --loudness --black --frozen --edit edit.json",
            "--edit <EDIT.json>",
            "--edit-tolerance <SECONDS>",
            "[default: 0.1]",
            "allowed: fade_in",
            "allowed: fade_out",
            "allowed: still",
            "-14 for music streaming",
            "-23 for EBU broadcast",
        ],
    );
}

#[test]
fn audio_finishing_help_states_the_mono_upmix() {
    for command in ["normalize", "tempo", "loop", "duck", "finish"] {
        assert_has(
            &help(&["audio", command]),
            &[
                "a mono input is converted to stereo at",
                "-3 dB per channel (equal power)",
            ],
        );
    }
}
