#!/usr/bin/env bash
set -euo pipefail
# End-to-end check of "Hear apps" while tutor is running: plays a quiet 3 s
# 1 kHz tone as an app on the kid's PC (they hear a faint beep), records it on
# their app sink and as it arrives here, and reports level, dropouts, clipping
# and purity for both. Both lines should match.
#
#   tools/audio-check.sh "Logan's PC" 192.168.8.106

NAME="${1:-Logan's PC}"
HOST="${2:-192.168.8.106}"
OUT=$(mktemp -d)
HERE=$(dirname "$0")

si=$(pactl list sink-inputs | awk -v n="application.name = \"$NAME apps\"" '/^Sink Input/{id=substr($3,2)} index($0,n){print id; exit}')
[ -n "$si" ] || { echo "no \"$NAME apps\" stream here - is tutor running with Hear apps on?" >&2; exit 1; }
orig=$(pactl list sink-inputs | awk -v id="$si" '/^Sink Input/{cur=substr($3,2)} cur==id && /Sink:/{print $2; exit}')
mod=$(pactl load-module module-null-sink sink_name=tutor_check)
trap 'pactl move-sink-input "$si" "$orig" 2>/dev/null; pactl unload-module "$mod"' EXIT
pactl move-sink-input "$si" tutor_check

(timeout 7 parecord -d tutor_check.monitor --format=s16le --rate=48000 --channels=2 --file-format=wav "$OUT/here.wav" &)
ssh -o BatchMode=yes "$HOST" 'export XDG_RUNTIME_DIR=/run/user/$(id -u)
  (timeout 7 parecord -d tutor_apps.monitor --format=s16le --rate=48000 --channels=2 --file-format=wav /tmp/tutor-check.wav &)
  sleep 1.5
  gst-launch-1.0 -q audiotestsrc freq=1000 volume=0.1 num-buffers=150 ! audio/x-raw,rate=48000,channels=2 ! pulsesink client-name="tutor test tone"
  sleep 3'
sleep 1.5
scp -q "$HOST:/tmp/tutor-check.wav" "$OUT/theirs.wav"
python3 "$HERE/analyze_tone.py" "$OUT/theirs.wav" "$OUT/here.wav"
