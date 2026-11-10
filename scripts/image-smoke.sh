#!/bin/sh
# Smoke test of the Docker image's bundled tools. The image's ffmpeg only has
# the components listed in the Dockerfile, so this runs the calls of
# crates/media/src/ffmpeg.rs and yt-dlp's merge/extract steps on samples in
# the formats the sites serve, and checks that yt-dlp finds QuickJS.
#
# Samples are encoded with Debian's full ffmpeg in a throwaway container.
# Needs Docker and network access (apt). Usage: scripts/image-smoke.sh [image]
set -eu

IMAGE=${1:-feedbot:test}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
chmod 777 "$WORK"

echo "== samples"
docker run --rm -v "$WORK:/w" -w /w -e OWNER="$(id -u):$(id -g)" debian:trixie-slim sh -euc '
apt-get update -qq >/dev/null
apt-get install -y -qq --no-install-recommends ffmpeg >/dev/null 2>&1
F="ffmpeg -hide_banner -loglevel error -y"
SRC="-f lavfi -i testsrc2=size=640x360:rate=25:duration=2"
AUD="-f lavfi -i sine=frequency=440:duration=2"
FRAG="-f mp4 -movflags frag_keyframe+empty_moov+default_base_moof"
$F $SRC -c:v libx264 -an $FRAG v.m4s                      # Bilibili DASH video
$F $AUD -c:a aac -vn $FRAG a.m4s                          # Bilibili DASH audio
$F $SRC $AUD -c:v libx264 -c:a aac -shortest durl.flv     # Bilibili durl
$F $SRC -c:v libvpx-vp9 -deadline realtime -an v.webm     # YouTube VP9
$F $AUD -c:a libopus -vn a.webm                           # YouTube Opus
$F $SRC -c:v libsvtav1 -preset 12 -an av1.mp4 2>/dev/null # AV1 (SVT logs to stderr)
$F $SRC -c:v libx265 -an -x265-params log-level=none hevc.mp4
for i in 1 2 3; do                                        # odd-sized ugoira frames
  $F -f lavfi -i testsrc2=size=101x75:rate=1:duration=5 -ss $i -frames:v 1 00000$i.jpg
done
$F -f lavfi -i testsrc2=size=101x75 -frames:v 1 000004.png
chown "$OWNER" ./*
'

echo "== checks in $IMAGE"
docker run --rm -v "$WORK:/w" -w /w --entrypoint sh "$IMAGE" -uc '
F="ffmpeg -hide_banner -loglevel error -y"
failed=0
ok() {
  if sh -c "$2" >/dev/null 2>err.txt; then
    echo "OK   $1"
  else
    echo "FAIL $1: $(tail -3 err.txt | tr "\n" " ")"
    failed=1
  fi
}
# yt-dlp itself, with the runtime the image configures.
ok "yt-dlp finds $FEEDBOT__YTDLP__JS_RUNTIME" \
  "yt-dlp -v --ignore-config --no-js-runtimes --js-runtimes $FEEDBOT__YTDLP__JS_RUNTIME --simulate https://example.invalid/ 2>&1 | grep -q \"JS runtimes: quickjs\""
# FfmpegCli::remux, probe, thumbnail.
ok "remux DASH h264+aac" "$F -i v.m4s -i a.m4s -c copy -movflags +faststart bili.mp4"
ok "remux flv"           "$F -i durl.flv -c copy -movflags +faststart flv.mp4"
ok "ffprobe json"        "ffprobe -v quiet -print_format json -show_format -show_streams bili.mp4 | grep -q \"\\\"width\\\": 640\""
ok "thumbnail"           "$F -ss 0 -i bili.mp4 -vframes 1 -vf \"scale='\''min(320,iw)'\'':-2\" -q:v 3 thumb.jpg"
# yt-dlp post-processing: merge VP9+Opus into mp4, extract m4a.
ok "merge vp9+opus"      "$F -i v.webm -i a.webm -map 0:v:0 -map 1:a:0 -c copy -movflags +faststart yt.mp4"
ok "extract m4a"         "$F -i a.webm -vn -c:a aac -b:a 128k -f ipod audio.m4a"
# FfmpegCli::transcode from every source codec.
for src in bili.mp4 av1.mp4 hevc.mp4 v.webm yt.mp4; do
  ok "transcode $src" "$F -i $src -c:v libx264 -preset veryfast -b:v 800k -c:a aac -b:a 96k -vf \"scale=-2:'\''min(720,ih)'\''\" -movflags +faststart tc_$src.mp4"
done
# FfmpegCli::ugoira_to_mp4 (concat demuxer, odd sizes).
printf "ffconcat version 1.0\nfile 000001.jpg\nduration 0.100\nfile 000002.jpg\nduration 0.250\nfile 000003.jpg\nduration 0.100\nfile 000003.jpg\n" > jpg.ffconcat
printf "ffconcat version 1.0\nfile 000004.png\nduration 0.100\nfile 000004.png\nduration 0.100\nfile 000004.png\n" > png.ffconcat
for l in jpg png; do
  ok "ugoira $l frames" "$F -f concat -safe 0 -i $l.ffconcat -fps_mode vfr -c:v libx264 -preset veryfast -pix_fmt yuv420p -vf \"scale=trunc(iw/2)*2:trunc(ih/2)*2\" -an -movflags +faststart ugoira_$l.mp4"
done
exit $failed
'
