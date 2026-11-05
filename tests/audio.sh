# Sound in recordings: tests shared by the Sway, GNOME and X11 suites.
# Source this after defining check(), then call
#   audio_tests GEOM
# with a region (slurp format) that records on this desktop. Starts a sound
# server first: pipewire-pulse where PipeWire is installed (GNOME's image),
# PulseAudio otherwise, so both are tested. A null sink stands in for the
# sound card, and ffmpeg plays a 1 kHz tone into it. A second null sink,
# "mic", stands in for a microphone: its monitor is the default source, and
# gets a 440 Hz tone, so each source can be told apart by frequency.

sound_server() {
  if command -v pipewire-pulse >/dev/null; then
    pgrep -x pipewire >/dev/null || { pipewire >/tmp/pw.log 2>&1 & sleep 0.5; }
    pgrep -x wireplumber >/dev/null || { wireplumber >/tmp/wp.log 2>&1 & }
    pipewire-pulse >/tmp/pp.log 2>&1 &
  else
    pulseaudio --daemonize=no --exit-idle-time=-1 >/tmp/pa.log 2>&1 &
  fi
  for _ in $(seq 50); do pactl info >/dev/null 2>&1 && break; sleep 0.1; done
  pactl load-module module-null-sink sink_name=clipt >/dev/null
  pactl load-module module-null-sink sink_name=mic >/dev/null
  pactl set-default-sink clipt
  pactl set-default-source mic.monitor
  pactl info | sed -n 's/^Server Name: /  sound server: /p'
}

# tone_on [FREQ SINK]: 1 kHz into the system sink by default.
tone_on()  { ffmpeg -nostdin -loglevel error -re -f lavfi -i sine=frequency=${1:-1000}:sample_rate=48000 -ac 2 -f pulse -device ${2:-clipt} tone & TONES="${TONES:-} $!"; }
tone_off() { kill $TONES 2>/dev/null; wait $TONES 2>/dev/null; TONES=; }
# The microphone's tone, started and stopped on its own so the system tone
# can keep playing: the ducking test needs the voice for only part of it.
MIC_TONE=
mic_tone_on()  { ffmpeg -nostdin -loglevel error -re -f lavfi -i sine=frequency=${1:-440}:sample_rate=48000 -ac 2 -f pulse -device mic tone & MIC_TONE=$!; }
mic_tone_off() { kill $MIC_TONE 2>/dev/null; wait $MIC_TONE 2>/dev/null; MIC_TONE=; }
# noise_on SINK AMPLITUDE: pink noise, speech-like enough for smart to judge
# (tones fill one band each, which turning anything down can't help).
noise_on()  { ffmpeg -nostdin -loglevel error -re -f lavfi -i anoisesrc=c=pink:a=$2:r=48000 -ac 2 -f pulse -device $1 noise & NOISE="${NOISE:-} $!"; }
noise_off() { kill $NOISE 2>/dev/null; wait $NOISE 2>/dev/null; NOISE=; }
# band_db FILE TRACK FREQ: mean level of one frequency in sound track TRACK
# (0-based), from 2 s in (PulseAudio's first second or so is silent).
band_db()  { ffmpeg -nostats -ss 2 -i "$1" -map 0:a:$2 -af "bandpass=f=$3:width_type=q:w=20,volumedetect" -f null - 2>&1 | sed -n 's/.*mean_volume: \(-*[0-9.]*\) dB/\1/p'; }
# band_db_seg FILE START LENGTH TRACK FREQ: the same for a stretch of the
# recording, in seconds from its start.
band_db_seg() { ffmpeg -nostats -i "$1" -map 0:a:$4 -af "atrim=start=$2:duration=$3,bandpass=f=$5:width_type=q:w=20,volumedetect" -f null - 2>&1 | sed -n 's/.*mean_volume: \(-*[0-9.]*\) dB/\1/p'; }
# Track names: Matroska's titles, or MP4's handler names.
titles()   { ffprobe -v error -select_streams a -show_entries stream_tags=title,handler_name -of default=nw=1:nk=1 "$1" 2>/dev/null | tr '\n' ',' | sed 's/,$//'; }
# Mean and peak level of a file's sound, in dB.
mean_db()  { ffmpeg -nostats -i "$1" -map 0:a -af volumedetect -f null - 2>&1 | sed -n 's/.*mean_volume: \(-*[0-9.]*\) dB/\1/p'; }
max_db()   { ffmpeg -nostats -i "$1" -map 0:a -af volumedetect -f null - 2>&1 | sed -n 's/.*max_volume: \(-*[0-9.]*\) dB/\1/p'; }
# Integrated loudness of the first sound track, in LUFS, as loudnorm reads it.
lufs()     { ffmpeg -nostats -i "$1" -map 0:a:0 -af loudnorm=I=-14:TP=-1:print_format=json -f null - 2>&1 | sed -n 's/.*"input_i".*: *"\(-*[0-9.]*\)".*/\1/p'; }
a_codec()  { ffprobe -v error -select_streams a -show_entries stream=codec_name -of csv=p=0 "$1" 2>/dev/null; }
s_dur()    { ffprobe -v error -select_streams "$2" -show_entries stream=duration -of csv=p=0 "$1" 2>/dev/null; }

audio_tests() {
  local g=$1
  echo "-- recording: sound"
  sound_server

  /clipt record -g "$g" -o /tmp/snd-none.mp4 2>/tmp/snd.err & local rp=$!
  sleep 1.5; /clipt record --stop; wait $rp
  check "no sound unless asked (streams: $(a_codec /tmp/snd-none.mp4))" '[ -s /tmp/snd-none.mp4 ] && [ -z "$(a_codec /tmp/snd-none.mp4)" ]'

  tone_on; sleep 0.5
  CLIPT_DEBUG=1 /clipt record -g "$g" --audio -o /tmp/snd.mp4 2>/tmp/snd.err & rp=$!
  sleep 3; /clipt record --stop; wait $rp; local rc=$?
  tone_off
  check "--audio records ($rc; $(grep -iE 'error|can.t|fail' /tmp/snd.err | head -1))" '[ $rc = 0 ]'
  check "the sound is AAC ($(a_codec /tmp/snd.mp4))" '[ "$(a_codec /tmp/snd.mp4)" = aac ]'
  local m=$(mean_db /tmp/snd.mp4)
  check "the tone is in it (mean ${m} dB)" 'awk "BEGIN{exit !($m > -30)}"'
  local va=$(s_dur /tmp/snd.mp4 v) aa=$(s_dur /tmp/snd.mp4 a)
  check "sound and video last as long ($aa s, $va s)" 'awk "BEGIN{d=$aa-$va; exit !(d < 0.25 && d > -0.25)}"'
  grep -E "sound chunks" /tmp/snd.err | sed 's/^clipt: /  /'

  # The tone plays only while paused: none of it may be in the recording.
  /clipt record -g "$g" --audio -o /tmp/snd-p.mp4 2>/tmp/snd.err & rp=$!
  sleep 1; /clipt record --pause; sleep 0.3
  tone_on; sleep 1.5; tone_off; sleep 0.5
  /clipt record --resume; sleep 1; /clipt record --stop; wait $rp
  m=$(max_db /tmp/snd-p.mp4)
  check "sound while paused is left out (peak ${m} dB)" 'awk "BEGIN{exit !($m < -40)}"'
  va=$(s_dur /tmp/snd-p.mp4 v); aa=$(s_dur /tmp/snd-p.mp4 a)
  check "so is its time ($aa s of sound, $va s of video, ~2 s recorded)" 'awk "BEGIN{exit !($aa < 2.6 && $va < 2.6 && $aa-$va < 0.25 && $va-$aa < 0.25)}"'

  /clipt record -g "$g" --audio --gif -o /tmp/snd.gif 2>/tmp/snd.err & rp=$!
  sleep 1; /clipt record --stop; wait $rp
  check "--audio with --gif says GIFs have no sound, and still makes one" 'grep -q "GIFs have no sound" /tmp/snd.err && [ "$(head -c 6 /tmp/snd.gif)" = GIF89a ]'

  echo "-- recording: several sound sources"
  /clipt list sound >/tmp/snd-list.txt 2>&1
  check "clipt list sound marks what system and mic record ($(tr '\n\t' '; ' </tmp/snd-list.txt | cut -c1-90))" \
    'grep -q "^clipt.monitor.*(system)" /tmp/snd-list.txt && grep -q "^mic.monitor.*(mic)" /tmp/snd-list.txt'

  tone_on 1000 clipt; tone_on 440 mic; sleep 0.5
  /clipt record -g "$g" --audio=system,mic --separate-audio -o /tmp/snd-2.mkv 2>/tmp/snd.err & rp=$!
  sleep 4; /clipt record --stop; wait $rp
  check "two sources with separate tracks: a mix and one per source ($(titles /tmp/snd-2.mkv))" '[ "$(titles /tmp/snd-2.mkv)" = "Mix,System sound,Microphone" ]'
  local s1=$(band_db /tmp/snd-2.mkv 1 1000) s4=$(band_db /tmp/snd-2.mkv 1 440)
  check "the system track has the 1 kHz tone and not the 440 Hz one (${s1} / ${s4} dB)" 'awk "BEGIN{exit !($s1 > -35 && $s1 - $s4 > 20)}"'
  local m1=$(band_db /tmp/snd-2.mkv 2 1000) m4=$(band_db /tmp/snd-2.mkv 2 440)
  check "the microphone track the reverse (${m4} / ${m1} dB)" 'awk "BEGIN{exit !($m4 > -35 && $m4 - $m1 > 20)}"'
  local x1=$(band_db /tmp/snd-2.mkv 0 1000) x4=$(band_db /tmp/snd-2.mkv 0 440)
  check "the mix has both, about equally (${x1} / ${x4} dB)" 'awk "BEGIN{d=($x1)-($x4); exit !($x1 > -35 && $x4 > -35 && d < 3 && d > -3)}"'

  /clipt record -g "$g" --audio=system:100,mic:25 -o /tmp/snd-v.mp4 2>/tmp/snd.err & rp=$!
  sleep 4; /clipt record --stop; wait $rp; rc=$?
  x1=$(band_db /tmp/snd-v.mp4 0 1000); x4=$(band_db /tmp/snd-v.mp4 0 440)
  check "volumes: mic at 25 % sits ~12 dB under system at 100 % ($(awk "BEGIN{print ($x4)-($x1)}") dB; $rc $(grep -iE 'error|can.t|fail|already' /tmp/snd.err | grep -v notification | head -1))" 'awk "BEGIN{d=($x4)-($x1); exit !(d < -9 && d > -15)}"'
  check "without --separate-audio there's just the mix ($(titles /tmp/snd-v.mp4))" '[ "$(titles /tmp/snd-v.mp4)" = Mix ]'

  /clipt record -g "$g" --audio=mic.monitor -o /tmp/snd-n.mp4 2>/tmp/snd.err & rp=$!
  sleep 3.5; /clipt record --stop; wait $rp
  m4=$(band_db /tmp/snd-n.mp4 0 440); m1=$(band_db /tmp/snd-n.mp4 0 1000)
  check "a device by name records it (${m4} dB at 440 Hz, ${m1} at 1 kHz)" 'awk "BEGIN{exit !($m4 > -35 && $m4 - $m1 > 20)}"'
  tone_off

  echo "-- recording: ducking under the microphone"
  # The system's 1 kHz tone plays throughout; the microphone's 440 Hz tone
  # only in the middle, so the 1 kHz band shows the dip. Windows are well
  # clear of PulseAudio's silent first second and of the tone's start and
  # stop, which can each lag by about a second: before 2-3 s, during
  # 5-6.5 s, after 9-10 s.
  tone_on 1000 clipt; sleep 0.5
  CLIPT_DEBUG=1 /clipt record -g "$g" --audio=both --audio-balance=duck -o /tmp/snd-duck.mp4 2>/tmp/snd.err & rp=$!
  sleep 3.5
  mic_tone_on; sleep 4.5
  mic_tone_off; sleep 3.5
  /clipt record --stop; wait $rp; rc=$?
  tone_off
  local b=$(band_db_seg /tmp/snd-duck.mp4 2.0 1.0 0 1000)
  local d=$(band_db_seg /tmp/snd-duck.mp4 5.0 1.5 0 1000)
  local a=$(band_db_seg /tmp/snd-duck.mp4 9.0 1.0 0 1000)
  local v=$(band_db_seg /tmp/snd-duck.mp4 5.0 1.5 0 440)
  check "duck: the system sound is 8 dB lower while the microphone plays (${b} / ${d} / ${a} dB at 1 kHz; microphone ${v} dB; rc=$rc)" \
    'awk "BEGIN{exit !($b > -35 && $a > -35 && $v > -35 && $b - $d >= 8 && $a - $d >= 8 && ($a - $b) < 3 && ($b - $a) < 3)}"'
  grep -E "audio balance" /tmp/snd.err | sed 's/^clipt: /  /'

  # The default, off, must leave the levels as they were.
  tone_on 1000 clipt; sleep 0.5
  /clipt record -g "$g" --audio=both -o /tmp/snd-off.mp4 2>/tmp/snd.err & rp=$!
  sleep 3.5
  mic_tone_on; sleep 4.5
  mic_tone_off; sleep 3.5
  /clipt record --stop; wait $rp
  tone_off
  b=$(band_db_seg /tmp/snd-off.mp4 2.0 1.0 0 1000)
  d=$(band_db_seg /tmp/snd-off.mp4 5.0 1.5 0 1000)
  check "off: the system sound keeps its level while the microphone plays (${b} / ${d} dB)" \
    'awk "BEGIN{exit !($b > -35 && $d > -35 && ($b - $d) < 3 && ($d - $b) < 3)}"'

  # smart ducks the same way, and says at the stop how clearly the voice
  # came through (two tones aren't speech, so only that it says).
  tone_on 1000 clipt; sleep 0.5
  CLIPT_DEBUG=1 /clipt record -g "$g" --audio=both --audio-balance=smart -o /tmp/snd-smart.mp4 2>/tmp/snd.err & rp=$!
  sleep 3.5
  mic_tone_on; sleep 4.5
  mic_tone_off; sleep 3.5
  /clipt record --stop; wait $rp; rc=$?
  tone_off
  d=$(band_db_seg /tmp/snd-smart.mp4 5.0 1.5 0 1000)
  b=$(band_db_seg /tmp/snd-smart.mp4 2.0 1.0 0 1000)
  check "smart: ducks, and reports clarity ($(grep -E 'speech clarity|hears the other' /tmp/snd.err | head -1 | sed 's/^clipt: //'); ${b} / ${d} dB; rc=$rc)" \
    '[ $rc = 0 ] && grep -qE "speech clarity|hears the other sound" /tmp/snd.err && awk "BEGIN{exit !($b - $d >= 8)}"'
  grep -E "clarity:" /tmp/snd.err | sed 's/^clipt: /  /'

  # Noise on the microphone under louder noise from the system: unclear, so
  # smart mixes again with the system quieter, and removes what it kept.
  noise_on clipt 0.5; sleep 0.5
  CLIPT_DEBUG=1 /clipt record -g "$g" --audio=both --audio-balance=smart -o /tmp/snd-remix.mp4 2>/tmp/snd.err & rp=$!
  sleep 2.5
  noise_on mic 0.15; sleep 7
  /clipt record --stop; wait $rp; rc=$?
  noise_off
  local before=$(sed -n 's/.*speech clarity \([0-9.]*\), .*/\1/p' /tmp/snd.err | head -1)
  local after=$(sed -n 's/^clipt: re-mixed: speech clarity \([0-9.]*\).*/\1/p' /tmp/snd.err)
  check "smart: re-mixes an unclear recording (${before:-?} -> ${after:-?}; rc=$rc; $(grep -E 're-mix' /tmp/snd.err | head -1 | sed 's/^clipt: //'))" \
    '[ $rc = 0 ] && [ -n "$before" ] && [ -n "$after" ] && awk "BEGIN{exit !($after > $before)}" && [ "$(a_codec /tmp/snd-remix.mp4)" = aac ] && ! ls -a /tmp | grep -q clipt-sound && ! ls -a /tmp | grep -q clipt-remix'
  grep -E "clarity:" /tmp/snd.err | sed 's/^clipt: /  /'

  # The finished mix normalized to -14 LUFS at the stop; loudnorm needs at
  # least 3 s of sound, and PulseAudio's first second can be silent.
  tone_on 1000 clipt; sleep 0.5
  CLIPT_DEBUG=1 /clipt record -g "$g" --audio=system:50 --audio-loudness -o /tmp/snd-loud.mp4 2>/tmp/snd.err & rp=$!
  sleep 5; /clipt record --stop; wait $rp; rc=$?
  tone_off
  local l=$(lufs /tmp/snd-loud.mp4)
  check "loudness: the mix normalized to -14 LUFS (${l} LUFS; rc=$rc; $(grep -E 'LUFS' /tmp/snd.err | head -1 | sed 's/^clipt: //'))" \
    '[ $rc = 0 ] && awk "BEGIN{exit !($l > -14.7 && $l < -13.3)}"'

  /clipt record -g "$g" --audio=system --audio-balance=duck -o /tmp/snd-nomic.mp4 2>/tmp/snd.err & rp=$!
  sleep 1.5; /clipt record --stop; wait $rp
  check "duck without a microphone says so and records as usual" 'grep -q "audio balance: no microphone to duck under" /tmp/snd.err && [ -s /tmp/snd-nomic.mp4 ]'

  # Refusals run under a time limit: if one isn't refused, the recording
  # would otherwise run until max_duration.
  timeout -s INT 15 /clipt record -g "$g" --audio=system,nosuch -o /tmp/snd-x.mp4 2>/tmp/snd.err
  check "an unknown device is refused, pointing at the list ($(head -1 /tmp/snd.err | cut -c1-70))" 'grep -q "no sound device .nosuch.*list sound" /tmp/snd.err && [ ! -e /tmp/snd-x.mp4 ]'
  timeout -s INT 15 /clipt record -g "$g" --audio=system:900 -o /tmp/snd-x.mp4 2>/tmp/snd.err
  check "an impossible volume is refused ($(head -1 /tmp/snd.err))" 'grep -q "outside 0 to 400" /tmp/snd.err && [ ! -e /tmp/snd-x.mp4 ]'
  PULSE_SERVER=unix:/nonexistent timeout -s INT 15 /clipt record -g "$g" --audio -o /tmp/snd-x.mp4 2>/tmp/snd.err
  check "no sound server: refused before recording ($(head -1 /tmp/snd.err | cut -c1-70))" 'grep -q "can.t record sound" /tmp/snd.err && [ ! -e /tmp/snd-x.mp4 ]'
}
