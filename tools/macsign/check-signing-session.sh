#!/bin/sh
# Probe whether the current macOS shell can sign binaries with the login
# keychain (as `make macos` requires). Safe and read-only: signs only a
# temporary copy of /usr/bin/true and removes it on exit.
set -eu

if [ "$(uname -s)" != "Darwin" ]; then
	echo "check-signing-session: macOS only (found $(uname -s))" >&2
	exit 1
fi

session="$(launchctl managername 2>/dev/null || echo unknown)"
echo "launchctl managername: $session"

keychain="$(security default-keychain -d user 2>/dev/null | tr -d '[:space:]"')"
if [ -n "$keychain" ]; then
	echo "default keychain:      $keychain"
	if info="$(security show-keychain-info "$keychain" 2>&1)"; then
		echo "keychain status:       unlocked (${info:-ok})"
	else
		echo "keychain status:       $info"
	fi
fi

id="${CODESIGN_IDENTITY:-}"
if [ -z "$id" ]; then
	id="$(security find-identity -v -p codesigning 2>/dev/null | awk '/^ *[0-9]+\)/{print $2; exit}')"
fi

if [ -z "$id" ]; then
	echo "codesign identity:     none valid" >&2
	security find-identity -p codesigning 2>&1 | sed 's/^/  /' >&2
	exit 1
fi
echo "codesign identity:     $id"

tmpdir="$(mktemp -d "${TMPDIR:-/tmp}/gomuxai-macsign.XXXXXX")"
trap 'rm -rf "$tmpdir"' EXIT INT TERM
cp /usr/bin/true "$tmpdir/probe"

if codesign --force --options runtime --sign "$id" "$tmpdir/probe" &&
	codesign --verify --strict "$tmpdir/probe"; then
	echo "result:                OK (signed and verified scratch binary)"
	exit 0
fi

echo "result:                FAILED" >&2
if [ "$session" != "Aqua" ]; then
	echo "hint: managername is '$session' (not 'Aqua'); start g3d-termd from a Mac GUI terminal or install tools/macsign/com.gomuxai.termd-agents.plist." >&2
fi
exit 1
