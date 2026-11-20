#!/usr/bin/env bash
# Compares the Rust agent's help and usage errors with the Go agent's, command by
# command. The fixtures (testdata/help, testdata/errors) were captured from the Go
# binary before the cutover and are the reference: `cargo test -p agent`
# (tests/help_fixtures.rs) compares against them, and they stay as they are.
#
#   help_diff.sh RUST_BIN                 diff RUST_BIN against the fixtures
#   help_diff.sh RUST_BIN GO_BIN          diff RUST_BIN against GO_BIN directly
#   help_diff.sh --update GO_BIN          rewrite the fixtures from GO_BIN
#
# Only help and lines that fail before the command runs are run: never a command
# that does anything (no setup, no store, no termd, no bus). GO_BIN is optional,
# for a live comparison: the Go source is in legacy/ (legacy/README.md). Build it
# into a scratch directory, since the data dir is the one beside the binary:
#   (cd legacy && go build -o /var/tmp/go-agent/agent ./cmd/agent)
#
# logs's help names the binary's data dir (<binary's dir>/.gomuxai); both sides
# print it as "<data dir>".
set -u
here=$(cd "$(dirname "$0")/.." && pwd)
data=$here/testdata
paths=$data/help/paths.txt
cases=$data/errors/cases.txt

# One command path per line of paths.txt ("" is agent itself); fixture names join
# the words with "_".
fixture_name() { if [ -z "$1" ]; then echo agent; else echo "${1// /_}"; fi; }

# What bin prints for these words, its data dir as "<data dir>".
run() { # bin, words...
	local bin=$1 dir
	shift
	dir=$(dirname "$(readlink -f "$bin")")/.gomuxai
	"$bin" "$@" | sed "s|$dir|<data dir>|g"
	return "${PIPESTATUS[0]}"
}

# Commands the Rust build has and the Go never had, by name (the same list as
# RUST_ONLY in help_fixtures.rs). The Rust root help lists them and the Go's
# doesn't, so they are left out of the Rust side's root help before comparing,
# with a group that lists nothing else; nothing else is.
rust_only="run revive fleet focus template skill"
without_rust_only() {
	awk -v names="$rust_only" '
	BEGIN { RS = ""; n = split(names, list, " "); for (i = 1; i <= n; i++) only[list[i]] = 1 }
	{
		k = split($0, lines, "\n"); out = lines[1]; kept = 0; dropped = 0
		group = (lines[1] ~ /^[^ ].*:$/ && k > 1)
		for (i = 2; i <= k; i++) {
			split(lines[i], w, " ")
			if (group && lines[i] ~ /^  [^ ]/ && (w[1] in only)) { dropped++; continue }
			out = out "\n" lines[i]; kept++
		}
		if (dropped && !kept) next
		printf "%s%s", sep, out; sep = "\n\n"
	}
	END { if (NR) printf "\n" }'
}

# Help fixtures whose text the Rust build changes on purpose, by name (the same
# list as RUST_CHANGED in help_fixtures.rs): rust_changed/<name>.go holds the
# Go's lines, <name>.rust what the Rust prints instead. The Go's lines are
# replaced once in the help wanted, and must be there. friction, friction_add:
# the friction kinds brief, handover and skill (docs/FLEET.md). spawn:
# --handoff (docs/FLEET.md, "Revive"). bus, bus_run, bus_start, bus_restart:
# --remote-ops defaults to agent_bus's DEFAULT_REMOTE_OPS, which adds fleet
# and template.
rust_changed="friction friction_add spawn bus bus_run bus_start bus_restart"
with_rust_changes() { # name, < the Go's help > the help wanted from the Rust
	local name=$1 text from to
	text=$(cat; printf x)
	text=${text%x}
	case " $rust_changed " in
	*" $name "*)
		from=$(cat "$data/rust_changed/$name.go"; printf x)
		from=${from%x}
		to=$(cat "$data/rust_changed/$name.rust"; printf x)
		to=${to%x}
		case $text in
		*"$from"*) text=${text/"$from"/"$to"} ;;
		*) echo "rust_changed/$name.go isn't in the Go's help for $name" >&2 ;;
		esac
		;;
	esac
	printf '%s' "$text"
}

# Each line of cases.txt is a command line (words split on spaces) that fails or
# prints help before anything runs. Its fixture holds the exit code, stdout and
# stderr. The Rust side's stdout goes through without_rust_only (a line can
# print the root help: agent -h send).
run_case() { # bin, line, [stdout filter]
	local out err code
	out=$(mktemp -p /var/tmp) err=$(mktemp -p /var/tmp)
	# shellcheck disable=SC2086
	run "$1" $2 >"$out" 2>"$err"
	code=$?
	printf '%s\n--- stdout\n' "$code"
	${3:-cat} <"$out"
	printf -- '--- stderr\n'
	cat "$err"
	rm -f "$out" "$err"
}

if [ "${1:-}" = --update ]; then
	go=$2
	mkdir -p "$data/help" "$data/errors"
	while IFS= read -r p; do
		# shellcheck disable=SC2086
		run "$go" help $p >"$data/help/$(fixture_name "$p").txt"
	done <"$paths"
	n=0
	while IFS= read -r line; do
		n=$((n + 1))
		run_case "$go" "$line" >"$data/errors/$n.txt"
	done <"$cases"
	echo "fixtures written from $go"
	exit 0
fi

rust=$1
go=${2:-}
fail=0 same=0
check() { # label, want-file, got-file
	if cmp -s "$2" "$3"; then
		same=$((same + 1))
	else
		fail=$((fail + 1))
		echo "DIFF: $1"
		diff "$2" "$3" | head -20
	fi
}
tmp=$(mktemp -d -p /var/tmp)
trap 'rm -rf "$tmp"' EXIT
while IFS= read -r p; do
	name=$(fixture_name "$p")
	want=$tmp/want-$name
	if [ -n "$go" ]; then
		# shellcheck disable=SC2086
		run "$go" help $p | with_rust_changes "$name" >"$want"
	else
		with_rust_changes "$name" <"$data/help/$name.txt" >"$want"
	fi
	filter=cat
	[ -z "$p" ] && filter=without_rust_only
	# shellcheck disable=SC2086
	run "$rust" help $p | $filter >"$tmp/help-$name"
	check "agent help $p" "$want" "$tmp/help-$name"
	# shellcheck disable=SC2086
	run "$rust" $p --help | $filter >"$tmp/flag-$name"
	check "agent $p --help" "$want" "$tmp/flag-$name"
done <"$paths"
n=0
while IFS= read -r line; do
	n=$((n + 1))
	want=$data/errors/$n.txt
	if [ -n "$go" ]; then
		want=$tmp/want-case-$n
		run_case "$go" "$line" >"$want"
	fi
	run_case "$rust" "$line" without_rust_only >"$tmp/case-$n"
	check "agent $line" "$want" "$tmp/case-$n"
done <"$cases"
echo "help-diff: $same identical, $fail different"
[ "$fail" -eq 0 ]
