//! Model descriptions of exact frames and short video ranges.
//!
//! Frames are described independently, one request per frame, so each result
//! is cached by what it depends on: source content hash, exact presentation
//! time, image settings, model, prompt version and cast. A coarse survey and
//! a later dense "focus in" pass around a cut share every frame they have in
//! common, and nothing is paid for twice. OpenRouter requests use the data
//! policy in `openrouter::data_policy` (zero retention by default); other
//! providers receive no OpenRouter-only fields and apply their own retention.
//!
//! An agent's `--focus` answer is cached separately, keyed by the general
//! entry's key plus the focus identity, so the general cache never depends on
//! whether a focus was asked: one request fills both when neither is cached,
//! a focus-only request runs when the description is cached, and nothing is
//! requested when both are.
pub mod cache_list;
pub mod focus;

use crate::{
    media, openrouter,
    providers::ModelRef,
    review::{self, FrameSelection},
    transcript::Transcript,
};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use clap::{Args, Subcommand};
use focus::Focus;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

pub const FRAME_PROMPT_VERSION: &str = "frame-describe-v1";
pub const VIDEO_PROMPT_VERSION: &str = "video-describe-v1";
const FRAME_FIT: u32 = 768;
const JPEG_QUALITY: &str = "4";
const DEFAULT_MAX_FRAMES: usize = 600;
const HARD_MAX_FRAMES: usize = 6000;
const MAX_VIDEO_SECONDS: f64 = 600.0;
const MAX_VIDEO_BYTES: u64 = 17 * 1024 * 1024;
/// Cache directories by entry kind, under the cache root.
pub const FRAME_DIR: &str = "frame-descriptions";
pub const VIDEO_DIR: &str = "video-descriptions";
pub const FOCUS_DIR: &str = "focus-answers";

const FRAMES_ABOUT: &str = "Ask a vision model about exact frames (cached per frame): what is visible, and with --focus exactly what you need (e.g. a bounding box for every face, 0..1 coordinates).";
const FRAMES_LONG_ABOUT: &str = "Describe exact frames independently, one vision-model request per frame, cached per frame.

Every frame gets the same general description (JSON: shot_size, camera_angle, people_count, people[] with position, appearance, gaze, expression, mouth, gesture; setting, notable_objects, visible_text, image_issues, summary). Choose frames with --every N (every Nth decoded frame), --interval SECONDS (one frame per interval, e.g. --interval 1 for one per second) or --around TIME --window SECONDS (dense, every 2nd frame).

Use --focus when you need something specific from each frame that the general description does not give, such as positions or bounding boxes of faces, whether a logo is visible, or the text on a sign. The answer arrives in each frame's \"focus\" key: {instructions_sha256, schema_sha256, answer, cached, response_format, attempts, clamped; first_attempt_errors when a corrective retry fixed it; cache_error when a valid answer could not be cached}. Add --focus-schema FILE to fix its exact JSON shape; the answer is validated against it. For coordinates, counts or any answer you will process, ALWAYS pass a schema (example below; mark boxes with \"x-skywrite\": \"box\"): without one the answer is free-form and unchecked, and models may answer in pixels, on a 0-1000 scale or in prose. A face-box schema to copy into faces.json:
  {\"type\":\"object\",\"additionalProperties\":false,\"required\":[\"faces\"],\"properties\":{\"faces\":{\"type\":\"array\",\"items\":{\"type\":\"object\",\"additionalProperties\":false,\"required\":[\"box\",\"position\"],\"properties\":{\"box\":{\"x-skywrite\":\"box\"},\"position\":{\"type\":\"string\"}}}}}}
then: skywrite describe frames clip.mp4 --interval 1 --focus \"bounding box of every visible human face\" --focus-schema faces.json

Coordinates (stated to the model and checked): normalized numbers 0..1 relative to the full frame as decoded and shown (upright, after display rotation), origin top-left, x right, y down. Boxes are [x1, y1, x2, y2] = [left, top, right, bottom] with x1 < x2 and y1 < y2; points are [x, y]. Multiply by the frame's pixel width/height for pixels. They are model ESTIMATES, not detector output: view the frame (\"image\" path) before relying on one.

Cost and caching: each uncached frame costs one request. The general description and the focus answer are cached separately: with nothing cached, one request returns both; with the description cached (an earlier run with or without --focus), only a focus request is sent; with both cached, nothing is sent and no key is needed. A different --focus text or schema re-asks only the focus. So never drop --focus to save money. Time: a hosted model answers in seconds; a small local model on a CPU can take 20-60 s per frame, so 10 frames may take several minutes: run the command in the foreground with a long timeout (or wait for it) rather than abandoning it. Rerunning after an interruption reuses every answer already cached. Check first with --dry-run (would_request = initial requests this exact command would make; a corrective focus retry can add one per frame) and list what is stored with `skywrite describe cache SOURCE`.";
const FRAMES_AFTER_HELP: &str = "Focus schema subset (--focus-schema, JSON, at most 16 KiB, depth 8, 256 nodes): type (object, array, string, number, integer, boolean, null, or a list of them), properties, required, additionalProperties (true/false), items, enum, const, minimum, maximum, exclusiveMinimum, exclusiveMaximum, minItems, maxItems, minLength, maxLength, description, title, $schema (top level), and \"x-skywrite\": \"box\" or \"point\" to mark a normalized coordinate array (validated: 4 or 2 numbers in 0..1, x1<x2, y1<y2; values within 0.02 outside 0..1 are clamped and counted in \"clamped\"). Beside \"x-skywrite\" only type, description and title are allowed (it defines its own length and range). enum values are scalars (strings, numbers, booleans, null). Numbers compare as IEEE-754 doubles (RFC 8259's interoperable precision; whole numbers exactly). Schema numbers must be integers within 64 bits or floats that are 0 or between 2^-1022 and 2^53 in magnitude. Anything else (oneOf, $ref, pattern, ...) is refused with the keyword named. Focus text: up to 16 --focus items plus --focus-file, 4000 bytes together.

Failures: an answer that does not match the schema gets one corrective retry; if it still fails, that frame's focus is {\"error\": {\"code\": \"focus_schema_mismatch\" or \"focus_invalid_json\", \"errors\": [...], \"attempts\": 2}} (not cached; rerun the same command to retry only those). The general description is kept either way. A valid description or focus answer that could not be written to the cache is still returned, with \"cache_error\". If every focus answer fails, the command fails. When the provider's model catalog lists structured_outputs for the model, focus requests use response_format json_schema: strict for focus-only requests when every object in the schema is closed (additionalProperties false) and fully required, non-strict when the same request also returns the general description (which has no strict schema). Otherwise json_object. The same local validation applies either way (report: focus.structured_outputs true, false, or null = unknown/not looked up).

Examples:
  # Face boxes once per second
  cat > faces.json <<'EOF'
  {\"type\":\"object\",\"additionalProperties\":false,\"required\":[\"faces\"],\"properties\":{\"faces\":{\"type\":\"array\",\"items\":{\"type\":\"object\",\"additionalProperties\":false,\"required\":[\"box\",\"position\"],\"properties\":{\"box\":{\"x-skywrite\":\"box\"},\"position\":{\"type\":\"string\"}}}}}}
  EOF
  skywrite describe frames clip.mp4 --interval 1 --focus \"bounding box of every visible human face\" --focus-schema faces.json -o faces-out.json
  # The same command again with --dry-run: would_request 0 when everything is cached
  skywrite describe frames clip.mp4 --interval 1 --focus \"bounding box of every visible human face\" --focus-schema faces.json --dry-run
  skywrite describe cache clip.mp4";

const VIDEO_LONG_ABOUT: &str = "Describe motion, expressions, gestures and shot changes in a short source range (up to 600 s) with a video-input model: the range is re-encoded to H.264 (--height, default 480) without audio and sent inline in one request, cached by source content, range, encoding, model, prompt and cast.

Use describe frames instead for exact per-frame facts and positions; use describe video for what happens over time. --focus asks for something specific about the clip (e.g. \"when does the speaker first look at the camera\"); the answer is in \"focus\" ({instructions_sha256, schema_sha256, answer, clip_start, ...}), times in it are seconds from the clip start (add focus.clip_start for source time), coordinates use the same normalized 0..1 top-left convention as describe frames ([x1, y1, x2, y2] boxes; model estimates). --focus-schema fixes its shape (same subset as describe frames; see its --help).

Caching is split the same way as describe frames: one request returns both when nothing is cached, a focus-only request when the description is cached, nothing when both are. --dry-run reports cached, focus_cached and would_request (0 or 1 initial request; a corrective focus retry can add one) without requesting.";
const VIDEO_AFTER_HELP: &str = "Examples:
  skywrite describe video interview.mp4 --start 56 --duration 8 -o laugh.json
  skywrite describe video interview.mp4 --start 56 --duration 8 --focus \"timestamps when the speaker smiles\" --dry-run";

#[derive(Subcommand)]
pub enum DescribeCommand {
    #[command(about = FRAMES_ABOUT, long_about = FRAMES_LONG_ABOUT, after_long_help = FRAMES_AFTER_HELP)]
    Frames(FrameArgs),
    /// Describe motion, expressions, gestures and shot changes in a short source range with a video-input model.
    #[command(long_about = VIDEO_LONG_ABOUT, after_long_help = VIDEO_AFTER_HELP)]
    Video(VideoArgs),
    #[command(
        about = cache_list::ABOUT,
        long_about = cache_list::LONG_ABOUT,
        after_long_help = cache_list::AFTER_HELP
    )]
    Cache(cache_list::CacheArgs),
}

/// What the agent needs from each frame or clip, in addition to the general description.
#[derive(Args, Default, Clone)]
pub struct FocusArgs {
    /// What you need from each frame/clip beyond the general description, in plain words (repeatable; joined in order). Answer in each result's "focus" key. Coordinates: normalized 0..1, top-left origin, boxes [x1,y1,x2,y2]; model estimates. For boxes or any fixed shape add --focus-schema, or the answer is unchecked.
    #[arg(long = "focus", value_name = "TEXT")]
    pub focus: Vec<String>,
    /// Read focus instructions from a UTF-8 text file (appended after --focus items; 4000 bytes total).
    #[arg(long, value_name = "FILE")]
    pub focus_file: Option<PathBuf>,
    /// JSON Schema (documented subset; "x-skywrite": "box"/"point" marks coordinates) that the focus answer must match. Needs --focus or --focus-file.
    #[arg(long, value_name = "FILE")]
    pub focus_schema: Option<PathBuf>,
}

impl FocusArgs {
    fn inputs(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.focus_file
            .iter()
            .chain(self.focus_schema.iter())
            .cloned()
    }
    fn load(&self) -> Result<Option<Focus>> {
        focus::load(
            &self.focus,
            self.focus_file.as_deref(),
            self.focus_schema.as_deref(),
        )
    }
}

#[derive(Args)]
pub struct FrameArgs {
    pub source: PathBuf,
    /// Range start in source seconds (from the first presented sample).
    #[arg(long, default_value_t = 0.0)]
    pub start: f64,
    /// Range length in seconds; defaults to the rest of the source.
    #[arg(long)]
    pub duration: Option<f64>,
    /// Center a dense window on this source time instead of --start/--duration.
    #[arg(long, conflicts_with_all = ["start", "duration"])]
    pub around: Option<f64>,
    /// Window length for --around, in seconds.
    #[arg(long, default_value_t = 1.0, requires = "around")]
    pub window: f64,
    /// Describe every Nth decoded frame (30 = about once a second at 30 fps; 2 = dense).
    #[arg(long, conflicts_with = "interval")]
    pub every: Option<u32>,
    /// Describe one frame per this many seconds (1 = one frame per second).
    #[arg(long)]
    pub interval: Option<f64>,
    /// Optional cast file of TEXT appearance descriptions for advisory "who is visible" matching.
    #[arg(long)]
    pub cast: Option<PathBuf>,
    /// Attach words active at each frame from this word-timed transcript (not sent to the model).
    #[arg(long)]
    pub transcript: Option<PathBuf>,
    #[command(flatten)]
    pub focus: FocusArgs,
    #[arg(long)]
    pub model: Option<String>,
    /// Refuse runs where more frames than this need a request (a description and/or a focus answer; default 600). A corrective focus retry can add one more request per frame.
    #[arg(long, default_value_t = DEFAULT_MAX_FRAMES)]
    pub max_frames: usize,
    /// Parallel requests, 1..16.
    #[arg(long, default_value_t = 4)]
    pub concurrency: usize,
    /// Extract frames and report what is cached and how many requests this exact command would make (would_request); make no requests.
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    #[arg(long)]
    pub overwrite: bool,
}

#[derive(Args)]
pub struct VideoArgs {
    pub source: PathBuf,
    #[arg(long, default_value_t = 0.0)]
    pub start: f64,
    #[arg(long)]
    pub duration: f64,
    /// Maximum encoded height for the request (smaller is cheaper; 240..1080).
    ///
    /// Pixels, default 480. The clip is never upscaled: a source whose displayed
    /// height (rotation applied, so a phone clip stored 1920x1080 with rotation 90
    /// is 1920 high) is smaller keeps that height, rounded down to an even number.
    /// The result's encoding field names the height sent, and results are cached
    /// per encoding.
    #[arg(long, default_value_t = 480, value_name = "PIXELS")]
    pub height: u32,
    #[arg(long)]
    pub cast: Option<PathBuf>,
    #[command(flatten)]
    pub focus: FocusArgs,
    #[arg(long)]
    pub model: Option<String>,
    /// Report whether the description and focus answer are cached and whether a request would be made; make none.
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    #[arg(long)]
    pub overwrite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CastMember {
    pub name: String,
    pub description: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Cast {
    pub version: u32,
    pub people: Vec<CastMember>,
}

pub fn load_cast(path: &Path) -> Result<Cast> {
    let cast: Cast = crate::io::read_json(path)?;
    ensure!(cast.version == 1, "cast files use \"version\": 1");
    ensure!(
        (1..=32).contains(&cast.people.len()),
        "a cast lists 1..32 people"
    );
    let mut names = std::collections::HashSet::new();
    for person in &cast.people {
        ensure!(
            !person.name.trim().is_empty()
                && person.name.len() <= 64
                && !person.name.chars().any(char::is_control),
            "cast names are 1..64 bytes without control characters"
        );
        ensure!(
            !person.description.trim().is_empty() && person.description.len() <= 512,
            "cast descriptions are 1..512 bytes"
        );
        ensure!(
            person.name != "unknown" && names.insert(person.name.to_lowercase()),
            "cast names must be unique and not \"unknown\""
        );
    }
    Ok(cast)
}

fn cast_sha256(cast: Option<&Cast>) -> Result<Option<String>> {
    cast.map(|cast| Ok(focus::sha256_hex(&serde_json::to_vec(cast)?)))
        .transpose()
}

const FRAME_PROMPT: &str = r#"You describe ONE exact video frame for a video editor's searchable log. Describe only what is visible in this single still image. Do not guess names, identities, off-screen events, or what happened before/after. Do not invent text, objects, or emotions; describe facial expression only from visible cues. If something is unclear, write "unclear" instead of guessing.

Return only a JSON object with exactly these keys:
{
  "shot_size": "extreme_close_up|close_up|medium_close_up|medium|medium_wide|wide|extreme_wide|insert|empty",
  "camera_angle": "eye_level|high|low|overhead|unclear",
  "people_count": integer,
  "people": [
    {
      "position": "where in frame (e.g. left, center, right, background)",
      "appearance": "visible clothing, hair, accessories",
      "framing": "how much of the person is visible (e.g. head and shoulders, partially cut off)",
      "gaze": "where they are looking, relative to the camera",
      "expression": "neutral|smiling|laughing|frowning|concentrating|surprised|unclear|other: short",
      "mouth": "closed|open|partly_open|not_visible",
      "appears_mid_speech": true|false|null,
      "gesture_or_action": "visible hand/body action, or none"
    }
  ],
  "setting": "short description of the location",
  "notable_objects": ["visible objects only"],
  "visible_text": ["exact legible text only; empty if none"],
  "image_issues": ["e.g. motion_blur, low_resolution, dark, compression; empty if none"],
  "summary": "one factual sentence"
}"#;

const CAST_SECTION: &str = r#"

Also match visible people against this cast list of TEXT appearance descriptions supplied by the user (there are no reference photos). Judge only from visible clothing, hair, accessories, position and setting. Do not infer identity from faces, and do not assume a listed person is present. Entries may be similar: choose one only when the deciding feature is actually visible and fits it better than every other entry; otherwise answer "unknown". Add these keys to the JSON object:
  "cast_matches": [{"position": "where in frame", "match": "exact cast name or unknown", "deciding_feature_seen": "the visible cue that decided it", "runner_up": "second most likely cast name or none"}],
  "cast_present": ["cast names clearly visible"]

Cast list:
"#;

fn frame_prompt(cast: Option<&Cast>) -> String {
    let mut prompt = FRAME_PROMPT.to_owned();
    if let Some(cast) = cast {
        prompt.push_str(CAST_SECTION);
        for person in &cast.people {
            prompt.push_str(&format!("- {}: {}\n", person.name, person.description));
        }
    }
    prompt
}

const VIDEO_PROMPT: &str = r#"You describe a short video clip for a video editor. Describe what actually happens over time: who is visible (by appearance, not by name), their motion, gestures, head turns, gaze changes, facial expressions and when they change, whether mouths move as if speaking, camera movement, and any cuts or shot changes. Give approximate timestamps in seconds from the clip start. Only report what is visible; write "unclear" rather than guessing. Do not describe audio.

Return only a JSON object:
{
  "duration_seconds_estimate": number,
  "shots": [{"start": number, "end": number, "shot_size": "...", "camera_motion": "static|pan|tilt|zoom|handheld|unclear", "description": "..."}],
  "events": [{"time": number, "who": "appearance-based label", "event": "visible action, gesture, expression change, or speech onset"}],
  "people": [{"label": "appearance-based label", "appearance": "...", "speaking_visibly": "yes|no|unclear"}],
  "summary": "two factual sentences at most"
}"#;

fn video_prompt(cast: Option<&Cast>) -> String {
    let mut prompt = VIDEO_PROMPT.to_owned();
    if let Some(cast) = cast {
        prompt.push_str("\n\nPeople may match this cast of TEXT appearance descriptions (advisory; say unknown unless the deciding feature is visible). Use a cast name as \"who\" only when clear:\n");
        for person in &cast.people {
            prompt.push_str(&format!("- {}: {}\n", person.name, person.description));
        }
    }
    prompt
}

/// The cast for a focus-only request: who may appear, as advisory text, so the
/// focus sees the same evidence whether or not the description was cached.
fn focus_cast_context(cast: Option<&Cast>) -> String {
    let Some(cast) = cast else {
        return String::new();
    };
    let mut text = String::from(
        "People may match this cast of TEXT appearance descriptions supplied by the user (no reference photos; advisory, not identification). Use a cast name only when its deciding feature is visible; otherwise say unknown:\n",
    );
    for person in &cast.people {
        text.push_str(&format!("- {}: {}\n", person.name, person.description));
    }
    text
}

/// Cache key for one frame description; public so tests can seed the cache.
/// Unchanged by `--focus`: a focus never changes the general entry.
pub fn frame_cache_key(
    source_sha256: &str,
    time: f64,
    width: u32,
    target: &ModelRef,
    cast: Option<&Cast>,
) -> Result<String> {
    let cast = serde_json::to_vec(&cast)?;
    Ok(openrouter::cache_key(&[
        FRAME_PROMPT_VERSION.as_bytes(),
        source_sha256.as_bytes(),
        format!("{time:.6}").as_bytes(),
        format!("fit{FRAME_FIT}-w{width}-q{JPEG_QUALITY}").as_bytes(),
        target.provider.identity().as_bytes(),
        target.model.as_bytes(),
        &cast,
    ]))
}

/// Cache key for one video range description.
pub fn video_cache_key(
    source_sha256: &str,
    start: f64,
    duration: f64,
    encode: &str,
    target: &ModelRef,
    cast: Option<&Cast>,
) -> Result<String> {
    Ok(openrouter::cache_key(&[
        VIDEO_PROMPT_VERSION.as_bytes(),
        source_sha256.as_bytes(),
        format!("{start:.6}:{duration:.6}").as_bytes(),
        encode.as_bytes(),
        target.provider.identity().as_bytes(),
        target.model.as_bytes(),
        &serde_json::to_vec(&cast)?,
    ]))
}

/// Cache key for a focus answer: everything the general entry depends on
/// (through its key) plus the focus instructions, schema and focus prompt.
pub fn focus_cache_key(general_key: &str, focus: &Focus) -> String {
    openrouter::cache_key(&[
        focus::FOCUS_PROMPT_VERSION.as_bytes(),
        general_key.as_bytes(),
        focus.sha256.as_bytes(),
    ])
}

/// What a report says about routing: the OpenRouter data policy, or that a
/// custom provider received none of it.
fn routing(target: &ModelRef) -> Result<(Value, Value)> {
    Ok(if target.provider.is_openrouter() {
        (json!(openrouter::data_policy()?), Value::Null)
    } else {
        (
            Value::Null,
            json!(format!(
                "provider {} is not OpenRouter: SKYWRITE_DATA_POLICY routing is not applied and the server's own retention applies",
                target.provider.name
            )),
        )
    })
}

#[derive(Serialize, Deserialize)]
pub(crate) struct CachedDescription {
    pub version: u32,
    pub model: String,
    /// The general description, or for a focus entry the validated answer.
    pub description: Value,
    pub usage: Value,
    /// What the entry belongs to, for `describe cache` (entries written
    /// before it existed get it the next time a run reads them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

pub(crate) const MAX_CACHE_ENTRY_BYTES: u64 = 1024 * 1024;

pub(crate) fn read_cached(path: &Path) -> Option<CachedDescription> {
    let bytes = crate::io::read_regular_bounded(path, MAX_CACHE_ENTRY_BYTES, true).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn with_created(meta: &Value) -> Value {
    let mut meta = meta.clone();
    meta["created_at"] = json!(now_unix());
    meta
}

/// Publishes an entry. A valid entry already at the path (a concurrent run
/// wrote the same answer) is kept; a rejected one (corrupt, oversized, or a
/// focus answer that fails `focus`'s schema) is replaced, so a bad file cannot
/// make every rerun pay again. The directory is checked first (private, not a
/// symlink).
fn write_cached(path: &Path, entry: &CachedDescription, focus: Option<&Focus>) -> Result<()> {
    if let Some(parent) = path.parent() {
        crate::cache::private_dir(parent)?;
    }
    let bytes = serde_json::to_vec(entry)?;
    if crate::io::atomic_write(path, &bytes, false).is_ok() {
        return Ok(());
    }
    let existing_valid = read_cached(path).is_some_and(|existing| match focus {
        None => true,
        Some(focus) => {
            let checked = focus::validate(focus.schema.as_ref(), &existing.description);
            checked.errors.is_empty() && checked.clamped == 0
        }
    });
    if existing_valid {
        return Ok(());
    }
    crate::io::atomic_write(path, &bytes, true)
}

/// Adds listing metadata to an entry written before it was recorded. Best
/// effort: a read-only cache, or a directory that is not a private regular
/// one (a symlink), keeps the entry as it is.
fn annotate(path: &Path, mut entry: CachedDescription, meta: &Value) -> CachedDescription {
    if entry.meta.is_none() {
        let mut meta = meta.clone();
        meta["created_at"] = json!(
            std::fs::symlink_metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
        );
        entry.meta = Some(meta);
        let safe_dir = path
            .parent()
            .is_some_and(|parent| crate::cache::private_dir(parent).is_ok());
        // Re-read just before publishing: never replace an entry another run
        // has since rewritten (it has metadata, or different content).
        let unchanged = || {
            read_cached(path)
                .is_some_and(|now| now.meta.is_none() && now.description == entry.description)
        };
        if safe_dir
            && unchanged()
            && let Ok(bytes) = serde_json::to_vec(&entry)
        {
            let _ = crate::io::atomic_write(path, &bytes, true);
        }
    }
    entry
}

/// A cached focus answer, re-validated so an edited cache cannot bypass the
/// schema. Answers are stored already clamped, so one that would need
/// clamping now was edited and counts as a miss.
fn read_focus(path: &Path, focus: &Focus, meta: &Value) -> Option<Value> {
    let entry = read_cached(path)?;
    let checked = focus::validate(focus.schema.as_ref(), &entry.description);
    if !checked.errors.is_empty() || checked.clamped > 0 {
        return None;
    }
    Some(annotate(path, entry, meta).description)
}

/// Requests and what they cost, shared by the workers of one run.
struct Asker<'a> {
    /// `None` when every answer is cached: no client, so no key is needed.
    api: Option<&'a openrouter::Client>,
    model: &'a str,
    spent: Mutex<f64>,
    uncosted: AtomicUsize,
    requests: AtomicUsize,
}

/// Cost of the requests made for one frame or clip; `None` when any was unreported.
#[derive(Default)]
struct Spend {
    total: f64,
    unknown: bool,
}
impl Spend {
    fn value(&self) -> Value {
        if self.unknown {
            Value::Null
        } else {
            json!(self.total)
        }
    }
}

struct Answer {
    parsed: Result<Value>,
    raw: String,
    usage: Value,
}

impl<'a> Asker<'a> {
    fn new(api: Option<&'a openrouter::Client>, model: &'a str) -> Self {
        Self {
            api,
            model,
            spent: Mutex::new(0.0),
            uncosted: AtomicUsize::new(0),
            requests: AtomicUsize::new(0),
        }
    }
    /// One request; disables reasoning where allowed (a per-frame task does not
    /// need it), and repeats without that setting when a model requires reasoning.
    /// A rejected request is not billed, so this cannot double-charge. A completed
    /// request returns its usage even when the answer is not valid JSON, so the
    /// caller can account for what it cost before reporting the failure.
    fn ask(
        &self,
        messages: Value,
        response_format: Value,
        max_tokens: u32,
        spend: &mut Spend,
    ) -> Result<Answer> {
        let mut request = json!({"model":self.model,"messages":messages,
            "response_format":response_format,"temperature":0.2,"max_tokens":max_tokens,
            "reasoning":{"enabled":false}});
        let api = self.api.context("no model client for this run")?;
        let response = match api.request("chat/completions", &request) {
            Ok(response) => response,
            Err(error)
                if api.is_openrouter() && error.to_string().contains("Reasoning is mandatory") =>
            {
                request.as_object_mut().expect("object").remove("reasoning");
                api.request("chat/completions", &request)?
            }
            Err(error) => return Err(error),
        };
        self.requests.fetch_add(1, Ordering::SeqCst);
        let usage = response.body.get("usage").cloned().unwrap_or(Value::Null);
        match cost(&usage) {
            Some(cost) => {
                *self.spent.lock().expect("lock") += cost;
                spend.total += cost;
            }
            None => {
                self.uncosted.fetch_add(1, Ordering::SeqCst);
                spend.unknown = true;
            }
        }
        let raw = response.body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        Ok(Answer {
            parsed: openrouter::assistant_json(&response.body),
            raw,
            usage,
        })
    }
    fn spent(&self) -> (f64, usize) {
        (
            *self.spent.lock().expect("lock"),
            self.uncosted.load(Ordering::SeqCst),
        )
    }
}

/// How one frame or clip is served, from what the cache already holds.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Need {
    Nothing,
    /// The general description only (no focus, or the focus is cached).
    Plain,
    /// Neither cached: one request answers both.
    Combined,
    /// The description is cached: ask only for the focus.
    FocusOnly,
}

fn need(general_cached: bool, focus_wanted: bool, focus_cached: bool) -> Need {
    match (general_cached, focus_wanted && !focus_cached) {
        (true, false) => Need::Nothing,
        (true, true) => Need::FocusOnly,
        (false, false) => Need::Plain,
        (false, true) => Need::Combined,
    }
}

/// The parts of a request that describe one frame or clip.
struct Subject<'a> {
    /// "frame" or "clip".
    kind: &'static str,
    general_prompt: &'a str,
    /// Context a focus-only request still needs (the cast), without the general task.
    context: &'a str,
    media: Value,
    max_tokens: u32,
}

struct FocusRun<'a> {
    focus: &'a Focus,
    structured: Option<bool>,
}

const MAX_ECHOED_ANSWER: usize = 8000;

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() > limit {
        format!("{}…", text.chars().take(limit).collect::<String>())
    } else {
        text.to_owned()
    }
}

/// The focus value or the problems with it.
fn check_focus(
    focus: &Focus,
    answer: &Result<Option<Value>, String>,
) -> std::result::Result<(Value, usize), (&'static str, Vec<String>)> {
    // Numbers compare as IEEE-754 doubles (RFC 8259 interoperability); see focus::validate.
    match answer {
        Err(error) => Err(("focus_invalid_json", vec![error.clone()])),
        Ok(None) => Err((
            "focus_invalid_json",
            vec!["the answer has no \"focus\" key".into()],
        )),
        Ok(Some(value)) => {
            let checked = focus::validate(focus.schema.as_ref(), value);
            if checked.errors.is_empty() {
                Ok((checked.value, checked.clamped))
            } else {
                Err(("focus_schema_mismatch", checked.errors))
            }
        }
    }
}

/// Validates a focus answer; on a mismatch asks once more with the errors,
/// then reports a typed failure. `Err` only for a request that must stop the run.
fn settle_focus(
    asker: &Asker,
    run: &FocusRun,
    first_message: &Value,
    first: (&Result<Option<Value>, String>, &str),
    label: &'static str,
    max_tokens: u32,
    spend: &mut Spend,
) -> Result<Value> {
    let focus = run.focus;
    let base = json!({"instructions_sha256":focus.instructions_sha256,"schema_sha256":focus.schema_sha256});
    let success = |value: Value, clamped: usize, attempts: usize, label: &str| {
        let mut out = base.clone();
        out["answer"] = value;
        out["cached"] = json!(false);
        out["response_format"] = json!(label);
        out["attempts"] = json!(attempts);
        out["clamped"] = json!(clamped);
        out
    };
    let (code, errors) = match check_focus(focus, first.0) {
        Ok((value, clamped)) => return Ok(success(value, clamped, 1, label)),
        Err(problem) => problem,
    };
    let (format, retry_label) = focus::response_format(focus, false, run.structured);
    let messages = json!([first_message,
        {"role":"assistant","content":truncate(first.1, MAX_ECHOED_ANSWER)},
        {"role":"user","content":focus::correction(&errors)}]);
    let failure = |code: &str, message: String, errors: Vec<String>, last: &str| {
        let mut out = base.clone();
        out["error"] = json!({"code":code,"message":message,"errors":errors,"attempts":2,
            "last_answer":truncate(last, 2000)});
        out
    };
    let retry = match asker.ask(messages, format, max_tokens, spend) {
        Ok(retry) => retry,
        Err(error) if error.downcast_ref::<openrouter::Unreachable>().is_some() => {
            return Err(error);
        }
        Err(error) => {
            return Ok(failure(
                "focus_request_failed",
                format!("the corrective retry failed: {error:#}"),
                errors,
                first.1,
            ));
        }
    };
    let second = match &retry.parsed {
        Ok(parsed) => Ok(parsed.get("focus").cloned()),
        Err(error) => Err(format!("{error:#}")),
    };
    match check_focus(focus, &second) {
        Ok((value, clamped)) => {
            // Say what the corrective retry fixed, so a caller can see why it cost two requests.
            let mut out = success(value, clamped, 2, retry_label);
            out["first_attempt_errors"] = json!(errors);
            Ok(out)
        }
        Err((code2, errors2)) => Ok(failure(
            code2,
            format!(
                "the focus answer did not match after one corrective retry (first attempt: {code})"
            ),
            errors2,
            &retry.raw,
        )),
    }
}

/// What one frame or clip produced: a new description (already cached) or
/// why there is none, and the focus result (an answer or a typed failure).
struct Served {
    description: Option<Value>,
    description_error: Option<String>,
    /// A valid description that could not be written to the cache.
    description_cache_error: Option<String>,
    focus: Option<Value>,
}

fn focus_failure(focus: &Focus, code: &str, message: String, attempts: usize) -> Value {
    json!({"instructions_sha256":focus.instructions_sha256,"schema_sha256":focus.schema_sha256,
        "error":{"code":code,"message":message,"errors":[],"attempts":attempts}})
}

/// Runs the request(s) `need` calls for. `store` caches a new description
/// before any focus retry, so a later failure never loses it. `Err` only when
/// the request for a needed description fails or the provider is unreachable.
fn serve(
    asker: &Asker,
    need: Need,
    subject: &Subject,
    run: Option<&FocusRun>,
    store: &dyn Fn(&Value, &Value) -> Result<()>,
    spend: &mut Spend,
) -> Result<Served> {
    let message = |text: String| json!({"role":"user","content":[{"type":"text","text":text}, subject.media.clone()]});
    match (need, run) {
        (Need::Nothing, _) => Ok(Served {
            description: None,
            description_error: None,
            description_cache_error: None,
            focus: None,
        }),
        (Need::Plain, _) | (Need::Combined | Need::FocusOnly, None) => {
            let answer = asker.ask(
                json!([message(subject.general_prompt.to_owned())]),
                json!({"type":"json_object"}),
                subject.max_tokens,
                spend,
            )?;
            let description = answer.parsed?;
            let description_cache_error = store(&description, &answer.usage)
                .err()
                .map(|error| format!("{error:#}"));
            Ok(Served {
                description: Some(description),
                description_error: None,
                description_cache_error,
                focus: None,
            })
        }
        (Need::Combined, Some(run)) => {
            let first = message(format!(
                "{}{}",
                subject.general_prompt,
                focus::combined_suffix(run.focus, subject.kind)
            ));
            let (format, label) = focus::response_format(run.focus, true, run.structured);
            let tokens = subject.max_tokens * 2;
            let answer = asker.ask(json!([first.clone()]), format, tokens, spend)?;
            // Each part stands alone: a usable description is cached even when
            // the focus needs a retry, and a usable focus even without a description.
            let (description, value) = match &answer.parsed {
                Ok(parsed) => split_combined(parsed),
                Err(error) => (Err(format!("{error:#}")), Err(format!("{error:#}"))),
            };
            // A cache write failure never discards either answer.
            let (description_error, description_cache_error) = match &description {
                Ok(description) => (
                    None,
                    store(description, &answer.usage)
                        .err()
                        .map(|error| format!("{error:#}")),
                ),
                Err(error) => (Some(error.clone()), None),
            };
            let focus = settle_focus(
                asker,
                run,
                &first,
                (&value, &answer.raw),
                label,
                tokens,
                spend,
            )?;
            Ok(Served {
                description: description.ok(),
                description_error,
                description_cache_error,
                focus: Some(focus),
            })
        }
        (Need::FocusOnly, Some(run)) => {
            let first = message(focus::only_prompt(run.focus, subject.kind, subject.context));
            let (format, label) = focus::response_format(run.focus, false, run.structured);
            let answer = match asker.ask(json!([first.clone()]), format, subject.max_tokens, spend)
            {
                Ok(answer) => answer,
                Err(error) if error.downcast_ref::<openrouter::Unreachable>().is_some() => {
                    return Err(error);
                }
                Err(error) => {
                    // The description is cached; only the focus failed.
                    return Ok(Served {
                        description: None,
                        description_error: None,
                        description_cache_error: None,
                        focus: Some(focus_failure(
                            run.focus,
                            "focus_request_failed",
                            format!("the focus request failed: {error:#}"),
                            1,
                        )),
                    });
                }
            };
            let value = match &answer.parsed {
                Ok(parsed) => Ok(parsed.get("focus").cloned()),
                Err(error) => Err(format!("{error:#}")),
            };
            let focus = settle_focus(
                asker,
                run,
                &first,
                (&value, &answer.raw),
                label,
                subject.max_tokens,
                spend,
            )?;
            Ok(Served {
                description: None,
                description_error: None,
                description_cache_error: None,
                focus: Some(focus),
            })
        }
    }
}

/// Splits a combined answer into its description and focus value, each with
/// its own problem. A model that returns the description's keys at the top
/// level beside "focus" is accepted; the focus value is never altered.
#[allow(clippy::type_complexity)]
fn split_combined(
    parsed: &Value,
) -> (
    std::result::Result<Value, String>,
    std::result::Result<Option<Value>, String>,
) {
    let Some(object) = parsed.as_object() else {
        let problem = "the answer is not a JSON object".to_owned();
        return (Err(problem.clone()), Err(problem));
    };
    let mut object = object.clone();
    let focus = object.remove("focus");
    let description = match object.remove("description") {
        Some(Value::Object(description)) => Ok(Value::Object(description)),
        None if object.contains_key("summary") => Ok(Value::Object(object)),
        _ => Err("model answer has no \"description\" object beside \"focus\"".to_owned()),
    };
    (description, Ok(focus))
}

fn fit_width(info: &media::MediaInfo) -> u32 {
    // The displayed (upright) size: frames are extracted after display
    // rotation, and focus coordinates are relative to that frame.
    let (w, h) = info
        .video()
        .and_then(|v| v.display_size())
        .unwrap_or((16, 9));
    let (w, h) = (w as f64, h as f64);
    let width = if w >= h {
        FRAME_FIT as f64
    } else {
        FRAME_FIT as f64 * w / h
    };
    ((width / 2.0).round() as u32 * 2).clamp(2, FRAME_FIT)
}

/// Billed cost when the provider reports one; `None` is unknown, not free.
fn cost(usage: &Value) -> Option<f64> {
    usage["cost"].as_f64()
}

pub fn execute(command: DescribeCommand) -> Result<Value> {
    match command {
        DescribeCommand::Frames(args) => frames(&args),
        DescribeCommand::Video(args) => video(&args),
        DescribeCommand::Cache(args) => cache_list::list(&args),
    }
}

fn finish(value: Value, output: Option<&Path>, overwrite: bool) -> Result<Value> {
    if let Some(path) = output {
        crate::io::write_json(path, &value, overwrite)?;
    }
    Ok(value)
}

/// Whether the model takes strict `json_schema` output, from the provider's
/// model catalog (`supported_parameters` lists `structured_outputs`). Only
/// looked up when a schema is given and a request will be made.
fn structured_outputs(
    focus: Option<&Focus>,
    target: &ModelRef,
    cache_dir: Option<&Path>,
) -> Result<Option<bool>> {
    if focus.and_then(|focus| focus.schema.as_ref()).is_none() {
        return Ok(None);
    }
    crate::models::supports_structured_outputs(&target.provider, &target.model, cache_dir)
}

fn focus_report(focus: Option<&Focus>, structured: Option<bool>, requested: bool) -> Value {
    focus.map_or(Value::Null, |focus| {
        json!({
            "instructions_sha256": focus.instructions_sha256,
            "schema_sha256": focus.schema_sha256,
            "sha256": focus.sha256,
            "preview": focus::preview(&focus.text),
            "prompt_version": focus::FOCUS_PROMPT_VERSION,
            // null: not looked up (no schema or no request) or the catalog did not say.
            "structured_outputs": if requested { json!(structured) } else { Value::Null },
            "coordinates": "normalized 0..1 of the full upright frame, origin top-left; boxes [x1, y1, x2, y2] (left, top, right, bottom); model estimates",
        })
    })
}

struct FrameWork {
    time: f64,
    image: PathBuf,
    general: PathBuf,
    focus: Option<PathBuf>,
    fresh: PathBuf,
    meta: Value,
    focus_meta: Option<Value>,
}

fn frames(args: &FrameArgs) -> Result<Value> {
    frames_with_api(args, |provider| openrouter::Client::new(provider, None))
}

fn frames_with_api(
    args: &FrameArgs,
    api: impl FnOnce(&crate::providers::Provider) -> Result<openrouter::Client>,
) -> Result<Value> {
    ensure!(
        (1..=16).contains(&args.concurrency),
        "--concurrency must be 1..16"
    );
    ensure!(
        (1..=HARD_MAX_FRAMES).contains(&args.max_frames),
        "--max-frames must be 1..{HARD_MAX_FRAMES}"
    );
    let info = media::probe_for_timing(&args.source)?;
    if let Some(output) = &args.output {
        let mut inputs = vec![info.path.clone()];
        inputs.extend(args.cast.iter().chain(args.transcript.iter()).cloned());
        inputs.extend(args.focus.inputs());
        crate::io::protect_inputs(output, &inputs)?;
        ensure!(
            args.overwrite || !output.exists(),
            "output already exists; pass --overwrite"
        );
    }
    let (start, duration) = match args.around {
        Some(center) => {
            ensure!(
                args.window.is_finite() && (0.04..=60.0).contains(&args.window),
                "--window must be 0.04..60 seconds"
            );
            let start = (center - args.window / 2.0).max(0.0);
            (
                start,
                (center + args.window / 2.0).min(info.duration) - start,
            )
        }
        None => (
            args.start,
            args.duration.unwrap_or(info.duration - args.start),
        ),
    };
    let every = match (args.every, args.interval, args.around) {
        (None, None, Some(_)) => Some(2),
        (None, None, None) => bail!("choose --every N, --interval SECONDS, or --around TIME"),
        (every, _, _) => every,
    };
    let cast = args.cast.as_deref().map(load_cast).transpose()?;
    let focus = args.focus.load()?;
    let transcript = args
        .transcript
        .as_deref()
        .map(Transcript::load)
        .transpose()?;
    eprintln!("Hashing source for the description cache…");
    let source_sha256 = crate::cache::media_sha256(&info.path, args.cache_dir.as_deref())?;
    if let (Some(transcript), Some(path)) = (&transcript, args.transcript.as_deref()) {
        transcript.verify_media(path, &info.path, &source_sha256)?;
    }
    let target = crate::models::resolve_target("frame-description", args.model.as_deref())?;
    let model = target.reference();
    let root = crate::cache::cache_root(args.cache_dir.as_deref())?;
    let assets = root.join("assets");
    crate::cache::private_dir(&assets)?;
    let asset = assets.join(&source_sha256);
    crate::cache::private_dir(&asset)?;
    let image_dir = asset.join("exact-frames");
    crate::cache::private_dir(&image_dir)?;
    let temp = tempfile::Builder::new()
        .prefix("skywrite-describe-")
        .tempdir()?;
    // The frame size is part of the description's identity: rotated footage
    // is fitted by its displayed shape.
    let width = fit_width(&info);
    let extracted = review::extract_frames(
        &info,
        &FrameSelection {
            start,
            duration,
            every,
            interval: args.interval,
            width,
        },
        temp.path(),
        HARD_MAX_FRAMES,
    )?;
    let descriptions = root.join(FRAME_DIR);
    let answers = root.join(FOCUS_DIR);
    let cast_hash = cast_sha256(cast.as_ref())?;
    // Persist frame images by exact time so agents can view what was described.
    let mut work = Vec::new();
    for frame in &extracted {
        let key = frame_cache_key(&source_sha256, frame.time, width, &target, cast.as_ref())?;
        let image = image_dir.join(format!("{:015.6}-w{width}.jpg", frame.time));
        let bytes = std::fs::read(&frame.path)?;
        crate::io::atomic_write(&image, &bytes, true)?;
        let meta = json!({"kind":"frame","source_sha256":source_sha256,"time":frame.time,
            "provider":target.provider.name,"model":model,"prompt_version":FRAME_PROMPT_VERSION,
            "image":format!("fit{FRAME_FIT}-w{width}-q{JPEG_QUALITY}"),"cast_sha256":cast_hash});
        let focus_meta = focus.as_ref().map(|focus| {
            let mut meta = meta.clone();
            meta["kind"] = json!("focus");
            meta["of"] = json!("frame");
            meta["focus_prompt_version"] = json!(focus::FOCUS_PROMPT_VERSION);
            meta["focus_sha256"] = json!(focus.sha256);
            meta["instructions_sha256"] = json!(focus.instructions_sha256);
            meta["schema_sha256"] = json!(focus.schema_sha256);
            meta["focus_preview"] = json!(focus::preview(&focus.text));
            meta
        });
        work.push(FrameWork {
            time: frame.time,
            image,
            focus: focus
                .as_ref()
                .map(|focus| answers.join(format!("{}.json", focus_cache_key(&key, focus)))),
            general: descriptions.join(format!("{key}.json")),
            fresh: frame.path.clone(),
            meta,
            focus_meta,
        });
    }
    let needs: Vec<Need> = work
        .iter()
        .map(|item| {
            let general = read_cached(&item.general).is_some();
            let cached_focus = match (&item.focus, &focus, &item.focus_meta) {
                (Some(path), Some(focus), Some(meta)) => read_focus(path, focus, meta).is_some(),
                _ => false,
            };
            need(general, focus.is_some(), cached_focus)
        })
        .collect();
    let uncached = needs
        .iter()
        .filter(|n| matches!(n, Need::Plain | Need::Combined))
        .count();
    let would_request = needs.iter().filter(|n| **n != Need::Nothing).count();
    let fps = info
        .video()
        .and_then(|v| v.avg_frame_rate.as_deref())
        .and_then(|r| {
            let (n, d) = r.split_once('/')?;
            let (n, d) = (n.parse::<f64>().ok()?, d.parse::<f64>().ok()?);
            (d > 0.0 && n > 0.0).then_some(n / d)
        });
    let video_start = info
        .video()
        .and_then(|v| v.start_time)
        .unwrap_or(info.start_time)
        - info.start_time;
    if args.dry_run {
        let focus_counts = focus.as_ref().map(|_| {
            let missing = needs
                .iter()
                .filter(|n| matches!(n, Need::Combined | Need::FocusOnly))
                .count();
            json!({"cached": work.len() - missing, "to_answer": missing})
        });
        return finish(
            json!({"source":info.path,"model":model,"provider":target.provider.name,"frames":work.len(),"cached":work.len()-uncached,"to_describe":uncached,
                "focus":focus_counts,"would_request":would_request,
                "range":{"start":start,"duration":duration},"requests":0,
                "note":"Dry run: frames were extracted locally; no model requests were made. would_request counts the requests this exact command would make (one per frame needing a description and/or a focus answer; a corrective focus retry can add one)."}),
            args.output.as_deref(),
            args.overwrite,
        );
    }
    ensure!(
        would_request <= args.max_frames,
        "{would_request} frames need a request, above --max-frames {}; narrow the range, sample less often, or raise --max-frames deliberately (see --dry-run)",
        args.max_frames
    );
    let structured = if would_request > 0 {
        structured_outputs(focus.as_ref(), &target, args.cache_dir.as_deref())?
    } else {
        None
    };
    // The client is built only when something must be requested, so a fully
    // cached run needs no key.
    let api = if would_request > 0 {
        Some(api(&target.provider)?)
    } else {
        None
    };
    let asker = Asker::new(api.as_ref(), &target.model);
    let prompt = frame_prompt(cast.as_ref());
    let cast_context = focus_cast_context(cast.as_ref());
    let run = focus.as_ref().map(|focus| FocusRun { focus, structured });
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<Value>>> = Mutex::new(vec![None; work.len()]);
    // Finished frames by outcome: (described, cached, failed).
    let tally = Mutex::new((0usize, 0usize, 0usize));
    // An unreachable provider fails every request the same way: stop at once.
    let unreachable: Mutex<Option<String>> = Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..args.concurrency.min(work.len().max(1)) {
            scope.spawn(|| {
                loop {
                    if unreachable.lock().expect("lock").is_some() {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some(item) = work.get(index) else {
                        break;
                    };
                    let mut spend = Spend::default();
                    let general = read_cached(&item.general)
                        .map(|entry| annotate(&item.general, entry, &item.meta).description);
                    let cached_focus = match (&item.focus, &run, &item.focus_meta) {
                        (Some(path), Some(run), Some(meta)) => read_focus(path, run.focus, meta),
                        _ => None,
                    };
                    let cached_focus_value = |run: &FocusRun, answer: Value| {
                        json!({"instructions_sha256":run.focus.instructions_sha256,
                            "schema_sha256":run.focus.schema_sha256,"answer":answer,"cached":true})
                    };
                    let need = need(general.is_some(), run.is_some(), cached_focus.is_some());
                    let result = (|| -> Result<Value> {
                        let media = if need == Need::Nothing {
                            Value::Null
                        } else {
                            // The published cache is user-controlled; only send this run's extraction.
                            let bytes = std::fs::read(&item.fresh)?;
                            json!({"type":"image_url","image_url":{"url":format!("data:image/jpeg;base64,{}",STANDARD.encode(bytes))}})
                        };
                        let subject = Subject {
                            kind: "frame",
                            general_prompt: &prompt,
                            context: &cast_context,
                            media,
                            max_tokens: 1500,
                        };
                        let store = |description: &Value, usage: &Value| {
                            write_cached(&item.general, &CachedDescription {
                                version: 1,
                                model: model.clone(),
                                description: description.clone(),
                                usage: usage.clone(),
                                meta: Some(with_created(&item.meta)),
                            }, None)
                        };
                        let served = serve(&asker, need, &subject, run.as_ref(), &store, &mut spend)?;
                        let mut value = match (&general, served.description) {
                            (Some(description), _) => json!({"description":description,"cached":true}),
                            (None, Some(description)) => {
                                let mut value = json!({"description":description,"cached":false});
                                if let Some(error) = &served.description_cache_error {
                                    // Valid and paid for; it was not written to the cache.
                                    value["cache_error"] = json!(error);
                                }
                                value
                            }
                            // Not cached; a usable focus answer from the same request still is.
                            (None, None) => json!({"error":served.description_error.as_deref().unwrap_or("no description was produced")}),
                        };
                        if need != Need::Nothing {
                            value["cost_usd"] = spend.value();
                        }
                        if let Some(run) = &run {
                            value["focus"] = match (&cached_focus, served.focus) {
                                (Some(answer), _) => cached_focus_value(run, answer.clone()),
                                (None, Some(mut result)) => {
                                    if result.get("answer").is_some()
                                        && let (Some(path), Some(meta)) = (&item.focus, &item.focus_meta)
                                        && let Err(error) = write_cached(path, &CachedDescription {
                                            version: 1,
                                            model: model.clone(),
                                            description: result["answer"].clone(),
                                            usage: json!({"cost_usd": spend.value()}),
                                            meta: Some(with_created(meta)),
                                        }, Some(run.focus))
                                    {
                                        // The answer is valid and paid for; say it was not cached.
                                        result["cache_error"] = json!(format!("{error:#}"));
                                    }
                                    result
                                }
                                (None, None) => bail!("no focus answer was produced"),
                            };
                        }
                        Ok(value)
                    })();
                    let mut tally = tally.lock().expect("lock");
                    let value = match result {
                        Ok(mut value) => {
                            value["source_time"] = json!(item.time);
                            value
                        }
                        Err(error) => {
                            if let Some(down) = error.downcast_ref::<openrouter::Unreachable>() {
                                unreachable.lock().expect("lock").get_or_insert(down.to_string());
                            }
                            let mut value = json!({"source_time":item.time,"cost_usd":spend.value()});
                            // Keep what is cached. The frame fails only when it has no
                            // description; otherwise the failure belongs to the focus.
                            match &general {
                                Some(description) => {
                                    value["description"] = description.clone();
                                    value["cached"] = json!(true);
                                }
                                None => value["error"] = json!(format!("{error:#}")),
                            }
                            if let Some(run) = &run {
                                value["focus"] = match &cached_focus {
                                    Some(answer) => cached_focus_value(run, answer.clone()),
                                    None => focus_failure(
                                        run.focus,
                                        "focus_request_failed",
                                        format!("the request failed: {error:#}"),
                                        1,
                                    ),
                                };
                            }
                            value
                        }
                    };
                    // The same exclusive outcome the final counts use.
                    if value.get("error").is_some() {
                        tally.2 += 1;
                    } else if value["cached"] == true {
                        tally.1 += 1;
                    } else {
                        tally.0 += 1;
                    }
                    results.lock().expect("lock")[index] = Some(value);
                    let (described, cached, failed) = *tally;
                    let finished = described + cached + failed;
                    if finished.is_multiple_of(25) || finished == work.len() {
                        eprintln!(
                            "Finished {finished}/{} frames ({described} described, {cached} cached, {failed} failed)",
                            work.len()
                        );
                    }
                    drop(tally);
                }
            });
        }
    });
    let (spent, uncosted) = asker.spent();
    if let Some(message) = unreachable.into_inner().expect("lock") {
        // Requests that completed before the server went away may be billed.
        if spent > 0.0 || uncosted > 0 {
            bail!(
                "{message}. Earlier requests in this run completed and cost ${spent} ({uncosted} more without a reported cost); a rerun requests only frames without a cached description"
            );
        }
        bail!("{message}");
    }
    let mut frames = Vec::new();
    let (mut cached, mut described, mut failed) = (0usize, 0usize, 0usize);
    let (mut focus_cached, mut focus_answered, mut focus_failed) = (0usize, 0usize, 0usize);
    for (item, value) in work.iter().zip(results.into_inner().expect("lock")) {
        let mut value = value.context("frame result missing")?;
        // Each frame is exactly one of: cached, newly described, failed.
        if value.get("error").is_some() {
            failed += 1;
        } else if value["cached"] == true {
            cached += 1;
        } else {
            described += 1;
        }
        if let Some(result) = value.get("focus") {
            if result.get("error").is_some() {
                focus_failed += 1;
            } else if result["cached"] == true {
                focus_cached += 1;
            } else {
                focus_answered += 1;
            }
        }
        value["image"] = json!(item.image);
        value["frame"] = json!(fps.map(|fps| ((item.time - video_start) * fps).round() as i64));
        if let Some(transcript) = &transcript {
            let words: Vec<&str> = transcript
                .words
                .iter()
                .filter(|w| w.start - 0.25 <= item.time && item.time <= w.end + 0.25)
                .map(|w| w.text.as_str())
                .collect();
            value["words_near_frame"] = json!(words);
        }
        frames.push(value);
    }
    // Completed requests may have been billed even when no answer was usable.
    ensure!(
        failed < frames.len() || frames.is_empty(),
        "every frame description failed; first error: {}. Completed requests cost ${spent} ({uncosted} more without a reported cost); failed frames are not cached{}{}",
        frames[0]["error"],
        if frames
            .iter()
            .any(|f| f["focus"].get("answer").is_some() && f["focus"].get("cache_error").is_none())
        {
            " (usable focus answers were cached)"
        } else {
            ""
        },
        frames
            .iter()
            .find_map(|f| f["focus"]["cache_error"].as_str())
            .map(|error| format!("; a valid focus answer could not be cached: {error}"))
            .unwrap_or_default()
    );
    // Publication status decides the wording, never an assumption.
    let description_write_failure = frames.iter().find_map(|f| f["cache_error"].as_str());
    ensure!(
        focus.is_none() || focus_failed == 0 || focus_cached + focus_answered > 0,
        "every focus answer failed; first: {}. Completed requests cost ${spent} ({uncosted} more without a reported cost); failed focus answers were not cached; {}",
        frames
            .iter()
            .find_map(|f| f["focus"].get("error"))
            .unwrap_or(&Value::Null),
        match description_write_failure {
            None => "general descriptions were cached".to_owned(),
            Some(error) => format!("some general descriptions could not be cached: {error}"),
        }
    );
    let (data_policy, routing_note) = routing(&target)?;
    let mut notes = vec![
        "Each frame is described on its own from one still image; motion and timing between frames are not observed (use describe video or review sheet).",
        "Descriptions are cached per exact frame, model, prompt and cast; repeating or overlapping passes reuse them at no cost. Rerun the same command to retry failed frames only.",
        "cast_matches are advisory text-description matches, not identification. They fail when the deciding feature is not visible or people look alike; confirm by viewing the frame.",
    ];
    if focus.is_some() {
        notes.push("focus answers are cached separately per frame, model, cast, focus text and schema: a description cached by any earlier run is reused and only the focus is requested. Coordinates are model estimates (normalized 0..1, origin top-left, boxes [x1, y1, x2, y2]); view the frame image before relying on one.");
    }
    finish(
        json!({
            "source": info.path,
            "source_sha256": source_sha256,
            "model": model,
            "provider": target.provider.name,
            "prompt_version": FRAME_PROMPT_VERSION,
            "data_policy": data_policy,
            "routing_note": routing_note,
            "range": {"start": start, "duration": duration, "every": every, "interval": args.interval},
            "counts": {"frames": frames.len(), "cached": cached, "described": described, "failed": failed,
                "focus": focus.as_ref().map(|_| json!({"cached": focus_cached, "answered": focus_answered, "failed": focus_failed}))},
            "requests": asker.requests.load(Ordering::SeqCst),
            // Unknown when any new description came without a billed cost.
            "cost_usd": if uncosted == 0 { json!(spent) } else { Value::Null },
            "uncosted_requests": uncosted,
            "known_cost_usd": spent,
            "cast": cast.as_ref().map(|c| c.people.iter().map(|p| &p.name).collect::<Vec<_>>()),
            "focus": focus_report(focus.as_ref(), structured, would_request > 0),
            "frames": frames,
            "notes": notes,
        }),
        args.output.as_deref(),
        args.overwrite,
    )
}

fn video(args: &VideoArgs) -> Result<Value> {
    video_with_api(args, |provider| openrouter::Client::new(provider, None))
}

fn video_with_api(
    args: &VideoArgs,
    api: impl FnOnce(&crate::providers::Provider) -> Result<openrouter::Client>,
) -> Result<Value> {
    ensure!(
        args.duration.is_finite() && args.duration > 0.0 && args.duration <= MAX_VIDEO_SECONDS,
        "--duration must be in (0, {MAX_VIDEO_SECONDS}] seconds"
    );
    ensure!(
        (240..=1080).contains(&args.height),
        "--height must be 240..1080"
    );
    let info = media::probe_for_timing(&args.source)?;
    ensure!(info.has_video(), "source has no video stream");
    media::validate_range(&info, args.start, args.duration)?;
    if let Some(output) = &args.output {
        let mut inputs = vec![info.path.clone()];
        inputs.extend(args.cast.iter().cloned());
        inputs.extend(args.focus.inputs());
        crate::io::protect_inputs(output, &inputs)?;
        ensure!(
            args.overwrite || !output.exists(),
            "output already exists; pass --overwrite"
        );
    }
    let cast = args.cast.as_deref().map(load_cast).transpose()?;
    let focus = args.focus.load()?;
    let target = crate::models::resolve_target("video-description", args.model.as_deref())?;
    let model = target.reference();
    let root = crate::cache::cache_root(args.cache_dir.as_deref())?;
    let source_sha256 = crate::cache::media_sha256(&info.path, args.cache_dir.as_deref())?;
    // Never upscale: the clip is encoded at --height or the displayed height,
    // whichever is smaller, rounded down to an even number for H.264 4:2:0.
    // The encoded height is part of the cache key and the reported encoding;
    // `upright` marks clips sized by their displayed shape, so descriptions of
    // rotated footage encoded from its stored shape are not reused.
    let video_height = info
        .video()
        .and_then(|v| v.display_size())
        .map_or(args.height, |(_, h)| h);
    let height = (args.height.min(video_height) & !1).max(2);
    let encode = format!("h264-crf28-upright-h{height}-noaudio");
    let key = video_cache_key(
        &source_sha256,
        args.start,
        args.duration,
        &encode,
        &target,
        cast.as_ref(),
    )?;
    let cache = root.join(VIDEO_DIR).join(format!("{key}.json"));
    let meta = json!({"kind":"video","source_sha256":source_sha256,"start":args.start,"duration":args.duration,
        "encoding":encode,"provider":target.provider.name,"model":model,"prompt_version":VIDEO_PROMPT_VERSION,
        "cast_sha256":cast_sha256(cast.as_ref())?});
    let focus_cache = focus.as_ref().map(|focus| {
        root.join(FOCUS_DIR)
            .join(format!("{}.json", focus_cache_key(&key, focus)))
    });
    let focus_meta = focus.as_ref().map(|focus| {
        let mut meta = meta.clone();
        meta["kind"] = json!("focus");
        meta["of"] = json!("video");
        meta["focus_prompt_version"] = json!(focus::FOCUS_PROMPT_VERSION);
        meta["focus_sha256"] = json!(focus.sha256);
        meta["instructions_sha256"] = json!(focus.instructions_sha256);
        meta["schema_sha256"] = json!(focus.schema_sha256);
        meta["focus_preview"] = json!(focus::preview(&focus.text));
        meta
    });
    let shift = |mut description: Value| {
        // Model times are clip-relative; add source times alongside them.
        for list in ["shots", "events"] {
            if let Some(items) = description[list].as_array_mut() {
                for item in items {
                    for field in ["start", "end", "time"] {
                        if let Some(t) = item[field].as_f64() {
                            item[format!("source_{field}")] = json!(args.start + t);
                        }
                    }
                }
            }
        }
        description
    };
    let general = read_cached(&cache).map(|entry| annotate(&cache, entry, &meta).description);
    let cached_focus = match (&focus_cache, &focus, &focus_meta) {
        (Some(path), Some(focus), Some(meta)) => read_focus(path, focus, meta),
        _ => None,
    };
    let need = need(general.is_some(), focus.is_some(), cached_focus.is_some());
    let structured = if need != Need::Nothing && !args.dry_run {
        structured_outputs(focus.as_ref(), &target, args.cache_dir.as_deref())?
    } else {
        None
    };
    let (data_policy, routing_note) = routing(&target)?;
    let mut value = json!({"source":info.path,"source_sha256":source_sha256,"model":model,"provider":target.provider.name,"prompt_version":VIDEO_PROMPT_VERSION,
        "range":{"start":args.start,"duration":args.duration},"encoding":encode,"data_policy":data_policy,"routing_note":routing_note,
        "focus_request": focus_report(focus.as_ref(), structured, need != Need::Nothing && !args.dry_run),
        "notes":["Times in the model output are approximate clip-relative seconds; source_* fields add the range start.","Descriptions are model observations of a reduced-resolution clip; confirm important moments with review sheet.",
            "A focus answer is cached apart from the description: a cached description is reused and only the focus is requested. Times in it are clip-relative (add focus.clip_start); coordinates are model estimates, normalized 0..1 from the top-left."]});
    let focus_value = |answer: Value, cached: bool| {
        let focus = focus.as_ref().expect("focus");
        json!({"instructions_sha256":focus.instructions_sha256,"schema_sha256":focus.schema_sha256,
            "answer":answer,"cached":cached,"clip_start":args.start})
    };
    if args.dry_run {
        value["cached"] = json!(general.is_some());
        value["focus_cached"] = json!(focus.as_ref().map(|_| cached_focus.is_some()));
        value["would_request"] = json!(usize::from(need != Need::Nothing));
        value["requests"] = json!(0);
        value["note"] = json!(
            "Dry run: no request made. would_request is 1 when this exact command needs a request (a description, a focus answer, or both in one request); a corrective focus retry can add one more."
        );
        return finish(value, args.output.as_deref(), args.overwrite);
    }
    if need == Need::Nothing {
        value["description"] = shift(general.expect("cached"));
        value["cached"] = json!(true);
        value["cost_usd"] = json!(0.0);
        value["requests"] = json!(0);
        if let Some(answer) = cached_focus {
            value["focus"] = focus_value(answer, true);
        }
        return finish(value, args.output.as_deref(), args.overwrite);
    }
    let temp = tempfile::Builder::new()
        .prefix("skywrite-video-")
        .tempdir()?;
    let clip = temp.path().join("clip.mp4");
    let mut command = crate::process::args(&[
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-protocol_whitelist",
        "file,pipe",
        "-ss",
        &format!("{:.6}", args.start),
        "-t",
        &format!("{:.6}", args.duration),
        "-i",
    ]);
    command.push(info.path.clone().into_os_string());
    command.extend(crate::process::args(&[
        "-map",
        "0:v:0",
        "-an",
        "-vf",
        &format!("scale=-2:{height}"),
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-crf",
        "28",
        "-pix_fmt",
        "yuv420p",
        "-movflags",
        "+faststart",
    ]));
    command.push(clip.clone().into_os_string());
    crate::process::run("ffmpeg", &command, Duration::from_secs(600), 1024 * 1024)?;
    let size = std::fs::metadata(&clip)?.len();
    ensure!(
        size <= MAX_VIDEO_BYTES,
        "encoded clip is {size} bytes; inline requests are limited to {MAX_VIDEO_BYTES}. Shorten --duration or lower --height"
    );
    let prompt = video_prompt(cast.as_ref());
    let cast_context = focus_cast_context(cast.as_ref());
    let subject = Subject {
        kind: "clip",
        general_prompt: &prompt,
        context: &cast_context,
        media: json!({"type":"video_url","video_url":{"url":format!("data:video/mp4;base64,{}",STANDARD.encode(std::fs::read(&clip)?))}}),
        max_tokens: 2500,
    };
    let api = api(&target.provider)?;
    let asker = Asker::new(Some(&api), &target.model);
    let run = focus.as_ref().map(|focus| FocusRun { focus, structured });
    let store = |description: &Value, usage: &Value| {
        write_cached(
            &cache,
            &CachedDescription {
                version: 1,
                model: model.clone(),
                description: description.clone(),
                usage: usage.clone(),
                meta: Some(with_created(&meta)),
            },
            None,
        )
    };
    let mut spend = Spend::default();
    // Known spend and requests without a reported cost, for every error below.
    let spent_text = |asker: &Asker| {
        let (spent, uncosted) = asker.spent();
        format!("completed requests cost ${spent} ({uncosted} more without a reported cost)")
    };
    let served = serve(&asker, need, &subject, run.as_ref(), &store, &mut spend)
        .with_context(|| format!("{}; nothing new was cached", spent_text(&asker)))?;
    let focus_result = match (&focus, cached_focus, served.focus) {
        (None, _, _) => None,
        (Some(_), Some(answer), _) => Some(focus_value(answer, true)),
        (Some(_), None, Some(mut result)) => {
            if result.get("answer").is_some()
                && let (Some(path), Some(meta)) = (&focus_cache, &focus_meta)
                && let Err(error) = write_cached(
                    path,
                    &CachedDescription {
                        version: 1,
                        model: model.clone(),
                        description: result["answer"].clone(),
                        usage: json!({"cost_usd": spend.value()}),
                        meta: Some(with_created(meta)),
                    },
                    focus.as_ref(),
                )
            {
                // The answer is valid and paid for (see cost fields); it was not cached.
                result["cache_error"] = json!(format!("{error:#}"));
            }
            result["clip_start"] = json!(args.start);
            Some(result)
        }
        (Some(_), None, None) => bail!("no focus answer was produced"),
    };
    let (description, cached) = match (general, served.description) {
        (Some(description), _) => (description, true),
        (None, Some(description)) => (description, false),
        (None, None) => bail!(
            "{}; {}; the description was not cached{}",
            served
                .description_error
                .as_deref()
                .unwrap_or("no description was produced"),
            spent_text(&asker),
            match focus_result.as_ref() {
                Some(f) if f.get("answer").is_some() => match f["cache_error"].as_str() {
                    None => ", the focus answer was".to_owned(),
                    Some(error) =>
                        format!("; the valid focus answer could not be cached either: {error}"),
                },
                _ => String::new(),
            }
        ),
    };
    value["description"] = shift(description);
    value["cached"] = json!(cached);
    if let Some(error) = &served.description_cache_error {
        // Valid and paid for; it was not written to the cache.
        value["cache_error"] = json!(error);
    }
    let (known, uncosted) = asker.spent();
    value["cost_usd"] = spend.value();
    // cost_usd is null when any request reported no cost; the known part stays visible.
    value["known_cost_usd"] = json!(known);
    value["uncosted_requests"] = json!(uncosted);
    value["requests"] = json!(asker.requests.load(Ordering::SeqCst));
    value["encoded_bytes"] = json!(size);
    if let Some(result) = focus_result {
        if let Some(error) = result.get("error") {
            bail!(
                "the focus answer failed: {error}. {}; {}, the focus answer was not",
                spent_text(&asker),
                match &served.description_cache_error {
                    None => "the description was cached".to_owned(),
                    Some(cache) => format!("the description could not be cached ({cache})"),
                }
            );
        }
        value["focus"] = result;
    }
    finish(value, args.output.as_deref(), args.overwrite)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_send_fresh_extraction_after_cache_image_is_replaced() {
        use std::io::{Read, Write};
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("v.mp4");
        let output = std::process::Command::new("ffmpeg")
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
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let cache = dir.path().join("cache");
        let image = cache
            .join("assets")
            .join(media::file_sha256(&video).unwrap())
            .join("exact-frames")
            .join(format!("{:015.6}-w{FRAME_FIT}.jpg", 0.0));
        let args = FrameArgs {
            source: video,
            start: 0.0,
            duration: None,
            around: None,
            window: 1.0,
            every: Some(30),
            interval: None,
            cast: None,
            transcript: None,
            focus: FocusArgs::default(),
            model: Some("fixture/frames".into()),
            max_frames: 1,
            concurrency: 1,
            dry_run: false,
            cache_dir: Some(cache),
            output: None,
            overwrite: false,
        };
        let private = dir.path().join("private.txt");
        std::fs::write(&private, b"private input must never be sent").unwrap();
        let mut fixture = None;
        let mut expected = Vec::new();
        let result = frames_with_api(&args, |_| {
            expected = std::fs::read(&image)?;
            assert!(expected.starts_with(b"\xff\xd8"));
            // Swap the published cache after extraction. Reading it for the
            // request would disclose the private file even with atomic publication.
            #[cfg(unix)]
            {
                std::fs::remove_file(&image)?;
                std::os::unix::fs::symlink(&private, &image)?;
            }
            #[cfg(not(unix))]
            std::fs::write(&image, b"private input must never be sent")?;
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            listener.set_nonblocking(true)?;
            let base = format!("http://{}", listener.local_addr()?);
            fixture = Some(std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(std::time::Instant::now() < deadline, "no loopback request");
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("accepting loopback request: {error}"),
                    }
                };
                // Accepted sockets inherit O_NONBLOCK from the listener on macOS, not Linux.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut headers = Vec::new();
                let mut byte = [0];
                while !headers.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    headers.push(byte[0]);
                    assert!(headers.len() < 8192);
                }
                let headers = String::from_utf8(headers).unwrap().to_ascii_lowercase();
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length:")
                            .map(|n| n.trim().parse().unwrap())
                    })
                    .unwrap();
                assert!(length < 1024 * 1024);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).unwrap();
                let request: Value = serde_json::from_slice(&body).unwrap();
                let url = request["messages"][0]["content"][1]["image_url"]["url"]
                    .as_str()
                    .unwrap();
                let sent = STANDARD
                    .decode(url.strip_prefix("data:image/jpeg;base64,").unwrap())
                    .unwrap();
                let reply = serde_json::to_vec(&json!({"choices":[{"message":{"content":"{\"summary\":\"red frame\"}"}}],"usage":{"cost":0}})).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.len()
                )
                .unwrap();
                stream.write_all(&reply).unwrap();
                sent
            }));
            Ok(openrouter::Client::for_test(
                base,
                None,
                Duration::from_secs(5),
            ))
        });
        let sent = fixture.unwrap().join().unwrap();
        let report = result.unwrap();
        assert_eq!(report["counts"]["described"], 1);
        assert_eq!(sent, expected);
        assert_eq!(
            std::fs::read(&private).unwrap(),
            b"private input must never be sent"
        );
    }

    #[test]
    fn cache_keys_depend_on_frame_provider_model_and_cast_only() {
        let cast = Cast {
            version: 1,
            people: vec![CastMember {
                name: "Ralph".into(),
                description: "grey suit".into(),
            }],
        };
        let target = |reference: &str| {
            crate::providers::resolve(
                reference,
                &std::collections::BTreeMap::from([(
                    "local".to_owned(),
                    crate::providers::ProviderConfig {
                        base_url: crate::providers::OPENROUTER_BASE.into(),
                        key_env: None,
                        headers: Default::default(),
                        timeout_seconds: None,
                    },
                )]),
            )
            .unwrap()
        };
        let m = target("m");
        let a = frame_cache_key("ab", 1.0, 768, &m, None).unwrap();
        assert_eq!(a, frame_cache_key("ab", 1.0000001, 768, &m, None).unwrap());
        assert_ne!(a, frame_cache_key("ab", 1.04, 768, &m, None).unwrap());
        assert_ne!(
            a,
            frame_cache_key("ab", 1.0, 768, &target("n"), None).unwrap()
        );
        assert_ne!(a, frame_cache_key("ab", 1.0, 768, &m, Some(&cast)).unwrap());
        // A differently fitted frame (rotated footage) is a different request.
        assert_ne!(a, frame_cache_key("ab", 1.0, 432, &m, None).unwrap());
        // Same model name and base URL on another provider never shares entries.
        assert_ne!(
            a,
            frame_cache_key("ab", 1.0, 768, &target("local:m"), None).unwrap()
        );
        assert!(frame_prompt(Some(&cast)).contains("- Ralph: grey suit"));
    }
    #[test]
    fn general_cache_keys_are_pinned_and_focus_keys_extend_them() {
        // Computed independently from the key layout; a change here orphans
        // every existing description cache, focus or not.
        let target = crate::providers::resolve("vendor/model", &Default::default()).unwrap();
        let sha = "ab".repeat(32);
        let frame = frame_cache_key(&sha, 1.5, 768, &target, None).unwrap();
        assert_eq!(
            frame,
            "d8ac6a36988281ca233b50059b4093cd583236ac230292384610a9308498737b"
        );
        let video = video_cache_key(
            &sha,
            2.0,
            3.0,
            "h264-crf28-upright-h480-noaudio",
            &target,
            None,
        )
        .unwrap();
        assert_eq!(
            video,
            "ca843918427bebc0419bcc3bea421f747652c3a8632c989d541f8619df6b6cad"
        );
        let faces = focus::load(&["faces".into()], None, None).unwrap().unwrap();
        let hands = focus::load(&["hands".into()], None, None).unwrap().unwrap();
        let a = focus_cache_key(&frame, &faces);
        assert_ne!(a, frame);
        assert_ne!(a, focus_cache_key(&frame, &hands));
        assert_ne!(a, focus_cache_key(&video, &faces));
        let other = frame_cache_key(&sha, 2.0, 768, &target, None).unwrap();
        assert_ne!(a, focus_cache_key(&other, &faces));
    }

    #[test]
    fn need_follows_what_is_cached() {
        assert_eq!(need(true, false, false), Need::Nothing);
        assert_eq!(need(true, true, true), Need::Nothing);
        assert_eq!(need(true, true, false), Need::FocusOnly);
        assert_eq!(need(false, true, false), Need::Combined);
        assert_eq!(need(false, true, true), Need::Plain);
        assert_eq!(need(false, false, false), Need::Plain);
    }
}
