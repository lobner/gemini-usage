#!/usr/bin/env bash
# Build "Gemini Usage.app" — a no-dock menu-bar agent (LSUIElement) — and offer
# to install it to /Applications.
#
# Usage: ./build/make-app.sh [--install | --no-install]
#
# With no flag it asks. --install and --no-install answer for you, which is also
# what happens when stdin isn't a terminal: it builds and stops.
#
# Installing quits any running copy first (either the one in /Applications or one
# launched from this repo — they share a bundle id), replaces the bundle, and
# relaunches it from /Applications.
set -euo pipefail

cd "$(dirname "$0")/.."

APP="Gemini Usage.app"
EXE="geminiusage"
BUNDLE_ID="dk.biq.geminiusage"
DEST="/Applications"

CONTENTS="$APP/Contents"
MACOS="$CONTENTS/MacOS"

# Validate the flag before building, but leave an interactive prompt until after,
# so nothing is asked about a build that then fails.
answer=""
case "${1:-}" in
--install) answer=y ;;
--no-install) answer=n ;;
"") ;;
*)
	echo "unknown argument: $1 (expected --install or --no-install)" >&2
	exit 2
	;;
esac

rm -rf "$APP"
mkdir -p "$MACOS"

# Stamp the build so the About row can identify itself. git describe gives the
# tag for a release build, "<tag>-<n>-g<sha>" for anything after it, and appends
# -dirty for uncommitted changes, so a build always says what it really is.
VERSION=$(git describe --tags --always --dirty 2>/dev/null || echo dev)
COMMIT=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)
BUILT=$(date -u +%Y-%m-%d)

echo "Building binary… ($VERSION)"
# The stamps reach the binary through option_env! in src/main.rs. Cargo tracks
# those variables, so a new stamp rebuilds the binary. Cargo is found in
# ~/.cargo/bin when it is not on PATH. --target-dir overrides CARGO_TARGET_DIR
# and build.target-dir, so the binary is always where the copy below reads it,
# and never a stale one from an earlier build.
CARGO=$(command -v cargo || echo "$HOME/.cargo/bin/cargo")
GEMINIUSAGE_VERSION=$VERSION GEMINIUSAGE_COMMIT=$COMMIT GEMINIUSAGE_BUILD_DATE=$BUILT \
	"$CARGO" build --release --locked --target-dir target
cp "target/release/$EXE" "$MACOS/$EXE"

cp build/Info.plist "$CONTENTS/Info.plist"

# Tell Finder's Get Info the same thing. CFBundle*Version want a plain dotted
# number, so the tag's numeric part is used and the rest of git describe dropped.
PLIST_VERSION=$(printf '%s' "${VERSION#v}" | sed -E 's/[^0-9.].*$//; s/\.$//')
: "${PLIST_VERSION:=0.0.0}"
for key in CFBundleShortVersionString CFBundleVersion; do
	/usr/libexec/PlistBuddy -c "Set :$key $PLIST_VERSION" "$CONTENTS/Info.plist" >/dev/null 2>&1 || true
done

# Ad-hoc sign with our own identifier. The linker already applies an ad-hoc
# signature, but its identifier is not the bundle id (the Go linker called every
# binary "a.out") - and macOS Background Task Management keys login items by
# signing identity, so two such apps can look like the same item to it, and
# registering one evicts the other's "Open at Login" entry. Signing with -i
# gives this bundle a distinct, stable identity. Still not a Developer ID and
# still not notarised: this buys identity, not trust.
codesign --force --sign - --identifier "$BUNDLE_ID" "$APP" >/dev/null 2>&1 ||
	echo "  warning: could not ad-hoc sign; Open at Login may clash with sibling apps" >&2

echo "Built \"${APP}\""

# Match the process by executable name, not with pgrep -f: a full-command-line
# match also hits any shell whose arguments happen to mention the bundle path,
# including the one running this script.
running() { pgrep -x "$EXE" >/dev/null 2>&1; }

# wait_gone polls for up to 5 s, since quitting is asynchronous.
wait_gone() {
	for _ in $(seq 20); do
		running || return 0
		sleep 0.25
	done
	return 1
}

quit_running() {
	running || return 0
	echo "Quitting the running app…"
	osascript -e "quit app id \"${BUNDLE_ID}\"" >/dev/null 2>&1 || true
	wait_gone && return 0

	echo "  it did not quit on its own; terminating it"
	pkill -x "$EXE" >/dev/null 2>&1 || true
	wait_gone && return 0

	echo "  still running — quit it from its menu, then re-run this script" >&2
	return 1
}

if [[ -z "$answer" ]]; then
	if [[ -t 0 ]]; then
		read -r -p "Install to ${DEST} and relaunch? [Y/n] " answer || answer=n
	else
		answer=n # non-interactive: build only, don't touch /Applications
	fi
fi

case "${answer:-y}" in
[Yy]* | "")
	quit_running
	echo "Installing to ${DEST}/${APP}…"
	rm -rf "${DEST:?}/${APP:?}"
	if ! cp -R "$APP" "$DEST/"; then
		echo "Could not write to ${DEST} — copy it yourself, or re-run with sudo." >&2
		exit 1
	fi
	open "${DEST}/${APP}"
	echo "Installed and relaunched \"${DEST}/${APP}\""
	;;
*)
	echo "Not installed. To do it yourself:"
	echo "  cp -R \"${APP}\" ${DEST}/ && open \"${DEST}/${APP}\""
	echo "Or just try this build in place:  open \"${APP}\""
	;;
esac
