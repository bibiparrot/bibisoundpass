#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if [[ -f "$HOME/.cargo/env" ]]; then
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
fi

cargo build --release --locked --bins >/dev/null
test_dir="$(mktemp -d)"
module_id=""
receiver_pid=""

cleanup() {
  if [[ -n "$receiver_pid" ]]; then
    kill "$receiver_pid" >/dev/null 2>&1 || true
  fi
  if [[ -n "$module_id" ]]; then
    pactl unload-module "$module_id" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

module_id="$(pactl load-module module-null-sink \
  sink_name=soundpass_test \
  sink_properties=device.description=SoundPass_Test \
  rate=48000 channels=1)"
mkdir -p "$test_dir/out"

target/release/soundpass-recv \
  --device soundpass_test \
  --max-seconds 90 \
  --out "$test_dir/out" \
  >"$test_dir/receiver.stdout" \
  2>"$test_dir/receiver.stderr" &
receiver_pid="$!"

sleep 1
if ! target/release/soundpass-send README.md \
  --device soundpass_test \
  --repair-percent 40 \
  --passes 2 \
  >"$test_dir/sender.stdout" \
  2>"$test_dir/sender.stderr"; then
  cat "$test_dir/sender.stderr" >&2
  exit 1
fi

if ! wait "$receiver_pid"; then
  cat "$test_dir/receiver.stderr" >&2
  exit 1
fi
receiver_pid=""

cmp README.md "$test_dir/out/README.md"
cat "$test_dir/sender.stderr"
cat "$test_dir/receiver.stderr"
cat "$test_dir/receiver.stdout"
echo "Linux PulseAudio loopback round trip passed: $test_dir"
