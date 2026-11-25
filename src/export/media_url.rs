//! Media URLs written into NLE interchange files (FCPXML and Premiere XML).
//!
//! By default each source keeps its local absolute path. With a media root,
//! every source is rewritten to the editor's folder while keeping its layout
//! relative to the media base: the deepest folder holding the edit file and
//! every source it uses. An export made in the cloud then links on open once
//! the editor copies that folder's contents to the media root.
use crate::timeline::ValidatedEdit;
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Component, Path, PathBuf},
};

/// An absolute folder on the editing machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MediaRoot {
    /// A Windows drive letter such as `D`, or `None` for a POSIX path.
    drive: Option<char>,
    segments: Vec<String>,
}

impl MediaRoot {
    pub(super) fn parse(text: &str) -> Result<Self> {
        let bytes = text.as_bytes();
        let (drive, rest) = if bytes.len() >= 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
        {
            let rest = &text[2..];
            ensure!(
                rest.is_empty() || rest.starts_with(['/', '\\']),
                "--media-root {text:?} must be an absolute folder such as D:\\Projects\\film"
            );
            (Some(bytes[0].to_ascii_uppercase() as char), rest)
        } else if text.starts_with('/') {
            (None, text)
        } else {
            bail!(
                "--media-root {text:?} must be an absolute folder on the editing machine, such as /Users/me/Projects/film or D:\\Projects\\film"
            )
        };
        let separators: &[char] = if drive.is_some() {
            &['/', '\\']
        } else {
            &['/']
        };
        let segments: Vec<String> = rest
            .split(separators)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        for segment in &segments {
            ensure!(
                segment != "." && segment != ".." && !segment.chars().any(char::is_control),
                "--media-root {text:?} must not contain '.', '..' or control characters"
            );
        }
        Ok(Self { drive, segments })
    }
}

/// URL style: Premiere writes `file://localhost/` and encodes a drive colon.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Style {
    Premiere,
    Fcpxml,
}

pub(super) struct MediaUrls {
    urls: HashMap<PathBuf, (String, String)>,
    /// The local folder mirrored at the media root, when one is given.
    pub(super) base: Option<PathBuf>,
}

impl MediaUrls {
    /// The URLs of every source, and warnings about relinking them.
    pub(super) fn new(
        edit: &ValidatedEdit,
        root: Option<&MediaRoot>,
    ) -> Result<(Self, Vec<String>)> {
        let mut sources: Vec<&Path> = edit
            .video_clips()
            .map(|c| c.source.as_path())
            .chain(edit.audio_clips().map(|c| c.source.as_path()))
            .collect();
        sources.sort();
        sources.dedup();
        let mut warnings = Vec::new();
        // Premiere's Locate relinks the remaining files by name, so names must
        // be unique (case-insensitively, for macOS and Windows volumes).
        let mut names = BTreeMap::<String, Vec<&Path>>::new();
        for source in &sources {
            if let Some(name) = source.file_name().and_then(|n| n.to_str()) {
                names.entry(name.to_lowercase()).or_default().push(source);
            }
        }
        for paths in names.values().filter(|paths| paths.len() > 1) {
            warnings.push(format!(
                "several sources share the file name {}: {}. After one relink, the NLE may locate the wrong file for the others by name; rename them so every file name is unique",
                paths[0].file_name().unwrap_or_default().to_string_lossy(),
                paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let base = root.map(|_| common_base(&edit.base_dir, &sources));
        if let Some(base) = &base {
            ensure!(
                base.has_root(),
                "--media-root needs every source and the edit on one drive; move the media under one project folder"
            );
            if base.parent().is_none() {
                warnings.push(format!(
                    "media_base is the filesystem root {} because some sources are outside the edit's folder tree; each URL mirrors its full path beneath the media root. Move the media under one project folder so the editor copies only that folder",
                    base.display()
                ));
            }
        }
        let mut urls = HashMap::new();
        for source in sources {
            let pair = match (root, &base) {
                (Some(root), Some(base)) => {
                    let relative = source.strip_prefix(base).with_context(|| {
                        format!("{} is outside {}", source.display(), base.display())
                    })?;
                    let mut segments = root.segments.clone();
                    for component in relative.components() {
                        match component {
                            Component::Normal(part) => segments.push(
                                part.to_str()
                                    .context("source paths must be valid UTF-8 for interchange")?
                                    .to_owned(),
                            ),
                            _ => bail!("unexpected component in {}", relative.display()),
                        }
                    }
                    (
                        segments_url(root.drive, &segments, Style::Premiere),
                        segments_url(root.drive, &segments, Style::Fcpxml),
                    )
                }
                _ => {
                    let url = url::Url::from_file_path(source).map_err(|_| {
                        anyhow::anyhow!("cannot make file URL for {}", source.display())
                    })?;
                    (
                        url.as_str().replacen("file://", "file://localhost", 1),
                        url.as_str().to_owned(),
                    )
                }
            };
            urls.insert(source.to_owned(), pair);
        }
        Ok((Self { urls, base }, warnings))
    }

    pub(super) fn get(&self, source: &Path, style: Style) -> &str {
        let (premiere, fcpxml) = &self.urls[source];
        match style {
            Style::Premiere => premiere,
            Style::Fcpxml => fcpxml,
        }
    }
}

/// The deepest folder containing the edit's folder and every source.
fn common_base(edit_dir: &Path, sources: &[&Path]) -> PathBuf {
    let mut base: Vec<Component<'_>> = edit_dir.components().collect();
    for source in sources {
        let parent = source.parent().unwrap_or(source);
        let shared = base
            .iter()
            .zip(parent.components())
            .take_while(|(a, b)| *a == b)
            .count();
        base.truncate(shared);
    }
    base.iter().collect()
}

fn segments_url(drive: Option<char>, segments: &[String], style: Style) -> String {
    let mut url = url::Url::parse("file:///").expect("static URL");
    {
        let mut path = url.path_segments_mut().expect("file URLs have paths");
        path.clear();
        if let Some(drive) = drive {
            path.push(&format!("{drive}:"));
        }
        path.extend(segments);
    }
    let text = url.as_str().to_owned();
    match style {
        Style::Fcpxml => text,
        Style::Premiere => {
            let text = text.replacen("file:///", "file://localhost/", 1);
            match drive {
                // Premiere writes drive letters as `D%3a`.
                Some(drive) => text.replacen(
                    &format!("file://localhost/{drive}:"),
                    &format!("file://localhost/{drive}%3a"),
                    1,
                ),
                None => text,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_parse_posix_and_windows_folders() {
        let root = MediaRoot::parse("/Users/me/My Film/").unwrap();
        assert_eq!(root.drive, None);
        assert_eq!(root.segments, ["Users", "me", "My Film"]);
        let root = MediaRoot::parse("d:\\Projects\\film").unwrap();
        assert_eq!(root.drive, Some('D'));
        assert_eq!(root.segments, ["Projects", "film"]);
        assert_eq!(
            MediaRoot::parse("/").unwrap().segments,
            Vec::<String>::new()
        );
        for bad in ["media", "./media", "/a/../b", "D:media", "", "~/film"] {
            assert!(MediaRoot::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn urls_keep_layout_and_encode_like_each_nle() {
        let segments = ["Users", "me", "My Film", "media", "a#1.wav"].map(String::from);
        assert_eq!(
            segments_url(None, &segments, Style::Premiere),
            "file://localhost/Users/me/My%20Film/media/a%231.wav"
        );
        assert_eq!(
            segments_url(None, &segments, Style::Fcpxml),
            "file:///Users/me/My%20Film/media/a%231.wav"
        );
        let segments = ["Projects", "film", "bed.mp3"].map(String::from);
        assert_eq!(
            segments_url(Some('D'), &segments, Style::Premiere),
            "file://localhost/D%3a/Projects/film/bed.mp3"
        );
        assert_eq!(
            segments_url(Some('D'), &segments, Style::Fcpxml),
            "file:///D:/Projects/film/bed.mp3"
        );
    }

    #[test]
    fn base_is_the_deepest_shared_folder() {
        let edit = Path::new("/w/project/edits");
        assert_eq!(
            common_base(edit, &[Path::new("/w/project/edits/a.mp4")]),
            Path::new("/w/project/edits")
        );
        assert_eq!(
            common_base(
                edit,
                &[
                    Path::new("/w/project/media/a.mp4"),
                    Path::new("/w/project/edits/sub/b.wav")
                ]
            ),
            Path::new("/w/project")
        );
    }
}
