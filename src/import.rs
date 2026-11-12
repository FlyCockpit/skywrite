//! Bring an NLE timeline back into an edit.
//!
//! Final Cut Pro 7 XML (xmeml, what Premiere exports with File > Export >
//! Final Cut Pro XML) is the supported path. Premiere `.prproj` files are read
//! experimentally: they are undocumented, versioned, gzip-compressed object
//! graphs, so results are labelled and anything unrecognised is reported.
use anyhow::{Context, Result, bail, ensure};
use quick_xml::{XmlVersion, events::Event};
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
};

use crate::timeline::{MAX_AUDIO_CLIPS, MAX_VIDEO_CLIPS};

const MAX_INPUT: u64 = 256 * 1024 * 1024;
const MAX_NODES: usize = 4_000_000;
/// Element nesting bound; real xmeml and prproj documents stay far below it.
const MAX_DEPTH: usize = 1024;
/// Bound import work while allowing channel partners and folded camera sound.
/// Validation applies the smaller clip limits to the resulting edit instead.
const MAX_RAW_AUDIO_ITEMS: usize = MAX_AUDIO_CLIPS * 8;
const MAX_RAW_VIDEO_ITEMS: usize = MAX_VIDEO_CLIPS * 8;
/// Premiere project time units per second.
const PRPROJ_TICKS: f64 = 254_016_000_000.0;

#[derive(Debug, Default)]
pub struct Node {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
    pub text: String,
}
impl Node {
    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
    fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }
    fn children<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }
    fn path(&self, path: &[&str]) -> Option<&Node> {
        path.iter().try_fold(self, |node, name| node.child(name))
    }
    fn text_of(&self, path: &[&str]) -> Option<&str> {
        self.path(path).map(|n| n.text.trim())
    }
    fn number(&self, path: &[&str]) -> Option<f64> {
        self.text_of(path)?.parse().ok()
    }
    /// Pre-order (document order), iterative so deep trees cannot overflow.
    fn descendants<'a>(&'a self, out: &mut Vec<&'a Node>) {
        let mut pending: Vec<&Node> = self.children.iter().rev().collect();
        while let Some(node) = pending.pop() {
            out.push(node);
            pending.extend(node.children.iter().rev());
        }
    }
}

/// A small DOM on quick-xml events; entities and character references resolved.
pub fn parse_xml(text: &str) -> Result<Node> {
    let mut reader = quick_xml::Reader::from_str(text);
    let mut stack = vec![Node::default()];
    let mut count = 0usize;
    let element = |start: &quick_xml::events::BytesStart<'_>| -> Result<Node> {
        let name = String::from_utf8_lossy(start.local_name().as_ref()).into_owned();
        let mut attrs = Vec::new();
        for attr in start.attributes() {
            let attr = attr?;
            attrs.push((
                String::from_utf8_lossy(attr.key.local_name().as_ref()).into_owned(),
                attr.normalized_value(XmlVersion::Implicit1_0)?.into_owned(),
            ));
        }
        Ok(Node {
            name,
            attrs,
            ..Node::default()
        })
    };
    loop {
        count += 1;
        ensure!(count <= MAX_NODES, "XML document is too large");
        match reader.read_event().context("invalid XML")? {
            Event::Start(start) => {
                ensure!(
                    stack.len() <= MAX_DEPTH,
                    "XML document is nested too deeply"
                );
                stack.push(element(&start)?);
            }
            Event::Empty(start) => {
                let node = element(&start)?;
                stack.last_mut().context("XML nesting")?.children.push(node);
            }
            Event::End(_) => {
                let node = stack.pop().context("XML nesting")?;
                stack
                    .last_mut()
                    .context("unbalanced XML")?
                    .children
                    .push(node);
            }
            Event::Text(text) => {
                let content = text.xml_content(XmlVersion::Implicit1_0)?;
                stack
                    .last_mut()
                    .context("XML nesting")?
                    .text
                    .push_str(&content);
            }
            Event::CData(data) => {
                let content = String::from_utf8_lossy(&data.into_inner()).into_owned();
                stack
                    .last_mut()
                    .context("XML nesting")?
                    .text
                    .push_str(&content);
            }
            Event::GeneralRef(reference) => {
                let resolved = if reference.is_char_ref() {
                    reference.resolve_char_ref()?.map(String::from)
                } else {
                    match reference.decode()?.as_ref() {
                        "amp" => Some("&".into()),
                        "lt" => Some("<".into()),
                        "gt" => Some(">".into()),
                        "quot" => Some("\"".into()),
                        "apos" => Some("'".into()),
                        _ => None,
                    }
                };
                if let Some(value) = resolved {
                    stack
                        .last_mut()
                        .context("XML nesting")?
                        .text
                        .push_str(&value);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    ensure!(stack.len() == 1, "unbalanced XML");
    let mut root = stack.pop().expect("root");
    ensure!(
        !root.children.is_empty(),
        "XML document has no root element"
    );
    Ok(root.children.remove(0))
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "import input must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take(MAX_INPUT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_INPUT,
        "import input exceeds 256 MiB"
    );
    Ok(bytes)
}

fn decode_prproj(bytes: &[u8], limit: u64) -> Result<String> {
    let mut expanded = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .take(limit.checked_add(1).context(".prproj limit is too large")?)
        .read_to_end(&mut expanded)
        .context("a .prproj must be gzip-compressed XML")?;
    ensure!(
        expanded.len() as u64 <= limit,
        ".prproj expands beyond the {} MiB limit",
        limit / (1024 * 1024)
    );
    String::from_utf8(expanded).context("a .prproj must be gzip-compressed XML in UTF-8")
}

/// Decode an xmeml or Premiere project into an edit document and notes.
pub fn import(path: &Path) -> Result<(Value, Vec<String>)> {
    let bytes = read_bounded(path)?;
    if bytes.starts_with(&[0x1f, 0x8b]) || path.extension().is_some_and(|e| e == "prproj") {
        let text = decode_prproj(&bytes, MAX_INPUT * 4)?;
        return prproj(&parse_xml(&text)?);
    }
    let text = String::from_utf8(bytes).context("XML must be UTF-8")?;
    let root = parse_xml(&text)?;
    match root.name.as_str() {
        "xmeml" => xmeml(&root),
        "PremiereData" => prproj(&root),
        other => bail!(
            "unsupported document <{other}>; import Final Cut Pro 7 XML (xmeml) or a Premiere .prproj"
        ),
    }
}

fn fps_of(rate: Option<&Node>) -> Result<(u32, u32)> {
    let rate = rate.context("missing <rate>")?;
    let timebase = rate.number(&["timebase"]).context("missing <timebase>")? as u32;
    ensure!(
        (1..=240).contains(&timebase),
        "unsupported timebase {timebase}"
    );
    let ntsc = rate
        .text_of(&["ntsc"])
        .is_some_and(|t| t.eq_ignore_ascii_case("TRUE"));
    Ok(if ntsc {
        (timebase * 1000, 1001)
    } else {
        (timebase, 1)
    })
}

fn file_path(url: &str) -> Result<PathBuf> {
    let normalized = url.replacen("file://localhost/", "file:///", 1);
    if let Ok(parsed) = url::Url::parse(&normalized)
        && let Ok(path) = parsed.to_file_path()
    {
        return Ok(path);
    }
    // Some writers emit plain paths.
    let path = PathBuf::from(url);
    ensure!(path.is_absolute(), "unsupported media URL {url}");
    Ok(path)
}

/// XML files are trusted input like scripts, but an absolute media path that
/// exists here must be a regular file, not a symlink, FIFO, device or socket.
/// Missing or unreadable media is fine: paths usually come from the exporting
/// machine. A relative path (possible in `.prproj`) resolves against the
/// written edit's directory, so `edit validate` checks it there instead.
fn refuse_link_or_special(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Ok(());
    }
    match std::fs::symlink_metadata(path) {
        Err(_) => Ok(()),
        Ok(meta) if meta.file_type().is_symlink() => bail!(
            "XML references {}, which is a symlink; relink it to the target file",
            path.display()
        ),
        Ok(meta) => {
            ensure!(
                meta.is_file(),
                "XML references {}, which is not a regular file (directories, FIFOs, devices and sockets are refused)",
                path.display()
            );
            Ok(())
        }
    }
}

#[derive(PartialEq)]
struct Effects {
    speed: f64,
    opacity: Option<(f64, Vec<(f64, f64)>)>,
    level: Option<(f64, Vec<(f64, f64)>)>,
    other: Vec<String>,
}

fn effects(item: &Node) -> Effects {
    let mut result = Effects {
        speed: 1.0,
        opacity: None,
        level: None,
        other: Vec::new(),
    };
    for effect in item.children("filter").filter_map(|f| f.child("effect")) {
        let id = effect.text_of(&["effectid"]).unwrap_or("").to_owned();
        let parameter = |name: &str| {
            effect
                .children("parameter")
                .find(|p| p.text_of(&["parameterid"]) == Some(name))
        };
        let keyframes = |p: &Node| -> Vec<(f64, f64)> {
            p.children("keyframe")
                .filter_map(|k| Some((k.number(&["when"])?, k.number(&["value"])?)))
                .collect()
        };
        match id.as_str() {
            "timeremap" => {
                if let Some(speed) = parameter("speed").and_then(|p| p.number(&["value"])) {
                    result.speed = speed / 100.0;
                }
            }
            "opacity" => {
                if let Some(p) = parameter("opacity") {
                    result.opacity =
                        Some((p.number(&["value"]).unwrap_or(100.0) / 100.0, keyframes(p)));
                }
            }
            "audiolevels" => {
                if let Some(p) = parameter("level") {
                    result.level = Some((p.number(&["value"]).unwrap_or(1.0), keyframes(p)));
                }
            }
            "" => {}
            other => result.other.push(other.to_owned()),
        }
    }
    result
}

fn enabled(item: &Node) -> bool {
    !item
        .text_of(&["enabled"])
        .is_some_and(|e| e.eq_ignore_ascii_case("FALSE"))
}

fn item_timing(item: &Node, source: &Path, notes: &mut Vec<String>) -> Option<(f64, f64, f64)> {
    let (start, end, in_frame) = (
        item.number(&["start"])?,
        item.number(&["end"])?,
        item.number(&["in"])?,
    );
    let (start, end) = if start < 0.0 || end < 0.0 {
        // -1 marks an edge owned by a transition; fall back to in/out.
        notes.push(format!(
            "{}: a transition edge was approximated",
            source.display()
        ));
        let out = item.number(&["out"]).unwrap_or(in_frame);
        if start >= 0.0 {
            (start, start + out - in_frame)
        } else {
            (end - (out - in_frame), end)
        }
    } else {
        (start, end)
    };
    Some((start, end, in_frame))
}

struct AudioItem<'a> {
    track: usize,
    item: &'a Node,
    source: PathBuf,
    timing: (f64, f64, f64),
    fx: Effects,
    enabled: bool,
    channels: HashSet<&'a str>,
    link_ids: HashSet<&'a str>,
    video_links: HashSet<&'a str>,
}

fn xmeml(root: &Node) -> Result<(Value, Vec<String>)> {
    let mut notes = Vec::new();
    let sequence = root
        .child("sequence")
        .or_else(|| {
            let mut all = Vec::new();
            root.descendants(&mut all);
            all.into_iter().find(|n| n.name == "sequence")
        })
        .context("no <sequence> in the XML")?;
    let (num, den) = fps_of(sequence.child("rate"))?;
    let fps = num as f64 / den as f64;
    let media = sequence.child("media").context("sequence has no <media>")?;
    // Bound raw work before matching partners or folding sound into video.
    for (kind, limit) in [
        ("audio", MAX_RAW_AUDIO_ITEMS),
        ("video", MAX_RAW_VIDEO_ITEMS),
    ] {
        let count = media
            .child(kind)
            .into_iter()
            .flat_map(|media| media.children("track"))
            .flat_map(|track| track.children("clipitem"))
            .take(limit + 1)
            .count();
        ensure!(
            count <= limit,
            "XML sequence may contain at most {limit} raw {kind} clipitems"
        );
    }
    let format = media.path(&["video", "format", "samplecharacteristics"]);
    let width = format.and_then(|f| f.number(&["width"])).unwrap_or(1920.0) as u32;
    let height = format.and_then(|f| f.number(&["height"])).unwrap_or(1080.0) as u32;

    // File definitions may appear once and be referenced by id later.
    let mut all = Vec::new();
    root.descendants(&mut all);
    let mut files = HashMap::<String, PathBuf>::new();
    for file in all.iter().filter(|n| n.name == "file") {
        if let (Some(id), Some(url)) = (file.attr("id"), file.text_of(&["pathurl"])) {
            let path = file_path(url)?;
            refuse_link_or_special(&path)?;
            files.insert(id.to_owned(), path);
        }
    }
    let item_file = |item: &Node| -> Option<PathBuf> {
        let file = item.child("file")?;
        files.get(file.attr("id")?).cloned()
    };

    // Keep linked sound available independently until a video can represent it.
    let video_ids: HashSet<String> = media
        .path(&["video"])
        .into_iter()
        .flat_map(|v| v.children("track"))
        .flat_map(|t| t.children("clipitem"))
        .filter_map(|c| c.attr("id").map(str::to_owned))
        .collect();
    let mut audio_items: Vec<AudioItem<'_>> = Vec::new();
    let mut seen = HashMap::<_, Vec<usize>>::new();
    let mut skipped = 0usize;
    for (track_index, track) in media
        .path(&["audio"])
        .into_iter()
        .flat_map(|a| a.children("track"))
        .enumerate()
    {
        for item in track.children("clipitem") {
            let Some(source) = item_file(item) else {
                skipped += 1;
                continue;
            };
            let Some(timing) = item_timing(item, &source, &mut notes) else {
                skipped += 1;
                continue;
            };
            let fx = effects(item);
            let channel = item.text_of(&["sourcetrack", "trackindex"]);
            let id = item.attr("id");
            // Self-links describe the item itself, not a playback association.
            let link_ids: HashSet<&str> = item
                .children("link")
                .filter_map(|l| l.text_of(&["linkclipref"]))
                .filter(|link| Some(*link) != id)
                .collect();
            let video_links: HashSet<&str> = link_ids
                .iter()
                .copied()
                .filter(|id| video_ids.contains(*id))
                .collect();
            let key = (
                source.clone(),
                [timing.0, timing.1, timing.2, fx.speed]
                    .map(|value| if value == 0.0 { 0 } else { value.to_bits() }),
            );
            let candidates = seen.entry(key).or_default();
            // Channel partners share one full-source playback, regardless of mix.
            if let Some(index) = candidates.iter().copied().find(|index| {
                let kept = &audio_items[*index];
                let new_channel = channel.is_some_and(|channel| {
                    !kept.channels.is_empty() && !kept.channels.contains(channel)
                });
                if new_channel
                    && ((link_ids.is_empty() && kept.link_ids.is_empty())
                        || !kept.video_links.is_disjoint(&video_links))
                {
                    return true;
                }
                // Explicit mutual links also join mono items mapped to both L/R.
                id.is_some_and(|id| kept.link_ids.contains(id))
                    && kept.item.attr("id").is_some_and(|id| link_ids.contains(id))
            }) {
                let kept = &mut audio_items[index];
                if kept.fx != fx || enabled(kept.item) != enabled(item) {
                    notes.push(format!(
                        "{}: per-channel mixes are not representable; the first channel's mix was used",
                        source.display()
                    ));
                }
                kept.enabled |= enabled(item);
                kept.channels.extend(channel);
                // A partner can carry additional video associations.
                kept.video_links.extend(video_links);
                continue;
            }
            candidates.push(audio_items.len());
            audio_items.push(AudioItem {
                track: track_index,
                item,
                source,
                timing,
                fx,
                enabled: enabled(item),
                channels: channel.into_iter().collect(),
                link_ids,
                video_links,
            });
        }
    }
    let mut linked_sound = HashMap::<&str, Vec<usize>>::new();
    for (index, sound) in audio_items.iter().enumerate() {
        for &video in &sound.video_links {
            linked_sound.entry(video).or_default().push(index);
        }
    }

    let seconds = |frames: f64| frames / fps;
    let mut folded = HashSet::new();
    let mut video_tracks = Vec::new();
    for track in media
        .path(&["video"])
        .into_iter()
        .flat_map(|v| v.children("track"))
    {
        let mut clips = Vec::new();
        for item in &track.children {
            if item.name == "generatoritem" || item.name == "transitionitem" {
                skipped += 1;
                continue;
            }
            if item.name != "clipitem" {
                continue;
            }
            if !enabled(item) {
                continue;
            }
            let Some(source) = item_file(item) else {
                skipped += 1;
                continue;
            };
            let Some((start, end, in_frame)) = item_timing(item, &source, &mut notes) else {
                skipped += 1;
                continue;
            };
            let fx = effects(item);
            for other in &fx.other {
                notes.push(format!(
                    "{}: effect {other} is not represented",
                    source.display()
                ));
            }
            let mut clip = json!({
                "start": seconds(start),
                "source": source,
                "source_in": seconds(in_frame) * fx.speed,
                "duration": seconds(end - start),
            });
            if (fx.speed - 1.0).abs() > 1e-9 {
                clip["speed"] = json!(fx.speed);
            }
            if let Some((opacity, keyframes)) = &fx.opacity {
                apply_opacity(&mut clip, *opacity, keyframes, in_frame, end - start, fps);
            }
            let id = item.attr("id").unwrap_or_default();
            let sound = linked_sound.get(id).and_then(|indices| {
                indices.iter().find_map(|index| {
                    let sound = &audio_items[*index];
                    (!folded.contains(index)
                        && sound.source == source
                        && sound.timing == (start, end, in_frame)
                        && sound.fx.speed == fx.speed)
                        .then_some((*index, sound))
                })
            });
            let mix = match sound {
                Some((index, sound)) => {
                    folded.insert(index);
                    if sound.enabled {
                        mix_of(&sound.fx, in_frame, end - start, fps)
                    } else {
                        Some(json!({"volume": 0}))
                    }
                }
                None => Some(json!({"volume": 0})),
            };
            if let Some(mix) = mix {
                clip["audio"] = mix;
            }
            clips.push(clip);
        }
        video_tracks.push(json!({"clips": clips}));
    }

    let mut audio_tracks: Vec<Vec<Value>> = Vec::new();
    let mut track_map = HashMap::<usize, usize>::new();
    for (index, sound) in audio_items.into_iter().enumerate() {
        if folded.contains(&index) || !sound.enabled {
            continue;
        }
        let (start, end, in_frame) = sound.timing;
        let source = sound.source;
        let fx = sound.fx;
        let mut clip = json!({
            "start": seconds(start),
            "source": source,
            "source_in": seconds(in_frame) * fx.speed,
            "duration": seconds(end - start),
        });
        if (fx.speed - 1.0).abs() > 1e-9 {
            clip["speed"] = json!(fx.speed);
        }
        if let Some(Value::Object(mix)) = mix_of(&fx, in_frame, end - start, fps) {
            for (key, value) in mix {
                clip[key] = value;
            }
        }
        let index = *track_map.entry(sound.track).or_insert_with(|| {
            audio_tracks.push(Vec::new());
            audio_tracks.len() - 1
        });
        audio_tracks[index].push(clip);
    }
    if skipped > 0 {
        notes.push(format!(
            "{skipped} items (generators, transitions, or clips without media) were not imported"
        ));
    }
    let chapters: Vec<Value> = sequence
        .children("marker")
        .filter_map(|m| {
            Some(json!({"start": seconds(m.number(&["in"])?), "title": m.text_of(&["name"]).filter(|n| !n.is_empty()).unwrap_or("Marker")}))
        })
        .collect();
    let mut edit = json!({
        "version": crate::timeline::EDIT_VERSION,
        "name": sequence.text_of(&["name"]).filter(|n| !n.is_empty()).unwrap_or("Imported sequence"),
        "width": width,
        "height": height,
        "fps": {"num": num, "den": den},
        "video": video_tracks,
        "audio": audio_tracks.into_iter().map(|clips| json!({"clips": clips})).collect::<Vec<_>>(),
    });
    if let Some(duration) = sequence.number(&["duration"]).filter(|d| *d > 0.0) {
        edit["duration"] = json!(seconds(duration));
    }
    if !chapters.is_empty() {
        edit["chapters"] = json!(chapters);
    }
    Ok((edit, notes))
}

fn apply_opacity(
    clip: &mut Value,
    opacity: f64,
    keyframes: &[(f64, f64)],
    in_frame: f64,
    length: f64,
    fps: f64,
) {
    if keyframes.is_empty() {
        if opacity < 1.0 {
            clip["opacity"] = json!(opacity.clamp(0.0, 1.0));
        }
        return;
    }
    // Recognise the fade shapes Skywrite writes: 0 -> level at the head,
    // level -> 0 at the tail. Other shapes keep their peak opacity.
    let points: Vec<(f64, f64)> = keyframes
        .iter()
        .map(|(when, value)| ((when - in_frame) / fps, value / 100.0))
        .collect();
    let peak = points
        .iter()
        .map(|p| p.1)
        .fold(0.0, f64::max)
        .clamp(0.0, 1.0);
    if peak < 1.0 {
        clip["opacity"] = json!(peak);
    }
    if let [first, second, ..] = points.as_slice()
        && first.1 <= 0.001
        && first.0 <= 0.001
        && second.1 > 0.0
    {
        clip["fade_in"] = json!(second.0);
    }
    if let [.., before, last] = points.as_slice()
        && last.1 <= 0.001
        && (last.0 - length / fps).abs() <= 1.0 / fps + 1e-9
        && before.1 > 0.0
    {
        clip["fade_out"] = json!(last.0 - before.0);
    }
}

fn mix_of(fx: &Effects, in_frame: f64, length: f64, fps: f64) -> Option<Value> {
    let (level, keyframes) = fx.level.clone()?;
    let mut mix = Map::new();
    if keyframes.is_empty() {
        if (level - 1.0).abs() > 1e-9 {
            mix.insert("volume".into(), json!(level.clamp(0.0, 4.0)));
        }
    } else {
        let mut points: Vec<Value> = Vec::new();
        let mut last = f64::NEG_INFINITY;
        for (when, value) in keyframes {
            let time = ((when - in_frame) / fps).clamp(0.0, length / fps);
            if time > last {
                points.push(json!({"time": time, "volume": value.clamp(0.0, 4.0)}));
                last = time;
            }
        }
        mix.insert("volume_keyframes".into(), json!(points));
    }
    (!mix.is_empty()).then_some(Value::Object(mix))
}

/// Experimental read-only Premiere project import. Objects are linked by
/// ObjectID/ObjectRef and ObjectUID/ObjectURef; times are ticks.
fn prproj(root: &Node) -> Result<(Value, Vec<String>)> {
    let mut notes = vec!["Experimental: .prproj is undocumented and changes between Premiere versions. Only clip placement, in points and media paths are read; effects, speed and levels are not. Prefer File > Export > Final Cut Pro XML, and report mismatches with skywrite nit.".to_owned()];
    let mut all = Vec::new();
    root.descendants(&mut all);
    let mut by_id = HashMap::<String, &Node>::new();
    for node in &all {
        for key in ["ObjectID", "ObjectUID"] {
            if let Some(id) = node.attr(key) {
                by_id.insert(id.to_owned(), node);
            }
        }
    }
    let resolve = |node: &Node| -> Option<&Node> {
        node.attr("ObjectRef")
            .or_else(|| node.attr("ObjectURef"))
            .and_then(|id| by_id.get(id).copied())
    };
    // Breadth-first search for a named element, following object references.
    let find = |start: &Node, target: &str, limit: usize| -> Option<String> {
        let mut queue = std::collections::VecDeque::from([(start, 0usize)]);
        let mut visited = 0usize;
        while let Some((node, depth)) = queue.pop_front() {
            visited += 1;
            if visited > 20_000 {
                return None;
            }
            if node.name == target && !node.text.trim().is_empty() {
                return Some(node.text.trim().to_owned());
            }
            if depth >= limit {
                continue;
            }
            for child in &node.children {
                queue.push_back((child, depth + 1));
                if let Some(target) = resolve(child) {
                    queue.push_back((target, depth + 1));
                }
            }
        }
        None
    };
    let mut video_tracks = Vec::new();
    let mut audio_tracks = Vec::new();
    for track in all.iter().filter(|n| {
        (n.name == "VideoClipTrack" || n.name == "AudioClipTrack") && n.attr("ObjectID").is_some()
    }) {
        let video = track.name == "VideoClipTrack";
        let mut items = Vec::new();
        track.descendants(&mut items);
        let mut clips = Vec::new();
        for reference in items.iter().filter(|n| n.name == "TrackItem") {
            let Some(item) = resolve(reference) else {
                continue;
            };
            let ticks = |name: &str| find(item, name, 3).and_then(|t| t.parse::<f64>().ok());
            let start = ticks("Start").unwrap_or(0.0);
            let Some(end) = ticks("End") else { continue };
            let in_point = find(item, "InPoint", 6)
                .and_then(|t| t.parse::<f64>().ok())
                .unwrap_or(0.0);
            let Some(path) = find(item, "ActualMediaFilePath", 12) else {
                continue;
            };
            refuse_link_or_special(Path::new(&path))?;
            let mut clip = json!({"start": start / PRPROJ_TICKS, "source": path,
                "source_in": in_point / PRPROJ_TICKS, "duration": (end - start) / PRPROJ_TICKS});
            if video {
                clip["audio"] = json!({"volume": 0});
            }
            clips.push(clip);
        }
        if clips.is_empty() {
            continue;
        }
        if video {
            video_tracks.push(json!({"clips": clips}));
        } else {
            audio_tracks.push(json!({"clips": clips}));
        }
    }
    ensure!(
        !video_tracks.is_empty() || !audio_tracks.is_empty(),
        "no clip tracks were recognised in this project; export Final Cut Pro XML from Premiere instead"
    );
    notes.push("Linked camera sound is imported on audio tracks; video clips are muted to avoid doubling it. Frame rate and size default to 30 fps 1920x1080: check them.".into());
    Ok((
        json!({"version": crate::timeline::EDIT_VERSION, "name": "Imported Premiere project", "width": 1920, "height": 1080,
            "fps": {"num": 30, "den": 1}, "video": video_tracks, "audio": audio_tracks}),
        notes,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prproj_expansion_limit_rejects_overflow_without_truncating() {
        use std::io::Write;

        let text = "<a>ok</a>";
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(text.as_bytes()).unwrap();
        let bytes = gzip.finish().unwrap();
        let length = text.len() as u64;
        assert_eq!(decode_prproj(&bytes, length + 1).unwrap(), text);
        assert_eq!(decode_prproj(&bytes, length).unwrap(), text);
        let error = decode_prproj(&bytes, length - 1).unwrap_err().to_string();
        assert!(error.contains(".prproj expands beyond the"), "{error}");
        assert!(error.contains("MiB limit"), "{error}");
    }

    #[test]
    fn xml_entities_and_paths() {
        let node = parse_xml("<a x=\"1 &amp; 2\"><b>caf&#233; &lt;ok&gt;</b><c/></a>").unwrap();
        assert_eq!(node.attr("x"), Some("1 & 2"));
        assert_eq!(node.text_of(&["b"]), Some("café <ok>"));
        assert!(node.child("c").is_some());
        assert_eq!(
            file_path("file://localhost/tmp/a%20b.mp4").unwrap(),
            PathBuf::from("/tmp/a b.mp4")
        );
        assert!(parse_xml("<a><b></a>").is_err());
    }
    #[test]
    fn deep_nesting_is_rejected() {
        let depth = 5000;
        let text = format!("{}{}", "<a>".repeat(depth), "</a>".repeat(depth));
        let err = parse_xml(&text).unwrap_err();
        assert!(err.to_string().contains("nested too deeply"), "{err}");
        let ok = MAX_DEPTH;
        let text = format!("{}{}", "<a>".repeat(ok), "</a>".repeat(ok));
        assert!(parse_xml(&text).is_ok());
    }
    #[test]
    fn descendants_are_pre_order() {
        let node = parse_xml("<r><a><b/><c><d/></c></a><e><f/></e><g/></r>").unwrap();
        let mut all = Vec::new();
        node.descendants(&mut all);
        let names: Vec<&str> = all.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c", "d", "e", "f", "g"]);
    }
}
