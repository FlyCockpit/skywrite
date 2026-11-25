# AGENTS.md

> Rules and context for AI coding agents working in Skywrite, a JSON-first Rust CLI for agent-driven video editing (inspect, sync, edit, render, NLE interchange, optional OpenRouter generation).

## STOP: the rules that matter most

1. **Prerelease: no compatibility code.** There are no users to migrate. Change formats, fields and flags directly; do not add aliases, fallbacks, deprecated paths, version shims or "old behaviour" switches. Update every caller, test and doc in the same change.
2. **Docs must match code.** A change to behaviour, output fields, flags or limits updates, in the same change, `README.md`, `docs/`, the agent skill `skills/skywrite/SKILL.md`, the bundled skills in `assets/skills/` (compiled into the binary) and `CHANGELOG.md`. If a doc and the code disagree, report both; don't silently pick one.
3. **Explicit state.** Results say what was measured and how sure it is (`quality`, `method`, `warnings`, limits used, such as `strong_p95_limit_seconds`). Never infer, guess or silently round something the user relies on; do not make network or paid calls the user didn't request.
4. **Measured, not assumed.** Timing, sync and media claims are checked against real FFmpeg output with synthetic fixtures (tones, clicks, known offsets and rates). Write measured numbers into the docs, with the FFmpeg version when it matters.
5. **Never pass user text through a shell or raw filter graph.** Subprocesses go through `src/process.rs` (bounded, no shell). Filter strings contain only numbers and fixed names.
6. NEVER commit secrets or print credential values; never run `sudo`; never run paid/live tests (ignored tests, OpenRouter, S3/R2) unless the user asks.
7. Stop and ask when you need a new dependency, a change is irreversible, or the request is ambiguous in a way that changes a public format.

## Build and test

Requires Rust 1.91+ (2024 edition), cmake and a C++ compiler (the on-device transcription engine, transcribe.cpp; `--no-default-features` leaves it out), FFmpeg 5.1+ with libx264/AAC, `ffprobe`, and `python3` (loopback test servers and exported render scripts). Unix only. The first build compiles the engine's C++ (several minutes). Static musl builds need a musl C++ toolchain (Alpine's `g++`); `build.rs` links its static `libstdc++.a`, and `TRANSCRIBE_CMAKE_ARGS=-DCMAKE_POSITION_INDEPENDENT_CODE=ON` is required (see `.github/workflows/release.yml`).

```sh
cargo build --release                               # target/release/skywrite
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked                                 # unit + integration (real FFmpeg); the landing gate
scripts/verify-fast                                 # fmt + Clippy + all tests except scripts/verify-fast.skip
cargo test --locked --lib -- sync::                 # name filter (also matches audio_sync::)
cargo test --locked --test export_pipeline          # one integration file
cargo deny check                                    # licenses, bans, sources, advisories
```

When another heavy job shares the machine, cap parallelism (`cargo build/test -j4`) and run under `nice -n 10`. A test that fails only on timing is re-run alone before it counts as a failure.

Tests that need FFmpeg's `rubberband` filter check `skywrite::stretch::rubberband_available()` and skip with an `eprintln!("skipping ...")` when it is missing; `tests/stretcher_cli.rs` simulates a build without it through a PATH shim.

Tests must not use the network, paid accounts or the real `~/.config`/`~/.cache`: use temp dirs, synthetic media generated with `ffmpeg -f lavfi`, loopback fakes, and `SKYWRITE_HOME`/`SKYWRITE_CACHE_DIR`. Fixtures stay tiny; generate media in the test rather than committing it (`tests/fixtures/` holds only small XML and the loopback network servers, certificates and data).

## Architecture map

| Area | Files |
|---|---|
| CLI entry, command wiring, `skywrite schema`; config commands | `src/main.rs`; `src/config_cli.rs` |
| Media probing, exact decoded timing, color | `src/media.rs`, `src/timing.rs`, `src/color.rs` |
| Sync (mapping `reference = offset + scale * asset`) | `src/sync.rs` (transcripts, shared fit and quality), `src/audio_sync.rs` (waveform/envelope) |
| Edit format: parse, validate, frame grid | `src/timeline.rs` (`ValidatedEdit` is the only input to backends) |
| Render and exports | `src/export.rs` (FFmpeg MP4 + Python script), `src/export/{fcpxml,xmeml,project,media_url}.rs` |
| NLE import | `src/import.rs` |
| Audio finishing, room tone | `src/audio.rs`; device listing and `record-bed` capture in `src/capture.rs` |
| Clip/tempo speed filters and the `--stretcher` choice (rubberband detection, latency calibration) | `src/stretch.rs` (`SpeedFilters`, resolved once per command; used by the renderer, FFmpeg script and `audio tempo`) |
| On-device transcription (`ondevice:` provider: pinned model download/remove, transcribe.cpp engine) | `src/ondevice.rs`, `build.rs` (static musl C++ runtime) |
| Analysis, transcripts, captions, review, describe | `src/analysis.rs`, `src/transcript.rs`, `src/captions.rs`, `src/review.rs`, `src/describe.rs`, `src/verify.rs` |
| Generation (durable paid jobs), models, OpenRouter, public-URL diagnostics | `src/generation.rs`, `src/project.rs` (+ `src/project/tests.rs`), `src/audio_generation.rs`, `src/speech_split.rs`, `src/models.rs`, `src/openrouter.rs`, `src/public_access.rs` |
| Storage, outputs, cache, digests (SHA-256 ids and cache keys), workspace, fonts, images | `src/storage.rs`, `src/outputs.rs`, `src/cache.rs`, `src/digest.rs`, `src/workspace.rs`, `src/fonts.rs`, `src/image.rs` |
| Process and file safety | `src/process.rs` (bounded subprocesses), `src/io.rs` (atomic writes, input protection) |
| User docs and skills | `docs/`, `README.md`, `skills/skywrite/SKILL.md`, `assets/skills/*.md` |

## Structure lint

- [`deslop-please`](https://github.com/deterministic-machines/deslop-please) lints code structure (module cycles, tangles, field leaks, shallow modules, long files, wide directories); fix what it finds from first principles, never by moves that only hide a finding.
- Run it at the root: `deslop-please --allow .deslop-please.allow`.
- `.deslop-please.allow` lists existing debt that is muted: fixing an entry never fails, a new finding fails.
- `deslop-please --update-allow .deslop-please.allow` regenerates the file and drops any comments in it.

## Conventions

- Every command prints one JSON object on success and `{"error":...}` with a nonzero exit on failure; progress goes to stderr.
- Times are seconds as `f64`; the output timeline is frame-rounded (`start`), source positions (`source_in`) are not. `speed` is source seconds per output second. Audio renders at 48 kHz.
- Inputs are never modified; outputs are written atomically and need `--overwrite` to replace a file.
- Every external process, decode, download and loop has an explicit bound (time, bytes, count).
- Keep the diff to the task; mention unrelated problems instead of fixing them in passing.

## Verification before handing back

1. `cargo fmt --check` and `cargo clippy --all-targets --locked -- -D warnings`.
2. The tests for the modules you touched, then `cargo test --locked`. While iterating, `scripts/verify-fast` (about a minute instead of two) runs fmt, Clippy and every test except the slow media-correctness tests named in `scripts/verify-fast.skip`; it never replaces the full `cargo test --locked` before landing. Only slow tests may be listed, and none that guards security, teardown, data preservation, overwrites, bounds, caches or paid requests unless another test that is not listed checks the same guard; `tests/verify_fast_skips.rs` checks the list. Report any failure with its output, and say which failures were timing-only and passed when re-run alone.
3. For media or timing changes, a real run on synthetic media (render, then measure with `ffmpeg` decode or `skywrite sync`), with numbers before and after.
4. Check that the docs and skills you were required to update (rule 2) describe the new behaviour.
