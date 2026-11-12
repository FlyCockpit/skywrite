# Contributing to Skywrite

Thanks for your interest in improving Skywrite. Please open an issue to discuss larger changes before you start on them.

Everyone taking part is expected to follow the [Code of Conduct](./CODE_OF_CONDUCT.md).

## Reporting security issues

Do not open public issues or pull requests for security problems. Report them privately as described in [SECURITY.md](./SECURITY.md).

## Licensing

Skywrite is licensed under the MIT License (see [LICENSE](./LICENSE)), copyright 2026 FlyCockpit LLC. Contributions are accepted under the same license: by submitting a contribution you agree that it is licensed to the project and its users under the MIT License (inbound = outbound). There is no separate contributor license agreement.

## Development setup

You need Rust 1.91 or newer (2024 edition), installed with [rustup](https://rustup.rs). The code is Unix-only (Linux and macOS). Install FFmpeg 5.1 or newer with `libx264` and AAC encoding, and `ffprobe` on your `PATH`. Integration tests also need `python3` for local loopback fixture servers. Run `skywrite doctor` to check the media tools. The [README](./README.md#development) lists the common commands.

Record all testing friction, bugs and missing features with `skywrite nit 'description'`.

## Checks

Before opening a pull request, run:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

CI checks formatting, Clippy and Rust 1.91 compatibility, runs tests on Linux and macOS, and checks licenses, bans and sources with `cargo deny`. RustSec advisories run weekly and on manual dispatch (see [deny.toml](./deny.toml)).

Tests must not need external network access, paid accounts or the real `~/.config` or `~/.cache`. Use temporary directories, synthetic media and fake providers served over loopback. Relocate personal files with `SKYWRITE_HOME` and cache files with `SKYWRITE_CACHE_DIR`; isolate `HOME`, `XDG_CONFIG_HOME` and `XDG_CACHE_HOME` as well. Paid/live OpenRouter, S3/R2 and public-access checks are explicit manual opt-ins; leave their credentials and fixture environment variables unset and do not run ignored tests in CI.

## Pull requests

- Keep each pull request focused on one change, and describe what it does and why.
- Keep the diff to the task. Unrelated refactors, reformatting or dependency bumps belong in their own pull request, and a new dependency should be discussed first.
- Add or update tests for behavior you change.
- Update the README, `docs/`, the agent skill in `skills/` and bundled skills in `assets/skills/` when your change affects them.
- Fill in the pull request template, including the checks you ran.

## Releases

Distribution is through GitHub Releases; there is no crates.io publish or Homebrew workflow. A `v` tag must match the package version and its changelog entry. Release archives contain the binary, MIT license, README, changelog and `THIRD_PARTY_LICENSES.html`. They also include the embedded Geist font's SIL Open Font License as `fonts-OFL.txt`. Linux archives add the statically linked musl libc's copyright notice as `musl-COPYRIGHT.txt`. Releases provide Linux and macOS binaries for x86_64 and aarch64, with `SHA256SUMS`. Linux binaries are static musl builds (`*-unknown-linux-musl`, built with `musl-gcc` from Ubuntu's `musl-tools`), so they have no glibc requirement; the release job fails if one is dynamically linked. FFmpeg and ffprobe remain system dependencies.
