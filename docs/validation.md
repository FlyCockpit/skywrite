# Measured validation and limits

This page records what has been measured, and what has not. It is dated
evidence, not a compatibility promise for every model, platform or S3 provider.
Later source changes need fresh validation.

## Automated checks

The normal Rust test suite (`cargo test --locked`) runs offline against media
generated locally with FFmpeg, together with formatting and all-target Clippy
with warnings denied. Continuous integration runs it on Linux and macOS and
checks that the crate still compiles with the minimum supported Rust version
(1.91). The minimum-version check establishes compilation, not a second full
runtime test matrix. None of this claims production readiness or actual Final
Cut/Resolve import.

## Live provider checks

During development, live behavior was checked with explicit, spending-capped
OpenRouter requests and a dedicated Cloudflare R2 test bucket. These are
measured examples from 2026-10-30, not guarantees:

| Executed check | Observed result |
| --- | --- |
| Image generation and reference edit | FLUX 3 Image Klein returned JPEG; PNG publication converted the bytes correctly |
| H3 Max video | 832×480, 24 fps, about 5.184 s |
| Exact MiniMax H3 video | 2560×1440, 24 fps, about 5.167 s, AAC 32-kHz stereo |
| Dedicated video upscale | 832×480 became 1664×960; about 24 ms duration difference, below one frame |
| Kokoro narration | `hexgrad/kokoro-82m`, `af_heart`: decoded 9.945 s, 24-kHz mono MP3 |
| Lyria 3 Clip music | The CLI retained the MP3 and published 24.921 s of 48-kHz stereo WAV |
| Narration word timing | ASR recovered the clock-drift explanation with 22 word timestamps |
| Completed audio/video replay | Saved results republished without keys or network, with no new paid submission |

The whole set of live checks, including rejected and protocol probes, cost about
US$1.27 in OpenRouter usage at the time. Lyria Pro was catalog-discovered but not
live-generated. Music duration, vocals and artistic suitability require listening
review; a requested format can differ from the provider's actual container, so
successful publication requires decoding and any necessary conversion. MIDI and
text-score rendering are not supported. Image upscaling is local Lanczos
resizing; the tested AI upscaler is for video.

The live Cloudflare R2 check used Apache Arrow’s `object_store` 0.14.2 backend.
It exercised signed hash-addressed MP4 upload and HEAD reuse, public HTTPS
byte/hash identity, an actual upscale using that R2 source, and deletion only
after a verified local result. Cleanup was deferred while another local receipt
was unfinished; a configuration mismatch preserved the result and source, and
restoring the settings allowed cleanup on resume. Paged purge preserved active
sources and foreign or unmarked objects while removing eligible inputs.

Two isolated machine configurations also exercised named output publication,
name/hash discovery, pagination, Unicode names, empty files, explicit conditional
replacement, a 4-MiB-plus-37-byte range download, checksum/no-clobber behavior,
and protection of the local storage configuration. No credentials, account
endpoints or test-bucket URLs are stored in the repository.

Not established: Amazon S3 itself or other compatible services; maximum 512-MiB
transfers and slow-network throughput; forced simultaneous stale-ETag writers;
every crash/power-loss window; cross-computer input-job leases; versioned-bucket
reclamation or CDN invalidation. Input cleanup protects local job receipts only;
use separate project prefixes when independent computers may still need a source.
Shared outputs are outside the temporary-input purge namespace.

## Reproduce public-media checks

The normal Rust test suite works offline and uses generated media. Public media
and paid calls are opt-in. Run from the repository root:

```sh
cargo test --locked --offline
python3 scripts/validate-public-media.py prepare
python3 scripts/validate-public-media.py
```

`prepare` downloads short AMI Meeting Corpus excerpts through FFmpeg HTTP seeks;
it needs network access and FFmpeg. The default directory is
`/tmp/skywrite-public-validation`, outside the repository. It stores source URLs,
derived-file SHA-256 hashes, and attribution there. Existing files are reused.
The default `check` mode runs real microphone alignment offline, checks expected
offset and clock scale, and validates existing analysis/edit artifacts. It removes
`OPENROUTER_API_KEY` from child processes. If live transcript artifacts exist, it
also proves that the exact cached transcription works without a key.

The source is AMI Consortium's ES2002a meeting: Headset-0, Array1-01,
Mix-Headset, and Closeup1 video. The audio excerpts cover original seconds 30..150;
the video covers 30..60. AMI's [download page](https://groups.inf.ed.ac.uk/ami/download/)
licenses signals and transcriptions under
[CC BY 4.0](https://creativecommons.org/licenses/by/4.0/). Preserve the generated
attribution file with distributed excerpts or rendered demonstrations. The two
derived microphone fixtures change the array sample rate to 16160 or 16002 Hz,
resample to 16000 Hz, then prepend 2 or 1 seconds of silence.

To explicitly permit bounded live requests:

```sh
python3 scripts/validate-public-media.py live --allow-paid --paid-budget 0.25
```

Set `OPENROUTER_API_KEY` in the environment or pass `--key-file /private/path`.
Never add credentials to the repository. Use a server-side key spending cap.
The script records key usage before/after and checks observed spending before
each request; this is a stop threshold, not an atomic provider billing cap.
There are four bounded operations: 30 seconds of word transcription, four-frame
video understanding, 30 seconds of audio understanding, and an edit suggestion.
The current model is configurable with `--model`; transcription uses the CLI's
default. Existing successful output files are reused, exact API responses are
cached, and failed paid requests are never retried automatically. Inspect saved
stderr and response-cache evidence before deliberately changing a request.

On 2026-10-30, a development build completed this live validation using
`openai/whisper-large-v3` and `google/gemini-3.5-flash-lite`:

| Executed check | Observed result |
| --- | --- |
| Headset versus table microphone | offset −0.979 ms; scale +54.64 ppm; 11 anchors over 112 s |
| Array sped up 1.01×, delayed 2 s | expected offset −2.02 s / scale 1.01; error 2.171 ms / 64.92 ppm |
| Array sped up 125 ppm, delayed 1 s | expected offset −1.000125 s / scale 1.000125; error 0.594 ms / 50.60 ppm |
| Real speech transcription | 77 provider-timestamped words, including the meeting introduction and participant roles |
| Visual understanding | Remote control, glasses, plaid shirt and listening posture independently checked against sampled frames |
| Audio-only understanding | Kick-off meeting topic agrees with the AMI manual annotation |
| Suggested edit and render | Validated 8 s, two video clips and one 5 s audio overlay; 384×288, 25 fps, 200 frames, H.264/AAC |

Alignment quality was reported `limited`: sparse overlapping speech and acoustic
propagation constrain the result. The checks allow 20 ms offset error and 250 ppm
scale error, rather than treating public microphones as exact electronic copies.
The rendered output has audible-source samples during its first 5 seconds and a
silent final 3 seconds; the sampled rendered frame depicts the remote demonstration.
It is an editorial cutaway, not measured lip synchronization.

Seven deliberately issued requests cost under US$0.01 in total, matching the
key's usage delta. This includes two rejected outputs and a low-signal probe:
the nearly silent individual headset produced an unreliable “Thank you” result;
the initial array transcription contained slightly unsorted provider word starts
and was rejected; an initial suggested edit copied `$schema` metadata and was
rejected without writing an edit. An explicit brief requiring an edit instance
without schema metadata then succeeded. Those observations are provider-quality
and integration limits, not evidence of guaranteed transcription/editorial
accuracy. No timestamp was fabricated. The source-mix introduction and role names
were checked against AMI manual word annotations; timestamps remain ASR estimates.

The provider word-order issue was subsequently fixed in ingestion. Replaying the
identical failed array response from cache without an API key succeeded with all
42 original word text/start/end values preserved, chronological starts, and an
explicit reordering diagnostic. This replay checks the actual saved provider
response; it does not issue or charge another request. The whole transcript text is
trimmed at its outer whitespace; the preservation claim applies to provider word
text/start/end triples, not byte identity of the whole text.

This dated evidence does not certify later source changes; rerun the script
after changes that affect these paths.

The offline regression suite also exercises native Takumi HTML/JSON rendering
with an empty executable PATH, local font/image preservation and PNG/WebP/JPEG
output.

On Linux, subprocess regressions use controlled local child process groups to
check ordinary SIGINT/SIGTERM cancellation, deadlines, bounded capture, reader
joins and repeated concurrent failure/success cleanup. The supervisor keeps one
signal notification thread per process; calls do not accumulate readers or file
descriptors. Signal exit keeps the normal signal status, after active helper
groups have been killed and direct children reaped. SIGKILL, machine failure and
descendants that deliberately leave their assigned process group are outside
this cleanup guarantee. Process-lifecycle behavior on other operating systems
requires separate validation.
