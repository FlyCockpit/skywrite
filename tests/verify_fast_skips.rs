//! Checks scripts/verify-fast.skip, the slow tests the fast hand-back check
//! leaves out. The full `cargo test --locked` still runs all of them; these
//! checks keep the list from silently dropping protected or renamed tests.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// Test files whose every test guards safety (teardown, trust, input
/// preservation, demuxer and generated-media checks): never skipped.
const PROTECTED_TARGETS: &[&str] = &[
    "process_lifecycle",
    "signal_cleanup",
    "source_trust",
    "input_preservation",
    "demuxer_allowlist",
    "generated_media_validation",
];

/// A skipped test's name may not contain a word starting with one of these:
/// security, trust, teardown/cancellation, data preservation, bounds/budgets,
/// caches and paid-request guards always run in the fast tier.
const PROTECTED_WORDS: &[&str] = &[
    "allowlist",
    "atomic",
    "authorit",
    "bound",
    "budget",
    "cache",
    "cancel",
    "certificat",
    "clean",
    "clobber",
    "confin",
    "credential",
    "durable",
    "excess",
    "fingerprint",
    "hostile",
    "http",
    "kill",
    "leak",
    "limit",
    "lock",
    "outside",
    "overwrit",
    "paid",
    "preserv",
    "private",
    "protect",
    "reap",
    "recovery",
    "refus",
    "reject",
    "replay",
    "request",
    "secret",
    "signal",
    "stale",
    "symlink",
    "teardown",
    "timeout",
    "tls",
    "trust",
    "unsafe",
];

struct Entry {
    target: String,
    name: String,
}

fn parse(text: &str) -> Vec<Entry> {
    text.lines()
        .filter_map(|line| {
            let line = line.split('#').next().unwrap();
            let fields: Vec<&str> = line.split_whitespace().collect();
            match fields.as_slice() {
                [] => None,
                [target, name] => Some(Entry {
                    target: (*target).to_owned(),
                    name: (*name).to_owned(),
                }),
                _ => panic!("malformed skip line: {line:?}"),
            }
        })
        .collect()
}

fn entries() -> Vec<Entry> {
    parse(&fs::read_to_string(Path::new(ROOT).join("scripts/verify-fast.skip")).unwrap())
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// `source` with comments, string and char literals blanked to spaces
/// (newlines kept), so braces can be counted.
fn code_only(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out: Vec<char> = chars.clone();
    let blank = |out: &mut Vec<char>, from: usize, to: usize| {
        for c in &mut out[from..to] {
            if *c != '\n' {
                *c = ' ';
            }
        }
    };
    let mut i = 0;
    while i < chars.len() {
        let rest = &chars[i..];
        if rest.starts_with(&['/', '/']) {
            let end = (i..chars.len())
                .find(|&j| chars[j] == '\n')
                .unwrap_or(chars.len());
            blank(&mut out, i, end);
            i = end;
        } else if rest.starts_with(&['/', '*']) {
            let end = (i + 2..chars.len())
                .find(|&j| chars[j - 1] == '*' && chars[j] == '/')
                .map_or(chars.len(), |j| j + 1);
            blank(&mut out, i, end);
            i = end;
        } else if let Some(hashes) = raw_string_hashes(&chars, i) {
            // r"..." or r#"..."#: ends at a quote followed by as many `#`.
            let closing: Vec<char> = std::iter::once('"')
                .chain(std::iter::repeat_n('#', hashes))
                .collect();
            let body = i + 2 + hashes;
            let end = (body..chars.len())
                .find(|&j| chars[j..].starts_with(&closing))
                .map_or(chars.len(), |j| j + closing.len());
            blank(&mut out, i, end);
            i = end;
        } else if chars[i] == '"' {
            let mut j = i + 1;
            while j < chars.len() && chars[j] != '"' {
                j += if chars[j] == '\\' { 2 } else { 1 };
            }
            let end = (j + 1).min(chars.len());
            blank(&mut out, i, end);
            i = end;
        } else if chars[i] == '\'' {
            // A char literal ('x', '\n', '\u{..}'), not a lifetime ('a).
            let end = if rest.get(1) == Some(&'\\') {
                (i + 2..chars.len())
                    .find(|&j| chars[j] == '\'')
                    .map(|j| j + 1)
            } else if rest.get(2) == Some(&'\'') {
                Some(i + 3)
            } else {
                None
            };
            match end {
                Some(end) => {
                    blank(&mut out, i, end);
                    i = end;
                }
                None => i += 1,
            }
        } else {
            i += 1;
        }
    }
    out.into_iter().collect()
}

/// The number of `#` when a raw string literal starts at `i`.
fn raw_string_hashes(chars: &[char], i: usize) -> Option<usize> {
    let word_start =
        |at: usize| at == 0 || !(chars[at - 1].is_alphanumeric() || chars[at - 1] == '_');
    // `r"..."`, or `br"..."` for raw byte strings.
    let starts_word = word_start(i) || (i > 0 && chars[i - 1] == 'b' && word_start(i - 1));
    if chars[i] != 'r' || !starts_word {
        return None;
    }
    let hashes = chars[i + 1..].iter().take_while(|&&c| c == '#').count();
    (chars.get(i + 1 + hashes) == Some(&'"')).then_some(hashes)
}

/// Lines of `code` (see `code_only`) with their brace depth at line start.
fn depths(code: &str) -> Vec<(usize, &str)> {
    let mut depth = 0usize;
    code.lines()
        .map(|line| {
            let at = depth;
            for c in line.chars() {
                match c {
                    '{' => depth += 1,
                    '}' => depth = depth.checked_sub(1).expect("unbalanced braces"),
                    _ => {}
                }
            }
            (at, line)
        })
        .collect()
}

/// Whether the item on `lines[index]` carries `#[test]`: only its own
/// attributes count, the run of `#[...]` (and blank) lines right above it.
fn is_test(lines: &[(usize, &str)], index: usize) -> bool {
    lines[..index]
        .iter()
        .rev()
        .map(|(_, line)| line.trim())
        .take_while(|line| line.is_empty() || (line.starts_with("#[") && line.ends_with(']')))
        .any(|line| line == "#[test]")
}

/// `item` without a leading `pub`, `pub(crate)`, `pub(super)` or `pub(in …)`.
fn without_visibility(item: &str) -> &str {
    let Some(rest) = item.strip_prefix("pub") else {
        return item;
    };
    let rest = match rest.strip_prefix('(') {
        Some(scoped) => match scoped.find(')') {
            Some(close) => &scoped[close + 1..],
            None => return item,
        },
        None => rest,
    };
    if rest.starts_with(char::is_whitespace) {
        rest.trim_start()
    } else {
        item
    }
}

/// The text between the `{` at byte `open` of `code` and its matching `}`.
fn braced(code: &str, open: usize) -> &str {
    let mut depth = 0usize;
    for (at, c) in code[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &code[open + 1..open + at];
                }
            }
            _ => {}
        }
    }
    panic!("unclosed brace");
}

/// The `#[path = "…"]` value among the attributes just above line `index`,
/// read from `source` (the lines as written: `code` blanks string literals).
fn path_attribute<'a>(
    lines: &[(usize, &str)],
    source: &[&'a str],
    index: usize,
) -> Option<&'a str> {
    (0..index)
        .rev()
        .map(|at| (at, lines[at].1.trim()))
        .take_while(|(_, line)| line.is_empty() || (line.starts_with("#[") && line.ends_with(']')))
        .find(|(_, line)| line.starts_with("#[path"))
        .map(|(at, _)| {
            path_value(source[at])
                .unwrap_or_else(|| panic!("cannot read the path of {:?}", source[at]))
        })
}

/// The string in `#[path = "…"]` at the start of `line`, skipping spaces and
/// `/* … */` comments around the `=` (paths hold no `"` or `\`).
fn path_value(line: &str) -> Option<&str> {
    fn skip(mut rest: &str) -> Option<&str> {
        loop {
            rest = rest.trim_start();
            match rest.strip_prefix("/*") {
                Some(comment) => rest = &comment[comment.find("*/")? + 2..],
                None => return Some(rest),
            }
        }
    }
    let rest = skip(line.trim_start().strip_prefix("#[path")?)?;
    let rest = skip(rest.strip_prefix('=')?)?.strip_prefix('"')?;
    Some(&rest[..rest.find('"')?])
}

/// Adds the libtest names (`module::inner::test`) of the `#[test]` fns in
/// module `code` and its submodules to `out`. `dir` holds its `mod x;` files,
/// following Rust's rules (`dir/x.rs` or `dir/x/mod.rs`; children in `dir/x/`).
/// `source` is `code` as written, line for line. At the top level of a file,
/// `file_dir` is that file's directory, which a `#[path = "…"]` module is
/// relative to (its own `mod x;` files are then beside it).
fn qualified_tests(
    code: &str,
    source: &[&str],
    prefix: &str,
    dir: &Path,
    file_dir: Option<&Path>,
    out: &mut Vec<String>,
) {
    let lines = depths(code);
    let mut offset = 0;
    for (index, (depth, line)) in lines.iter().enumerate() {
        let start = offset;
        offset += line.len() + 1;
        if *depth != 0 {
            continue;
        }
        let item = without_visibility(line.trim_start());
        if let Some(rest) = item.strip_prefix("fn ") {
            if is_test(&lines, index) {
                let name = rest.split(['(', '<']).next().unwrap().trim();
                out.push(format!("{prefix}{name}"));
            }
            continue;
        }
        let Some(rest) = item.strip_prefix("mod ") else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let inner = format!("{prefix}{name}::");
        let after = rest[name.len()..].trim_start();
        if after.starts_with(';') {
            let (file, children) = match path_attribute(&lines, source, index) {
                Some(path) => {
                    let base = file_dir.unwrap_or_else(|| {
                        panic!("#[path] on mod {name} inside an inline module is not supported")
                    });
                    let file = base.join(path);
                    let children = file.parent().unwrap().to_owned();
                    (file, children)
                }
                None => (
                    [
                        dir.join(format!("{name}.rs")),
                        dir.join(&name).join("mod.rs"),
                    ]
                    .into_iter()
                    .find(|file| file.is_file())
                    .unwrap_or_else(|| panic!("no file for mod {name} in {}", dir.display())),
                    dir.join(&name),
                ),
            };
            module_file_tests(&file, &inner, &children, out);
        } else if after.starts_with('{') {
            let body = braced(code, start + line.find('{').unwrap());
            qualified_tests(body, &source[index..], &inner, &dir.join(&name), None, out);
        }
    }
}

/// Adds the tests of the module in `file` (children in `dir`) to `out`.
fn module_file_tests(file: &Path, prefix: &str, dir: &Path, out: &mut Vec<String>) {
    let source = fs::read_to_string(file).unwrap();
    let code = code_only(&source);
    let lines: Vec<&str> = source.lines().collect();
    qualified_tests(&code, &lines, prefix, dir, file.parent(), out);
}

/// The libtest names of a crate root's tests.
fn crate_tests(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    module_file_tests(root, "", root.parent().unwrap(), &mut out);
    out
}

fn protected_word(name: &str) -> Option<&'static str> {
    name.split("::")
        .flat_map(|part| part.split('_'))
        .find_map(|word| {
            PROTECTED_WORDS
                .iter()
                .copied()
                .find(|prefix| word.starts_with(prefix))
        })
}

#[test]
fn every_skipped_test_exists_once_and_is_not_protected() {
    let entries = entries();
    assert!(!entries.is_empty());
    // Every test binary: the library's unit tests, the CLI's, each tests/ file.
    let src = Path::new(ROOT).join("src");
    let mut crates = vec![
        ("lib".to_owned(), crate_tests(&src.join("lib.rs"))),
        ("bin".to_owned(), crate_tests(&src.join("main.rs"))),
    ];
    let mut files = Vec::new();
    rust_files(&Path::new(ROOT).join("tests"), &mut files);
    for file in files {
        let stem = file.file_stem().unwrap().to_str().unwrap().to_owned();
        crates.push((stem, crate_tests(&file)));
    }
    let mut seen = std::collections::HashSet::new();
    for Entry { target, name } in &entries {
        assert!(seen.insert(name.clone()), "{name} is listed twice");
        assert!(
            !PROTECTED_TARGETS.contains(&target.as_str()),
            "{target} tests are protected and always run in the fast tier"
        );
        if let Some(word) = protected_word(name) {
            panic!("{name} looks protected (`{word}`): it must run in the fast tier");
        }
        assert!(
            target != "bin" && crates.iter().any(|(crate_name, _)| crate_name == target),
            "{target}: not `lib` or a tests/ file"
        );
        // `--exact --skip NAME` applies to every test binary, so the name must
        // be exactly one test, in the target it is listed under.
        for (crate_name, tests) in &crates {
            let count = tests.iter().filter(|test| *test == name).count();
            let expected = usize::from(crate_name == target);
            assert_eq!(count, expected, "{name} in {crate_name}");
        }
    }
}

#[test]
fn verify_fast_skips_exactly_the_listed_names_and_selects_every_target() {
    let output = Command::new("bash")
        .arg(Path::new(ROOT).join("scripts/verify-fast"))
        .arg("--print-test-args")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let mut expected = "--exact\n".to_owned();
    for entry in entries() {
        expected.push_str(&format!("--skip\n{}\n", entry.name));
    }
    assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);

    // No target selection: new test files and unit tests run by default.
    let script = fs::read_to_string(Path::new(ROOT).join("scripts/verify-fast")).unwrap();
    let test_lines: Vec<&str> = script
        .lines()
        .filter(|line| line.trim_start().starts_with("cargo test"))
        .collect();
    assert_eq!(
        test_lines,
        ["cargo test --locked -- \"${test_args[@]}\""],
        "verify-fast must run all targets with only the listed skips"
    );
}

#[test]
fn verify_fast_parses_comments_and_rejects_malformed_lines() {
    let dir = tempfile::tempdir().unwrap();
    let scripts = dir.path().join("scripts");
    fs::create_dir_all(&scripts).unwrap();
    fs::copy(
        Path::new(ROOT).join("scripts/verify-fast"),
        scripts.join("verify-fast"),
    )
    .unwrap();
    let run = |skip: &str| {
        fs::write(scripts.join("verify-fast.skip"), skip).unwrap();
        Command::new("bash")
            .arg(scripts.join("verify-fast"))
            .arg("--print-test-args")
            .output()
            .unwrap()
    };

    let ok = run("# header\n\n  lib  a::tests::slow  # why\nmedia b_slow");
    assert!(ok.status.success(), "{ok:?}");
    assert_eq!(
        String::from_utf8(ok.stdout).unwrap(),
        "--exact\n--skip\na::tests::slow\n--skip\nb_slow\n"
    );
    // Windows line endings must not leave `\r` on the names.
    let crlf = run("# header\r\nlib a::tests::slow\r\n\r\nmedia b_slow # why\r\n");
    assert!(crlf.status.success(), "{crlf:?}");
    assert_eq!(
        String::from_utf8(crlf.stdout).unwrap(),
        "--exact\n--skip\na::tests::slow\n--skip\nb_slow\n"
    );
    for bad in ["media", "media one two", "media one two\r\n"] {
        let output = run(bad);
        assert_eq!(output.status.code(), Some(2), "{bad:?}: {output:?}");
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn test_names_follow_the_module_tree_and_own_attributes() {
    let lib = crate_tests(&Path::new(ROOT).join("src/lib.rs"));
    let has = |name: &str| lib.iter().any(|test| test == name);
    assert!(has(
        "audio_sync::integration_tests::audio_sync_ffmpeg_offset_and_one_percent_drift"
    ));
    // Right function, wrong module: libtest would not match it.
    assert!(!has(
        "audio_sync::tests::audio_sync_ffmpeg_offset_and_one_percent_drift"
    ));
    assert!(!has("audio_sync_ffmpeg_offset_and_one_percent_drift"));
    // `mod tests;` in src/project.rs lives in src/project/tests.rs.
    assert!(has("project::tests::long_speech_node_is_one_combined_job"));

    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("a")).unwrap();
    fs::write(
        dir.path().join("lib.rs"),
        "mod a;\n#[test]\nfn real() {}\nfn helper() {}\n#[test]\npub fn public() {}\nfn public_helper() {}\n#[cfg(test)]\nmod t {\n    #[test]\n    #[ignore]\n    fn inner() { let s = \"}\"; }\n}\npub(crate) mod v {\n    #[test]\n    pub(super) fn scoped() {}\n}\n",
    )
    .unwrap();
    fs::write(dir.path().join("a.rs"), "pub mod b;\n").unwrap();
    fs::write(dir.path().join("a/b.rs"), "#[test]\nfn deep() {}\n").unwrap();
    assert_eq!(
        crate_tests(&dir.path().join("lib.rs")),
        ["a::b::deep", "real", "public", "t::inner", "v::scoped"]
    );

    // A `#[path]` module is read from that file (relative to the declaring
    // file's directory); its own `mod x;` files sit beside it.
    fs::create_dir_all(dir.path().join("crate/other")).unwrap();
    fs::write(
        dir.path().join("crate/root.rs"),
        "#[path = \"other/x.rs\"] // not \"here\"\n#[cfg(test)]\nmod renamed;\n#[test]\nfn top() {}\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("crate/other/x.rs"),
        "mod y;\n#[test]\nfn in_x() {}\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("crate/other/y.rs"),
        "#[test]\nfn in_y() {}\n",
    )
    .unwrap();
    assert_eq!(
        crate_tests(&dir.path().join("crate/root.rs")),
        ["renamed::y::in_y", "renamed::in_x", "top"]
    );
    // tests/build_cxx.rs loads the shared build_cxx.rs this way.
    let build_cxx = crate_tests(&Path::new(ROOT).join("tests/build_cxx.rs"));
    assert!(
        build_cxx
            .iter()
            .any(|test| test == "target_specific_compiler_wins_and_wrappers_are_dropped")
    );
}

#[test]
fn path_values_skip_comments_around_the_equals_sign() {
    assert_eq!(path_value(r#"#[path = "a/b.rs"]"#), Some("a/b.rs"));
    assert_eq!(
        path_value(r#"  #[path /* "decoy.rs" */ = /* "x" */ "real.rs"] // "not.rs""#),
        Some("real.rs")
    );
    assert_eq!(path_value(r#"#[path="tight.rs"]"#), Some("tight.rs"));
    assert_eq!(path_value(r#"#[path = /* open"#), None);
}

#[test]
fn visibility_is_stripped_only_as_a_whole_word() {
    assert_eq!(without_visibility("pub fn a()"), "fn a()");
    assert_eq!(without_visibility("pub(crate) mod b {"), "mod b {");
    assert_eq!(without_visibility("pub(in crate::x) fn c()"), "fn c()");
    assert_eq!(without_visibility("public_fn()"), "public_fn()");
    assert_eq!(without_visibility("fn d()"), "fn d()");
}

#[test]
fn code_only_blanks_braces_in_strings_chars_and_comments() {
    let code = code_only(
        "fn a() { let s = \"{\"; let c = '}'; let r = r#\"}\"#; let b = br#\"{\"#; } // {\nfn b<'x>() {}",
    );
    assert_eq!(
        depths(&code).iter().map(|(d, _)| *d).collect::<Vec<_>>(),
        [0, 0]
    );
    assert_eq!(code.matches('{').count(), 2);
    assert_eq!(code.matches('}').count(), 2);
}

#[test]
fn protected_words_match_whole_word_prefixes() {
    assert_eq!(
        protected_word("cancellation_reaps_real_ffmpeg"),
        Some("cancel")
    );
    assert_eq!(
        protected_word("analysis::cache_tests::reuse"),
        Some("cache")
    );
    // `clock` is not `lock`, `recovers` is not `recovery`.
    assert_eq!(protected_word("audio_lands_at_clock_drift_speeds"), None);
    assert_eq!(protected_word("ducking_recovers"), None);
}
