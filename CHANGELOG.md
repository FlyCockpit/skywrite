# Changelog

All notable changes to Skywrite are documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 1.0.0 - Unreleased

### Added

- First public release of Skywrite, licensed under MIT, copyright 2026 FlyCockpit LLC.
- JSON-first media inspection, frame sampling and optional OpenRouter analysis and transcription with word timestamps.
- Multi-recording synchronization by audio waveform or transcript, with drift estimates and quality reporting.
- Edit JSON format version 1, with validation, lint and review tools and a native FFmpeg MP4 renderer.
- Required transcript content fingerprints for transcript-based caption tracks, `describe frames --transcript` and `review lint --transcript`, matching analysis and edit suggestions; byte-identical media copies are accepted, and media SHA-256 hashes are cached locally.
- Export to FCPXML, Premiere XML (xmeml), Hyperframes, Remotion and FFmpeg scripts; import Premiere/FCP 7 XML and experimental `.prproj` files.
- Native HTML or JSON still graphics through embedded Takumi, with PNG, JPEG and WebP output.
- Optional image, video, narration and music generation and upscaling through OpenRouter, with durable generation projects and optional S3/R2 uploads.
- Agent skills, saved style prompts, task model defaults and `skywrite nit` feedback capture.
- Release binaries for Linux (static musl: x86_64, aarch64; no glibc requirement) and macOS (Apple silicon, Intel) on GitHub Releases, with SHA256SUMS and third-party license notices; archives include the embedded Geist font license as `fonts-OFL.txt`, and Linux archives include the statically linked musl libc's notice as `musl-COPYRIGHT.txt`.
- `external_sources` in every render and export result lists sources outside the edit file's directory; symlinked and special-file sources are refused, in edits and in imported XML. Edit and XML files are documented as trusted input.
