#!/usr/bin/env bash
# Reuse a trusted official AppDir's complete runtime, helper, resources and licenses.
# Build the shell with --features fork-no-updates BEFORE calling this script.
set -euo pipefail
if [ "$#" -ne 5 ]; then
  echo "Usage: $0 OFFICIAL_APPDIR FORK_SHELL OUTPUT.AppImage APPIMAGETOOL PATCHELF" >&2
  exit 2
fi
base=$(realpath "$1")
binary=$(realpath "$2")
output=$(realpath -m "$3")
tool=$(realpath "$4")
patchelf=$(realpath "$5")
stage=$(mktemp -d /tmp/opencode/digup-fork-appdir-XXXXXX)
trap 'rm -rf "$stage"' EXIT
test -f "$base/usr/lib/DigUp/helper/digup"
test -f "$base/AppRun"
# Fail closed if accidentally handed a normal upstream-updating build.
if ! grep -aq 'Upstream updates are disabled in this fork' "$binary"; then
  echo 'Expected a shell built with --features fork-no-updates; refusing upstream updates.' >&2
  exit 1
fi
cp -a "$base/." "$stage/"
install -m755 "$binary" "$stage/usr/bin/digup-shell"
"$patchelf" --set-rpath '$ORIGIN/../lib' "$stage/usr/bin/digup-shell"
test "$("$patchelf" --print-rpath "$stage/usr/bin/digup-shell")" = '$ORIGIN/../lib'
sed -i 's/^Name=DigUp$/Name=DigUp Low Memory (fork)/' "$stage/usr/share/applications/"*.desktop
cp "$(dirname "$0")/../docs/linux-low-memory.md" "$stage/LOW-MEMORY-FORK.md"
# AppImage extraction may make directories owner-only; mounted packages must be readable.
chmod -R a+rX "$stage"
mkdir -p "$(dirname "$output")"
ARCH=x86_64 "$tool" "$stage" "$output"
chmod +x "$output"
sha256sum "$output" > "$output.sha256"
cat "$output.sha256"
