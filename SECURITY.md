# Security Policy

## Reporting a vulnerability

Please report security vulnerabilities **privately** through GitHub private vulnerability reporting:

<https://github.com/FlyCockpit/skywrite/security/advisories/new>

Do not open public issues, discussions or pull requests for security problems. We aim to acknowledge reports within 72 hours.

In scope: the `skywrite` binary and release binaries built from this repository; handling of untrusted media, XML, `.prproj` files and HTML templates; OpenRouter API key handling; and S3/R2 credential handling. Leaks of credentials or private media, unsafe input processing and unexpected network access are especially important.

## Supported versions

Only the latest release receives fixes.

## Design stance

Skywrite processes local media using FFmpeg and ffprobe, and renders HTML templates with embedded Takumi. FFmpeg is a system dependency: keep FFmpeg and ffprobe at the latest point release of a maintained FFmpeg branch through your operating system or package manager (FFmpeg lists fixed versions at https://ffmpeg.org/security.html). FFmpeg decoders regularly receive security fixes, and they parse every untrusted input. Treat untrusted files and templates with care and use isolation appropriate to your environment.

Media inputs are passed to FFmpeg/ffprobe with `-protocol_whitelist file,pipe`. Containers that follow references named inside the file are rejected at probe time: playlists and manifests (`hls`, `dash`, `imf`, `concat`, `webm_dash_manifest`), session descriptions and streams (`sdp`, `sap`, `rtp`, `rtsp`), scripts (`avisynth`, `vapoursynth`), disc structures (`dvdvideo`, `bluray`) and sidecar formats (`mlv`, `vobsub`). Skywrite never decodes, renders or exports such inputs. ffprobe opens the file once to identify its format, and a `-format_whitelist` built from the installed FFmpeg's demuxers (minus the rejected ones) stops it before it reads the header or any file the input names. Network protocols are blocked throughout by the protocol whitelist. Later FFmpeg runs on the same path rely on that probe and do not repeat the format whitelist. Exported FFmpeg and Python render scripts run outside Skywrite and do not repeat the check.

Edit JSON, Premiere XML and `.prproj` files are trusted input, like scripts: they choose which local files are read, rendered and copied, and absolute or `../` source paths are allowed. Their parsing is still hardened (size limits, no external entities), and media, LUT, caption and transcript files must be regular files: symlinks, directories, FIFOs, devices and sockets are refused. Project export opens sources without following symlinks and refuses a source whose path changed after validation; rendering passes the validated paths to FFmpeg, so someone who can rewrite your media directories while a render runs can still substitute files. Every render and export result lists sources outside the edit file's directory as `external_sources`; Hyperframes and Remotion projects copy those files into the deliverable, so check the list before sharing a project built from someone else's edit.

## Security controls

- Model calls require explicit OpenRouter credentials; do not include API keys, S3/R2 credentials or private media in public issues, logs or fixtures.
- Ordinary integration tests use synthetic fixtures and loopback servers. The `tests/fixtures/network/*.pem` and `*.key` files are fake **TEST ONLY** credentials for those loopback tests, not production secrets.
- SHA-pinned GitHub Actions, and `cargo deny` checks of licenses, sources, bans and RustSec advisories in CI.
