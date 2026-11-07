# Audio in recordings: implementation plan

Status: phases 0–3 done (system sound; several sources, volumes, separate
tracks, devices; overlay and tray controls, the Flatpak), 2026-11-01. Phase
4 (smart balance) done 2026-11-05: the ffmpeg watchdog, `duck`, `smart`'s
clarity report and at-stop re-mix, and `--audio-loudness`. The dynamic EQ
was decided against (see phase 4's list).

## Goal

`clipt record --audio` adds sound to a recording: what the computer is
playing, the microphone, or both. Audio stays in sync with the video over
long recordings and across pauses, costs little CPU, and is off unless asked
for, so nobody's microphone is recorded by surprise.

## Decisions

| Question | Decision | Why |
|---|---|---|
| Capturing sound | libpulse-simple, loaded at run time (`dlopen`) like libpipewire | One small API (`pa_simple_new`, `pa_simple_read`, `pa_simple_get_latency`, `pa_simple_free`) that works the same on Wayland, X11 and in the Flatpak. PipeWire desktops serve it through pipewire-pulse, which every major distribution installs; plain PulseAudio serves it natively. The library is in the Flatpak runtime and on this machine. Not linked, so clipt still starts without it |
| Not ffmpeg's own audio input (`-f pulse`) | Rejected | ffmpeg would stamp audio with its own clock, and keep recording through a pause. clipt's video timestamps are its own (see the recording plan's writer), so audio has to be too |
| Not native PipeWire audio | Rejected for now | clipt's PipeWire code handles video formats only, and the Flatpak can't reach PipeWire for audio without a permission Flathub discourages (`xdg-run/pipewire-0`); it gets audio through the PulseAudio socket. Revisit only if pipewire-pulse turns out to be missing somewhere that matters |
| Getting audio to ffmpeg | Extra tracks in the Matroska stream clipt already writes (`A_PCM/INT/LIT`, 16-bit, 48 kHz, stereo) | Audio and video timestamps come from one clock, in one stream, so pause and sync are handled in one place |
| Sources | Up to 8: `system` (the default output's monitor, `@DEFAULT_MONITOR@`), `mic` (`@DEFAULT_SOURCE@`), or devices by name; `both` means `system,mic` | Covers tutorials (`both`), demos (`system`) and narration (`mic`); names cover a second microphone or another output. Added in phase 2 on request, with `clipt list sound` to find the names |
| Several at once | One audio track per source, each through its own `volume`, mixed by ffmpeg (`amix`, `normalize=0`) | Each source keeps its own clock and latency; mixing in clipt would mean resampling one to match the other. `normalize=0` keeps the volumes asked for, rather than dividing by the number of sources |
| Balance | A volume per source, 0–400 %, set before recording (`system:70,mic:120`); optionally each source also kept as its own track at its recorded level (`--separate-audio`), after the mix | Fixed volumes give one track that plays right everywhere; separate tracks let the balance be fixed afterwards in an editor. Changing volumes during a recording is left for later (see *Not in this plan*) |
| Listing devices | libpulse's asynchronous API, loaded like libpulse-simple, reading only the first fields of `pa_server_info` and `pa_source_info` | libpulse-simple can't list anything; the first fields of those records haven't changed in libpulse's history, and a unit test pins their offsets |
| Encoding | AAC (ffmpeg's built-in `aac`), 160 kb/s, in every container | MP4 and MOV need it, MKV takes it, and it plays everywhere. Opus is better per bit but not in MOV, and is awkward in MP4 for some players |
| GIF | No audio | GIF can't hold sound; `--audio` with `--gif` says so and records without it |
| Default | Off | Recording the microphone without being asked would be a privacy failure |

## How it fits together

```
 video source ──frames──▶ camera ──▶ writer ─┐
                                              ├─▶ Matroska on ffmpeg stdin ──▶ file.mp4
 audio source(s) ──chunks (+ capture time)──▶─┘    (track 1 video, 2–3 audio)
 (libpulse-simple, own thread each)
```

- **Audio source** (`src/record/audio.rs`): one thread per source, blocking
  in `pa_simple_read` on 20 ms chunks (3 840 bytes at 48 kHz, 16-bit
  stereo; `fragsize` set to match, or the server hands over much larger
  fragments). Chunks are stamped by a sample clock: an anchor plus the
  samples read so far, divided by 48 000. Each read gives one estimate of the
  anchor: when the read returned, minus the reported latency and the
  samples read. Reads can only return late, never early, so the anchor is
  the earliest estimate over the last 10 s (the window lets it follow clock
  drift). Stamping each chunk from its own read time instead jitters by
  ±15 ms (see the spike results). Chunks go to the writer through a
  bounded queue. Audio can't be dropped the way video frames are without
  causing gaps, but a full queue means ffmpeg has stalled, so it ends the
  recording with an error rather than eating memory.
- **Writer** (`src/record/mod.rs`): already turns capture times into stream
  times, with paused time subtracted. Audio chunks go through the same
  function, placed against a list of every pause so a chunk next to a pause
  lands exactly; chunks heard while paused are thrown away. Time zero stays
  the first video frame, which arrives at once (capture always sends a first
  frame), so at most a chunk is lost before it. Chunks are written as they
  arrive rather than interleaved by timestamp: ffmpeg orders the two tracks
  itself, and the tests show the lengths and pauses come out right. After a
  stop, the sound thread keeps reading until a chunk stamped after the stop
  arrives (at most 1 s), because the server's latency worth of sound is
  still in its buffer when the stop comes.
- **Matroska** (`src/record/mkv.rs`): a second (and third) TrackEntry
  (`A_PCM/INT/LIT`, SamplingFrequency 48 000, Channels 2, BitDepth 16).
  SimpleBlocks already carry a track number, so `frame_prefix` became
  `block_prefix`, with a track argument.
- **Encoder** (`src/record/encode.rs`): maps the audio tracks and adds
  `-c:a aac -b:a 160k`. A resampling filter (`aresample=async=1:first_pts=0`)
  takes up the slow drift between the sound card's clock and the system
  clock (under 2 parts per million in the spike, but other hardware can do
  worse), fills any tiny gap between chunks, and fills the wait before the
  first chunk with silence so the sound starts with the video (see
  *PulseAudio's slow start* under Risks). With two sources:
  `amix=inputs=2:duration=longest:normalize=0`, so neither source is
  halved in volume.

## Keeping sound and picture in sync

Both are stamped on the monotonic clock at capture, so they line up by
construction. What can still go wrong, and the answer to each:

- **Latency guesses.** If `pa_simple_get_latency` is off, everything is
  shifted by a constant. In the spike it was 3–6 ms early once settled, too
  small to correct. A `record_audio_offset_ms` setting stays as the escape
  hatch for setups that measure worse.
- **Clock drift** over long recordings: under 2 parts per million in the
  spike (about 7 ms an hour), and the resampler above for hardware that
  drifts more.
- **Still screens.** On a still screen ffmpeg gets audio and no video for a
  long time. Its muxer holds the audio until video catches up (in the spike
  an MP4 stayed at 48 bytes through 14.5 s of still screen), and the result
  is still in sync. That only matters if ffmpeg is killed: a crash loses
  that much audio, and an MP4 without its index at the end is unplayable
  anyway. So the writer doesn't need to re-send frames.
- **Pause.** No sound is captured while paused, and audio after resuming
  is shifted back by the same paused time as the video, so the cut is in
  the same place in both.

## User interface

- `clipt record --audio[=SOURCES]`: `system`, `mic`, `both`, or a comma-
  separated list of sources (`system`, `mic`, device names), each with an
  optional `:PERCENT` volume. A bare `--audio` means `system`.
  `--separate-audio` also keeps each source as its own track. `--no-audio`
  overrides the setting for one recording.
- Settings: `record_audio`, taking the same values (default `"none"`), and
  `record_separate_audio` (default `false`).
- `clipt list sound` lists the devices, marking what `system` and `mic`
  stand for. (Not new lines in `clipt list`: the Flatpak probe script counts
  its lines as outputs.)
- Track names: "Mix", then "System sound", "Microphone" or the device name;
  stored as the handler name in MP4 and MOV (what players show) and as the
  title in MKV.
- Overlay in record mode: a toggle in the toolbar (speaker, microphone, off)
  that starts from the setting. It changes only that recording, not the
  setting.
- Tray: a *Record with sound* checkbox under the record entries.
- Errors are clear and stop before recording starts: no libpulse-simple, no
  sound server, an unknown device (pointing at `clipt list sound`), or a
  volume out of range, each saying what to install or check. The recording
  doesn't quietly go ahead without sound. (A missing microphone can't be
  told apart: the server always has a default input, falling back to a
  monitor or a dummy device.)
- The man page, completions, README Usage and CHANGELOG get the new
  options.

## Flatpak

- `--socket=pulseaudio` (the usual permission for apps that play or record
  sound; recording the microphone through it is allowed on today's Flatpak).
- The manifest's ffmpeg gains what it needs and nothing more: the
  `pcm_s16le` decoder, the `aac` encoder, swresample (currently disabled),
  and the `aresample`, `amix` and `aformat` filters.

## Testing

- Unit tests: Matroska audio track entries, chunk timestamps (latency,
  pauses), interleaving order.
- e2e (Sway and GNOME suites first): a PulseAudio or pipewire-pulse server
  with a null sink in the container. Play a 1 kHz tone, record, and check
  the result with `ffprobe` and `astats`: there is an AAC track, the tone is
  in it, and its duration matches the video. Pause test: the tone plays only
  while paused, and the file must be silent there. Several sources: a second
  null sink stands in for the microphone with a 440 Hz tone, so each track
  can be checked by frequency. (The planned beep-and-flash sync test was
  dropped: the test's own playback latency, ffmpeg's PulseAudio output and
  PulseAudio's slow start, is unknown, so it would mostly measure itself.
  The spike measured stamping against a known reference instead.)
  Phase 4 adds ducking and smart (two tones a fixed dip apart, pink noise
  for a re-mix that raises the clarity), and `--audio-loudness` (loudnorm
  before and after: a −27 LUFS mix comes out at −14).
- Flatpak: the same tone test through `clipt-launch` against headless
  Sway, as done for video. For `--audio-loudness` so far only its ffmpeg
  commands, inside the Flatpak (see the loudness notes in phase 4).
- By hand: 30 minutes with a video playing on Plasma, then the sync
  checked at the end; then once on GNOME.

## Phases

Each phase ends with something usable, tests in CI, and a PR.

**0. Spike (done 2026-11-01).** libpulse-simple through Python's ctypes,
and a Python Matroska writer copying clipt's. See the spike results below.

**1. System sound (done 2026-11-01).** `audio.rs`, Matroska audio track,
the writer's sound handling, AAC encoding, pause, `--audio`, `--no-audio`
and `record_audio` (`none` and `system`), errors (no libpulse-simple, no
sound server, an ffmpeg without AAC, `mic`/`both` not yet). The man page,
completions and README got the options now rather than in phase 3. Shared
e2e tests (`tests/audio.sh`) on Sway and X11 with PulseAudio, on GNOME
with pipewire-pulse: a tone is recorded at the right level, sound and video
last as long, a tone played only while paused is left out, `--gif` with
`--audio` warns, and a missing server stops the recording before it starts.

**2. Several sources (done 2026-11-01).** Widened on request from "`mic`
and `both`" to: any number of sources up to 8, a volume for each,
`--separate-audio`, devices by name and `clipt list sound`. e2e tests on
all three desktops: the listing, three tracks with the right names, each
separate track holding only its own tone (31 dB apart), the mix holding
both equally, `system:100,mic:25` putting the microphone 12 dB under,
a device by name, and the unknown-device and volume errors. Found on the
way: pipewire-pulse, unlike PulseAudio, accepts a stream from a device that
doesn't exist, and the recording ran on with nothing from it, so named
devices are now checked against the server's list before recording, and a
source that has delivered nothing after 5 s stops the recording.

**3. Controls and packaging (done 2026-11-01).** The overlay's record
toolbar has speaker and microphone toggles (**S**, **M**) that start from
the setting and swap `system` and `mic` in or out for that recording,
keeping named devices and volumes; the tray has a *Record sound* checkbox
(ticked when `record_audio` names sources; unticked passes `--no-audio`).
The Flatpak has `--socket=pulseaudio`, and its ffmpeg the PCM decoder, AAC
encoder, swresample and the sound filters (7.8 MB, from 6.9). Found on the
way: the AAC check's `anullsrc` makes 8-bit samples, whose decoder the
Flatpak's ffmpeg lacks, so the check now asks for 16-bit, as recordings
send. Tested: Sway e2e (S in the overlay turns sound on, and off when the
setting has it on), unit tests for the toggles and the tray checkbox, the
toolbar rendered and looked at, and a tone recorded through the Flatpak.

**4. Smart balance (done 2026-11-05).** Keeping a voice intelligible over the
desktop's sound, from the voice-over-music rules below. Its spike is done.
The order it suggested: a watchdog on ffmpeg finishing (protects every
recording, not just this phase); `duck` (voice clean-up with level matching,
sidechain ducking); then `smart` (voice gate, leakage check, SII report,
at-stop re-mix for loud beds); the dynamic EQ last, if at all (decided
against, see below). The background classifier (YAMNet) isn't worth
building: the 20 dB default stays (see the spike list).

Done:
- **The watchdog** (#46): after 30 s without ffmpeg finishing, clipt kills
  it and reports the partial file.
- **`duck`** (#48): `--audio-balance=duck` or `record_audio_balance =
  "duck"`. Only the `mic` source counts as the voice for now: `:` already
  sets a volume (`mic:0.7`), so marking a named device needs its own
  syntax. The spike's clean-up chain and ducking settings, in the mix only;
  separate tracks stay as recorded, for the at-stop re-mix. **Level
  matching** can't be a fixed make-up gain: a compressor's net gain
  depends on the input level, so the 9.5 dB that matched at −23 dBFS made
  a LibriVox voice at −45 dBFS 10 dB louder (with its noise). The make-up
  follows the voice instead: the raw and cleaned voice's power, each
  smoothed over about 1.6 s, and the cleaned voice scaled by the square
  root of their ratio (within ±12 dB). It stays within 0.1 dB from −45 to
  −23 dBFS, peaks no higher than the raw ones, and costs about 2 % of a
  core against 0.5 %. Two ffmpeg traps it hit: `aeval` leaves the channel
  layout unset, after which `lowpass` filters nothing; and `amerge` can't
  pick a format between clipt's 16-bit sound and the compressor's doubles.
  An `aformat` fixes each. The Sway e2e test shows the system sound
  dipping 10.6 dB under the microphone.
- **`smart`'s report** (`--audio-balance=smart`): `duck`, plus a second
  ffmpeg output that sends the mix's inputs (after clean-up and ducking)
  back to clipt as mono 48 kHz floats, measured in `src/record/clarity.rs`
  and reported at the stop ("speech clarity 0.84, good"; with a leak, the
  voice's median gap over the rest instead). The measurement is the
  spike's, and reproduces its plain-bed table on the same recordings
  (0.29, 0.61, 0.74, 0.85 at 0, 10, 15, 20 dB, against 0.30, 0.61, 0.74,
  0.86). Decisions: speech frames are the spike's level gate (within 15 dB
  of the voice's 90th percentile), not nnnoiseless, so no new dependency;
  bands come from an FFT with sharp edges, because fourth-order Butterworth
  bands let the voice's strong low end into the 4 and 8 kHz bands and read
  0.12 high; the leak check counts windows whose other sound is above
  −60 dBFS (quiet music is about −55 once ducked). On the readings a leak
  20, 30 or 40 dB under the voice peaks at 0.83–0.90, a clean one at 0.13.
  Under 0.5 % of a core. Below 0.75 it also says how much quieter the
  other sound should be, computed from the frames already measured:
  ducking's dip is set by the voice alone, so turning the other sources
  down lowers them by exactly that in the mix. From the 10 dB recording it
  predicts 6 dB (so 16 dB); remade at 15.5 dB it measures 0.748 and
  predicts the same 16. So the at-stop re-mix needs no search: one pass
  at the computed volume.
- **The re-mix** (the same option): while recording, `smart` also keeps
  each source as recorded in a hidden FLAC file beside the video
  (`.NAME.clipt-sound.mka`; 0.3 Mbit/s for speech, 0.5 for music, so
  150–250 MB an hour per source). Below 0.75, and
  without a leak, one ffmpeg pass mixes again from it with the other
  sources the computed amount quieter (at most 20 dB: past that the other
  sound is gone, not under the voice; and only if that raises the clarity
  by 0.05, so a voice that is itself the problem is left alone), copies the video, encodes the mix
  and any separate tracks again, measures the result the same way, and
  replaces the file. The kept file goes however the recording ends. A
  band where the voice has next to nothing (60 dB under its strongest)
  now counts for nothing: otherwise a band silent in both reads as half
  heard, and better as the rest is turned down. The spike's numbers are
  unchanged by it. Unit test: a voice under pink noise 10 dB louder
  measures 0.41 and asks for 10.5 dB; the re-mix measures 0.76.
- **The dynamic EQ** (the table's third row): decided against. The spike
  found it adds +0.03 to +0.09, only worth it for the loudest beds, and
  only those within about 5 dB of the voice need the re-mix at all (which
  reaches the target in one pass without it). It would also be the most
  fragile graph to build (`acrossover` into `sidechaincompress` and back),
  and the one that hung ffmpeg at end of stream (which the watchdog now
  covers). So `smart` is clean-up, ducking and the re-mix; the dynamic EQ
  stays out unless real recordings show beds it would have saved.
- **The loudness** (`--audio-loudness`, `record_audio_loudness`): the
  table's last row. At the stop, after any re-mix, a `loudnorm` pass
  measures the mix (`print_format=json`) and `volume` applies one fixed
  gain to −14 LUFS, capped so the true peak stays at or under −1 dBTP: a
  peaky mix ends up quieter than −14 rather than compressed, and the stop
  message says so. AAC can raise a transient's peak (clicks capped at −1
  went in and came out at −0.2), so the encoded mix is measured as well,
  and encoded again with that much less gain, up to three times, until
  its true peak is under the limit. Every ffmpeg step runs under the same
  watchdog as the re-mix, so a hung one can't keep the recording's lock. Not `loudnorm`'s own second pass: even with
  `linear=true` it falls back to its dynamic mode, silently, when the
  measured LRA is over its target (7 LU by default) or the gain would put
  a peak over the limit; a unit test steps a tone through 0, −8 and
  −16 dB (LRA about 10) and checks the 16 dB between them survives, where
  the old pass made it 11. The video and any separate tracks are copied,
  the mix alone encoded again. A mix already within half a LU with no
  peak over the limit is left alone rather than re-encoded, and so is one
  with no loudness at all (silence). The fixed gain, not a dynamic one, is
  what keeps a quiet voice from being pumped up between sentences. The
  measurement's null output is encoded as `pcm_f32le`, which the Flatpak's
  ffmpeg has (its default, `pcm_s16le`, it doesn't). e2e:
  a −27 LUFS tone mix comes out at −13.9 on Sway, GNOME and X11, with any
  separate track copied at its level and codec. The Flatpak's filtered
  ffmpeg needed the AAC decoder (the mix is read back) and `loudnorm`.
  Its ffmpeg was run by hand on a built Flatpak (2026-11-07): the
  measurement without an encoder named fails there ("Encoder not found",
  no `pcm_s16le`) and with `pcm_f32le` reads the stepped tone (−34.7
  LUFS, LRA 10.3); the gain step then brings it to −14.0 with its 15.9 dB
  swing unchanged. A whole recording in the Flatpak, which on Plasma goes
  through the ScreenCast portal's dialog, is still to be run.

## Smart balance (phase 4)

clipt has the voice and the other sources as separate streams before they
are mixed, which is what this needs. A setting such as
`record_audio_balance = "off" | "duck" | "smart"` (default `off`):

| Step | How | When | In |
|---|---|---|---|
| Voice clean-up: high-pass ~80 Hz, ~3:1 compression for 4–8 dB of gain reduction, de-essing, 2–4 kHz presence boost | ffmpeg `highpass`, `acompressor`, `deesser`, `equalizer` on the voice source | While recording | `duck`, `smart` |
| Ducking: the other sources dip while the voice is present, attack 10–50 ms, release 200–500 ms | `sidechaincompress` on each other source, keyed by a split of the voice | While recording | `duck`, `smart` |
| Dynamic EQ: 3–6 dB cut at 1–4 kHz in the other sources only while the voice is present | `acrossover` splits them at 1 and 4 kHz; only the middle band goes through `sidechaincompress`; the bands are summed again | While recording | Not built: decided against (see the phase 4 list) |
| Measuring intelligibility: Speech Intelligibility Index (ANSI S3.5, octave bands) | In clipt, on the PCM chunks it already handles: octave-band filters at 250 Hz–8 kHz, levels per ~400 ms frame for the voice and for the rest, SII = Σ Iᵢ·clamp((SNRᵢ + 15)/30, 0, 1) with weights 0.062, 0.167, 0.237, 0.265, 0.214, 0.055. Judged on the 10th-percentile frame | Reported at stop ("speech clarity 0.84, good") | `smart` |
| Fixing it | If the 10th-percentile SII is under 0.75 (or 0.90 for a safe margin), mix again from the separate tracks with the other sources lower until it isn't; sound is re-encoded, video copied | At stop | `smart` |
| Loudness: −14 LUFS integrated, −1 dBTP | `loudnorm` measures the final mix, `volume` applies one peak-limited gain, video copied | At stop | `record_audio_loudness` |

To settle first:
- **Which source is the voice.** `mic` by default; a named device would
  need marking, for example `--audio=system,alsa_input…:voice`.
- **Only frames with speech count.** Between sentences the voice's SNR is
  meaningless and would drag the 10th percentile down, so the measurement
  needs voice activity detection on the voice (see below).
- **Speakers spoil it.** Through speakers the microphone also hears the
  desktop, so its track looks clearer than it is and SII reads high, and
  (the research found) voice detection then counts music as speech too.
  Headphones keep it honest; see *Leakage* below for how clipt can tell.
- **SII treats music as steady noise.** Judging the 10th percentile helps;
  the broadband rules of thumb (other sound 15 dB under the voice, 20 dB
  if it has singing or talking, 10–12 dB with the dynamic EQ) are the
  sanity check.
- **Testing needs speech-like sound.** Steady tones won't trigger ducking
  the way speech does; speech-shaped, syllable-rate modulated noise can
  stand in for a voice in the e2e tests. Add music under the voice source,
  and a music-only voice source, so the tests cover leakage too.

### Telling what's playing

Two questions, answered on separate threads fed from the chunks the writer
already handles (never in the reading thread, so a slow analysis can't hold
up a read; no real-time callback is involved, so no lock-free ring buffer
is needed either):

- **Is someone speaking?** (the voice source) **nnnoiseless**, a pure-Rust
  port of RNNoise: no model file, no ONNX Runtime, BSD-3-Clause (allowed
  by `deny.toml`). It works at 48 kHz on 10 ms frames, so each 20 ms chunk
  is two frames and nothing is resampled, and it returns a voice
  probability per frame. It's also a denoiser, which could later cover
  noise suppression. Silero VAD is more accurate but needs ONNX Runtime (a
  native library few have) or tract (megabytes more); webrtc-vad compiles
  C into the binary (harder for the cross-compiled release targets) and is
  weaker against background noise.

  Measured ([audio-phase4-research.md](audio-phase4-research.md)): on a
  clean voice source it's a good speech-versus-silence gate (AUC 0.98;
  threshold 0.5 with 100 ms smoothing keeps 73 % of speech frames and
  passes 2.6 % of pauses; a 200 ms hangover at 0.7 reaches 92 % with 10 %).
  It is not a speech-versus-music detector: against music with vocals,
  instrumental music and noise its AUC is 0.2–0.66, no threshold separates
  them, and reference RNNoise does no better. CPU: under 0.5 % of a core.
  **Decision:** nnnoiseless gates speech on a voice source known to be
  clean; *Leakage* decides whether it is.
- **Leakage: does the voice source hear the other sources?** clipt has
  both streams, so it can tell: correlate the voice source against each
  other source at lags up to ~250 ms (the speakers' and room's delay),
  in the voice source's pauses. Measured ([audio-phase4-spike.md](audio-phase4-spike.md)):
  over whole windows the voice swamps the leak, but in the quietest
  second-long windows a leak even 40 dB under the voice correlates at
  0.45–0.79 while controls stay at or below 0.19, so "a quiet second
  above about 0.3" detects it. Cheap: one FFT pair per quiet second at
  8 kHz. Without enough pauses, it says it couldn't check. With leakage, `smart` says so at stop ("the microphone hears
  the speakers; headphones give a reliable result") and falls back to the
  broadband rules rather than report an SII it can't trust. YAMNet's
  speech score on the voice source is the stronger second discriminator,
  if the background model is built anyway.
- **What is the rest: music with vocals, instrumental music, or noise?**
  It picks the target in the broadband rule: 20 dB under the voice for
  singing or other talkers (competing speech masks beyond its energy,
  which SII doesn't capture), 15 dB otherwise. Open until the spike, in
  order of cost:
  1. No model: assume the safe 20 dB, or take a hint
     (`background = "vocals" | "instrumental" | "noise"`).
  2. YAMNet (AudioSet's 521 classes: summed speech-and-singing scores
     against music scores, an RMS gate to skip silence, decisions smoothed
     over a few windows), behind a cargo feature. Its ~3.7 M parameters
     are about 15 MB, three times clipt's binary, so the model would be
     downloaded on first use to `~/.local/share/clipt/models` and checked
     against a known hash, not built in. It would run through `tract`
     (pure Rust; `ort` loads ONNX Runtime, which few have) on 0.96 s
     windows every 0.48 s at 16 kHz, so this path does resample (`rubato`
     or a simple polyphase filter), on one thread, loaded before recording
     starts. The Flatpak has no network access, so it would ship the
     model, fetched at build time and checked by hash.

     Licence (checked in the research): the ONNX export and Google's
     original are both Apache-2.0, so clipt can download and the Flatpak
     can ship it, with the licence and attribution. The export's source
     archive hash matches Google's; the export is 16,093,355 bytes,
     SHA-256 `d3835ffb…8f908e` (in full in the research). The class names
     are AudioSet's, CC BY-SA 4.0, so clipt hard-codes the few class
     indices it needs (Speech 0, Conversation 2, Narration 3, Singing 24,
     Choir 25, Music 132, Vocal music 249, …) rather than ship the CSV.

Libraries, if the measurement moves into clipt: the octave-band filters
are a few biquads, about 30 lines, so written here rather than a crate
(`biquad` would do, pure Rust); an FFT (`realfft`) is more than six bands
need and adds to the binary. `ebur128` (pure Rust, MIT) for short-term
loudness per stream, if the broadband rules are checked live; the −14 LUFS
master stays with ffmpeg's `loudnorm`. Not `cpal`: on Linux it links ALSA
(and libpulse or libpipewire with those features), which clipt avoids so
it starts without them, and it doesn't readily record the output's monitor.
No lock-free ring buffer either: sound is read with blocking reads on its
own thread, not in a real-time callback.

Spike:
- Done (2026-11-02, by the superagent, PR #36): nnnoiseless as voice
  detection, and the YAMNet weights' licence. Findings above and in
  [audio-phase4-research.md](audio-phase4-research.md).
- Done (2026-11-02, [audio-phase4-spike.md](audio-phase4-spike.md)):
  SII with the bed plain, ducked and ducked with the dynamic EQ. The rules
  of thumb hold (plain: 0.75 at about 15 dB under, more with vocals);
  ducking by about 11 dB (`sidechaincompress`, threshold 0.02, ratio 8,
  attack 20 ms, release 300 ms) reaches 0.75 with the bed only 10 dB
  under; the dynamic EQ adds 0.03–0.09; only beds within ~5 dB of the
  voice need the at-stop re-mix; the voice clean-up helps (+0.05–0.07 at
  equal level) only if it keeps the voice's loudness.
- Done (same document): the leakage check, which works in the pauses (see
  *Leakage* above).
- Done (2026-11-02, by the superagent, #43,
  [audio-phase4-yamnet.md](audio-phase4-yamnet.md)): YAMNet through
  tract. About 0.9 % of a core and a 46 ms load, but 18.9 MiB more binary
  on top of the 16 MB model, and a clean build going from 2.6 s to 248 s.
  It finds speech, instrumental music and noise well (97 %, 93 %, 100 %),
  but only 68 % of music with vocals, the one case that picks the 20 dB
  target. **Decision:** no background classifier; the safe 20 dB default
  stays. As a voice check its speech score is strong (AUC 1.00 against
  music and noise on clean speech, 0.83–0.87 with music under it), so it
  remains the fallback if the leakage check proves not enough.
- Found (same document): the dynamic-EQ graph (`acrossover`,
  `sidechaincompress`, `amix`) sometimes hung at end of stream in ffmpeg
  9.0.2, and clipt waited for ffmpeg to finish with no limit, so a
  watchdog came first (done, #46).

## Spike results (2026-11-01)

Python scripts, not clipt code, so the numbers are about the approach.
PipeWire 1.6 with pipewire-pulse throughout.

**Timestamps.** A null sink in a container. The test played 1 kHz beeps at
moments known from the playback stream's own sample clock, recorded its
monitor, and compared each beep's recorded start with its played start,
15 beeps per run, three runs:

| Stamping | Recorded minus played | Spread |
|---|---|---|
| Each chunk from its own read time, minus latency | −3.3 to −4.7 ms mean | 28–32 ms, the same sawtooth every run |
| Sample clock, earliest anchor | −3 to −6 ms once settled (after ~4 s); up to +11 ms before | under 0.5 ms once settled |

The sawtooth is probably PipeWire processing in 1 024-sample (21.3 ms)
blocks against 20 ms reads, so reads return in bursts. The sample clock
removes it. The first seconds' error is the anchor still settling; it is
under one frame at 60 fps, and the writer already holds audio back for
200 ms, so stamps could be fixed up before sending if it ever matters.

**ffmpeg.** A Matroska stream from the spike writer: black frames at 0 and
0.5 s, a white frame at 15 s, a black one at 20 s, and continuous PCM with
beeps at 1 s and 15 s. Encoded with clipt's libx264 arguments plus
`-c:a aac -b:a 160k -af aresample=async=1`:

- MP4, MOV and MKV: the beeps at 1.000 and 15.000 s (±0.3 ms), and the
  flash at 15.000 s. ffmpeg handles AAC's start-up delay itself.
- Written in real time, with 14.5 s of still screen: the same result. The
  MP4 stayed at 48 bytes during the still screen (see *Still screens*).
- ffmpeg warns `Guessed Channel Layout: stereo`, which is right. Matroska
  has no channel-layout element; clipt's `-loglevel error` hides it.

**Drift.** libpulse-simple on a desktop with a Focusrite Scarlett Solo
(USB), counting samples from `@DEFAULT_MONITOR@` against the monotonic
clock for 10 minutes, with a silent stream playing so the sound card was
running and timing the graph. The difference never passed ±1.1 ms and
didn't build up: under 2 parts per million, so at most about 7 ms over an
hour. The resampler's drift correction is a safety net, not a necessity,
on hardware like this. A second run, where the card may have been idle,
agreed (±1.5 ms over 4 minutes). Other sound cards, and Bluetooth, may
drift more.

## Risks

- **PulseAudio's slow start.** Found in phase 1: PulseAudio (17, null
  sink) took 1.5 s to deliver the first 20 ms chunk, then ran normally;
  pipewire-pulse delivered it in 16 ms. Asking for the server's default
  fragment size made the first read quick but the second one slow
  instead. So on PulseAudio the first second or so of a recording is
  silent (padded by the resampler) rather than missing. Whether real
  sound cards on PulseAudio do the same is unchecked; opening the stream
  before the overlay would hide it there.
- **pipewire-pulse missing** on a minimal PipeWire install: clipt says so.
  Native PipeWire audio is the fallback to build only if this proves
  common.
- **Latency reporting varies** between PulseAudio and pipewire-pulse
  versions: phase 0 measures both, and there's the offset setting.
- **Bluetooth headsets** switch profile (and sample rate) when the
  microphone opens, which can change the system sound mid-recording. The
  sound server resamples to the 48 kHz clipt asks for, so the stream stays
  valid. Worth one manual test.
- **Echo with `both`**: through speakers, the microphone picks up the
  system sound as well. Not clipt's to solve; the README suggests headphones.

## Not in this plan

Changing volumes during a recording (it would mean applying them in clipt
before ffmpeg, with a command such as `clipt record --volume mic=+10%`;
worth it once the overlay or tray has controls for it), noise suppression,
recording one application's sound only (PipeWire can, through the portal,
but not through the PulseAudio API), and webcam overlay.
