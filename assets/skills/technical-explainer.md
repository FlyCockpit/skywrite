# Technical explanation videos

Verify the script's technical claims and equations first. Use deterministic HTML/SVG diagrams and Hyperframes/Remotion animations for exact labels and causal explanations; generated video can supply supporting footage.

1. Read any requested style with `skywrite styles read NAME`.
2. Discover a narration model and voice: `skywrite models list --task speech-generation`. Run `skywrite generate speech --file narration.txt --model MODEL --voice VOICE --output narration.mp3`.
   Generated speech has digital-black pauses. If the user has a room-tone recording (their room with nobody talking), or agrees when you ask to record one with `skywrite audio record-bed --seconds 30 -o room.wav --confirm` (never record without asking; tell them to stay silent in the room the voice should seem to be in), run `skywrite audio roomtone narration.mp3 --bed room.wav -o narration-room.wav`; it keeps the exact duration, so word times from step 3 still apply. Listen to the result.
3. Obtain real word times: `skywrite transcribe narration.mp3 --output narration.words.json`. Do not guess word timing from character counts.
4. Render diagrams/cards with `skywrite image render`; read builtin takumi, hyperframes or remotion skills for animation and typography.
5. Discover a music model with `skywrite models list --task music-generation`. Generate an original bed with `skywrite generate music --model MODEL --prompt 'Gentle instrumental music, ...' --instrumental --output bed.mp3`. Listen to verify vocals, mood, and ending. This is waveform music, not MIDI. To dip it under the voice, run `skywrite audio duck bed.mp3 --sidechain narration.mp3 -o bed-ducked.wav` (use `narration-room.wav` instead if step 2 made it; same samples on every run; a mono bed becomes stereo at -3 dB per channel).
6. Assemble a validated edit JSON using narration and a quieter music bed on audio tracks. Match visual changes to real word times. Render or export FCPXML, Hyperframes, Remotion, or Python/FFmpeg.
7. Watch/listen: verify facts, pronunciation, exact labels, unclipped audio, and readability. You cannot hear the render yourself: measure it with `skywrite verify OUT.mp4 --ends-together --loudness --black --frozen --edit EDIT.json`, quote the numbers and say what was not measured.
8. Share finished assets using outputs upload/list/download.

Repeated generation reuses a durable job. Use `skywrite jobs resume ID` for cached publication; never blindly repeat an uncertain paid submission. Record ALL frustrations with `skywrite nit 'description'`.
