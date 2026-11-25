# Local audio finishing

`skywrite audio` uses installed FFmpeg/ffprobe and makes no model calls. The finishing commands (`normalize`, `tempo`, `loop`, `duck`, `finish`, and `roomtone` below) emit JSON with source hashes, first-audio decoded timing, sample rounding, format and actual final decoded duration. Their outputs are 48-kHz stereo PCM16 WAV or 192-kbit/s MP3, selected by `.wav`/`.mp3`; bytes are encoded and checked, not renamed. A mono input is converted to stereo at -3 dB per channel (equal power) when it is first decoded, before any level target, which then applies to the stereo result; later steps (looping, ducking, fades, MP3 encoding) change levels as usual. For example, `audio loop` of a mono cue at -31.5 dBFS RMS measured -34.5 dBFS per channel (FFmpeg 8.0.1). `roomtone` converts its speech and bed the same way. A `--sidechain` is not converted: it only controls the gain. `inputs` and `record-bed` (mono 48-kHz PCM16 WAV) are described under [Recording room tone](#recording-room-tone).

```sh
# Normalize speech with two-pass integrated loudness and a true-peak ceiling.
skywrite audio normalize narration.mp3 -o narration.wav --lufs -18 --peak-db -2
# Or normalize sample peak alone, without a LUFS target.
skywrite audio normalize narration.wav -o peak.wav --peak-db -6
# A multiplier above one speeds up speech; pitch is kept for changes beyond 0.1%
# (rubberband when FFmpeg has it; --stretcher auto|rubberband|atempo).
skywrite audio tempo narration.wav -o faster.wav --tempo 1.1
# Repeat one cue with overlapping crossfades, then fade to an exact endpoint.
skywrite audio loop cue.wav -o bed.wav --duration 180 --crossfade 2 \
  --lufs -24 --peak-db -4 --fade-in 1.5 --fade-out 5
# Narration controls attenuation. The output contains only the ducked music.
skywrite audio duck bed.wav -o ducked.wav --sidechain narration-stem.wav \
  --threshold 0.035 --ratio 4 --attack-ms 15 --release-ms 600 --duration 180
# One composable command can trim, adjust speed/levels, fade and pad.
skywrite audio finish speech.mp3 -o finished.wav --start 0.1 --length 12 \
  --tempo 1.05 --lufs -18 --peak-db -2 --fade-in 0.025 --duration 14
```

The five commands `normalize`, `tempo`, `loop`, `duck` and `finish` accept the same typed options (`roomtone` has its own, below). `normalize` defaults to -18 LUFS when neither level target is supplied; `tempo` requires `--tempo`, `loop` requires `--duration`, and `duck` requires `--sidechain`. `finish` applies only the requested operations. Use `finish --loop --duration ...` to compose looping with other steps. `--crossfade 0` uses direct concatenation. Crossfades must be shorter than half the selected, tempo-adjusted loop; each overlap reduces the repeat stride. Each repeat is faded in and out with a linear gain, delayed to its start and summed with the others, so every output sample is the exact sum of the overlapping copies (FFmpeg's `acrossfade` is not used: before FFmpeg 8.0 it could end a loop of a short cue early and pad the rest with silence). A target shorter than the cue trims it without repeating. This first version repeats one cue; assemble different cues on edit tracks or prepare their sequence separately.

Processing order is source selection, tempo, normalization, loop/crossfade, narration-driven ducking, final padding/trim, then fades. Tempo 1 passes the 48-kHz samples through unchanged. Within 0.1% of 1 tempo resamples, which is sample-exact and shifts pitch by under 1.8 cents; larger changes keep pitch with the time-stretcher chosen by `--stretcher`, as in edit renders ([edit format](edit-format.md)). `auto` (default) uses FFmpeg's `rubberband` when the build has it, latency-compensated by a per-speed calibration so audio lands within about 6 ms (8 ms at 0.125x) and the output has exactly `round(selected samples / tempo)` samples. Without rubberband it falls back to `atempo` with a warning: 3-36 ms early at 0.5-2x and up to about 60 ms early below 0.5x, and its output is short (for one second of source: 1511 samples at 0.8, 7680 samples at 0.25). `rubberband` fails if FFmpeg lacks it; `atempo` always uses atempo. The JSON reports the choice in `audio_stretcher`. Normalization targets the prepared selection. Later crossfades, ducking, fades and MP3 encoding can change final loudness or peaks; the report does not claim a final LUFS target was verified. Two-pass loudnorm may use its dynamic fallback if linear normalization cannot satisfy the requested range/ceiling. Digital silence stays silent and reports that the level target is unachievable. Peak-only normalization targets decoded sample peak, rather than intersample true peak.

`--start` and `--length` refer to decoded samples of the first audio stream, starting at its first sample, irrespective of container timestamp origins or gaps. The output timeline starts at zero. The JSON retains container/stream start timestamps and the original source hash: preserve these when mapping back to source presentation time. At constant tempo, source sample time equals the reported selected start plus output time multiplied by tempo; loops repeat that mapping. Narration's first decoded sample aligns to output zero, with no implicit offset. Ducking gives the same samples on every run: both inputs reach `sidechaincompress` in fixed 1024-sample frames, because its output depends on how they are split into frames, which on FFmpeg 7.0 and later follows thread timing. With the same music and narration, the ducking step measured identical on FFmpeg 5.1.2 through 9.1; earlier steps such as `--tempo` can still differ between builds (for example rubberband on one, atempo on another). To duck a film, supply a narration stem with its deliberate timeline silence already included. Short narration is padded with silence so the music continues and recovers.

Source selection, final duration and fades round to the nearest 48-kHz sample. With atempo, tempo output duration follows the actual filter output, not a promise of exact arithmetic division; rubberband output is trimmed to the exact sample count. `--duration` always pads silence and then trims to the requested sample count, even if a previous filter produced a short tail (for example 179.2 seconds for a 180-second film). A fresh full decode of the encoded output must match that sample count before publication. MP3's container duration may include encoder delay/padding; the decoded gapless sample count is the reported endpoint. Prefer WAV for timeline stems.

Existing outputs require `--overwrite`. Every consumed input, including the sidechain, is protected against output replacement; original media is never edited. Processing uses owned temporary files beside the output and atomically publishes only a complete, freshly decoded result. Failure or ordinary cancellation preserves any previous output. No paths or user text enter a shell or raw filter graph. Paths beginning with a dash work as positional arguments after `--`, or as absolute paths.

Resource limits are one GiB per source/output, one hour of decoded source/final audio, eight input channels, 192-kHz input, 128 loop repetitions, tempo 0.25–4, crossfades 0–30 seconds and finite validated numeric parameters. Processing stages share a 15-minute deadline, each supervised FFmpeg process is capped at ten minutes and one MiB diagnostic output. Exact timing uses the existing bounded six-hour/ten-minute decoder and bounded cache-lock waits; its wall time is additional to processing stages. No decoded PCM is captured on stdout. Temporary disk usage can include several WAV intermediates (up to several GiB at the maximum duration).

For an existing Pip-style film, finish each voice section, assemble voice placements using edit audio offsets, then loop a music cue and duck it against the assembled narration stem. Keep the measured per-section durations with the edit instead of inferring them from MP3 metadata. See [audio generation](audio-generation.md) and [edit format](edit-format.md).

Final source and sidechain hash checks enforce the one-GiB input limit before
reading, and bound actual reads even if a file grows after the initial metadata
check. Changed sources fail before publication. Hashing stable original media
in other commands remains bounded to that file's opened size, without a global
one-GiB restriction.

## Room tone under generated speech

Generated narration has digital-black pauses, which sound unnaturally dead next to recorded footage. `audio roomtone` puts dry speech on a bed of room tone (a recording of the room with nobody talking):

```sh
skywrite audio roomtone narration.mp3 --bed room.wav -o narration-room.wav
# Quieter or louder floor; high-pass a bed with machine hum first.
skywrite audio roomtone narration.mp3 --bed room.wav -o narration-room.wav \
  --bed-dbfs -62 --bed-highpass 120
```

The recipe is fixed: the speech is resampled to 48 kHz stereo and two-pass loudness-normalized first (`--lufs`, default -18; `--peak-db` true-peak ceiling, default -2); the bed is converted to the same rate and layout (a mono bed is upmixed at -3 dB per channel), optionally high-passed (`--bed-highpass HZ`, 20–400), measured in double precision over the part that is mixed (the first speech-length of a longer bed; all of a shorter one) and gained so its RMS lands at `--bed-dbfs` (default -58 dBFS, allowed -80 to -40), so any recording gives the same floor. A bed shorter than the speech is looped with equal-power crossfades (`--crossfade`, default 0.5 s; quarter-sine, so uncorrelated noise keeps its level across seams), a longer one is trimmed. The two are summed (`amix` with `normalize=0`) with no loudness pass after the mix, and no reverb or impulse response. The output has exactly the speech's decoded duration at 48 kHz, so word timings measured on the dry take still apply.

Refused before anything is written: speech that is digital silence or too short for integrated loudness (under about 0.4 s of gated signal), a missing or unreadable bed, the speech itself as the bed, a bed under one second, a digitally silent bed, a bed louder than -30 dBFS RMS (speech, music or a loud noise, not room tone), and a bed that would need more than +40 dB to reach its target. The JSON report has `speech` (normalization measurements, `measured_lufs` and `measured_true_peak_dbtp` before the mix), `bed` (`measured_rms_dbfs`, `measured_peak_dbfs`, `applied_gain_db`, `level_in_mix_rms_dbfs`, `loop_repetitions`, `trimmed`, `highpass_hz`), `mix` (`integrated_lufs`, `true_peak_dbtp`) and the fresh output decode. It warns when the mixed true peak exceeds the ceiling by more than 0.1 dB, when the fitted bed lands more than 1 dB from its target (a bed whose level changes over time), and when the bed had to loop; a fitted bed that is digital silence is refused.

Measured with FFmpeg 8.0.1 on synthetic media (6 s of 220-Hz bursts, 0.4 s on and 0.6 s of digital black, mono 24 kHz; pink-noise beds): speech measured -18.0 LUFS before the mix; a 2.5-s stereo 44.1-kHz bed looped three times landed at -58.0 dBFS RMS in the mix (triangular seams had measured -58.24), a 12-s mono 48-kHz bed was trimmed to -57.9 dBFS; every 0.5-s gap measured within 1.5 dB of -58 dBFS RMS; the mix's true peak stayed under -2 dBTP; output was exactly 288000 samples for both WAV and MP3. `tests/roomtone.rs` checks these with independent FFmpeg decodes.

## Recording room tone

```sh
skywrite audio inputs                        # capture devices; records nothing
skywrite audio record-bed --seconds 30 -o room.wav --confirm [--device ID]
```

`audio inputs` lists what the system FFmpeg can capture from, as JSON `devices` with `id`, `name`, `default`, `backend` and `monitor`: PulseAudio/PipeWire (`ffmpeg -sources pulse`) and ALSA (`-sources alsa`) on Linux, avfoundation on macOS (`-list_devices true`; FFmpeg marks no default there, and the id is the audio device index), dshow on Windows (the id is the device's alternative name). Pulse `.monitor` sources capture playback, not the room.

`audio record-bed` captures the user's real environment, so it records only with `--confirm`; without it, it fails and says to ask the user first. Agents must ask before running it. It prints the device and duration on stderr before recording, keeps `--seconds` (5–120, default 30) of mono 48-kHz PCM16 WAV after discarding a 4-second start-up lead, refuses an existing output without `--overwrite`, and never uploads anything. `--device` must be an id from `audio inputs`. Without it, record-bed uses only an input the backend itself names as default (Pulse/ALSA `*`; on macOS FFmpeg's avfoundation `default`, the System Settings input, listed as id `default`); when none is named (dshow), it asks for `--device` instead of guessing. The lead is measured, not assumed: the built-in microphone of the Linux test machine (PipeWire, FFmpeg 8.0.1) starts with a DC transient of -0.84 FS at 0.5 s that decays below 0.001 FS only after about 3.5 s, so the 1.5 s first planned would have kept it.

The report adds `rms_dbfs`, `peak_dbfs`, `crest_factor_db`, `dc_offset`, `low_frequency_share_below_150_hz` (power share), per-50-ms-window statistics in the 200 Hz–4 kHz speech band (`median_window_rms_dbfs`, `transient_count` for windows 15 dB above the median, `active_window_share` and `speech_bursts` for 100–800 ms runs 10 dB above it) and a `speech_likeness` hint (`unlikely`, `possible`, `likely`), with warnings: louder than -40 dBFS RMS (too loud for room tone), more than half the power below 150 Hz (use `audio roomtone --bed-highpass 120`), transients or speech (re-record in silence), and a DC offset above 0.01 FS (device still settling). A digitally silent capture is reported as such. Measured on the Linux test machine's 30-s capture (after the lead): RMS -31.8 dBFS, peak -16.7 dBFS, 84% of the power below 150 Hz, no transients, speech unlikely: machine-room hum, flagged as loud with the high-pass suggested.

On macOS the first capture triggers the system microphone permission for the app that runs skywrite. FFmpeg reports no permission state of its own: a denied or never-granted microphone either fails to open ("Failed to create AV capture input device: …") or delivers digital silence. Both are reported with what to allow: System Settings > Privacy & Security > Microphone, for the terminal, editor or agent app. A capture that produces nothing before its deadline is reported as a probably pending permission prompt.

Good room tone: record in the room the voice should seem to be in, at the same microphone position and gain as the speech, with nobody talking or moving, notifications off and doors closed, for at least 30 seconds.

