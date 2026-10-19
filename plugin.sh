#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/input.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/input" ;;
    *) rel="x86_64-apple-darwin/input" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/input" ;;
    *) rel="x86_64-unknown-linux-musl/input" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/input.exe" "$here/target/release/input" "$here/target/debug/input.exe" "$here/target/debug/input"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "input binary not found in $here" >&2
exit 1
