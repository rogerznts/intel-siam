#!/usr/bin/env bash
# Reap leaked Ghidra/REA analysis processes so they cannot eat the machine's memory.
#
#   tools/re/reap.sh             kill leaks, report what was done (silent when nothing)
#   tools/re/reap.sh --dry-run   only report
#   tools/re/reap.sh --throttle  at most one scan per minute (used by hooks before every tool call)
#
# A leak is:
#   - a headless Ghidra JVM (class ghidra.app.util.headless.AnalyzeHeadless — REA sessions and
#     tools/re/decompile.sh) whose parent died (PPID 1);
#   - a Ghidra native `decompile` whose JVM died (PPID 1), or that has burnt more than
#     REAP_DECOMPILE_CPU_MIN minutes of CPU (default 30): a single function decompiles in seconds,
#     a runaway loop does not stop (one reached 70 GB);
#   - an `rea mcp` server whose client died (PPID 1), with its engine;
#   - a $TMPDIR/rea-ghidra-* session directory that no live process references.
# The Ghidra GUI (ghidra.GhidraRun) is never touched.
set -uo pipefail

DRY=0; THROTTLE=0
for a in "$@"; do
	case "$a" in
		--dry-run) DRY=1 ;;
		--throttle) THROTTLE=1 ;;
		*) echo "usage: $0 [--dry-run] [--throttle]" >&2; exit 2 ;;
	esac
done
CPU_MAX_MIN="${REAP_DECOMPILE_CPU_MIN:-30}"
TMP="${TMPDIR:-/tmp}"; TMP="${TMP%/}"
STAMP="$TMP/intel-siam-reap.stamp"

if [ "$THROTTLE" -eq 1 ] && [ -f "$STAMP" ]; then
	now=$(date +%s); last=$(stat -f %m "$STAMP" 2>/dev/null || stat -c %Y "$STAMP" 2>/dev/null || echo 0)
	[ $((now - last)) -lt 60 ] && exit 0
fi
[ "$DRY" -eq 1 ] || touch "$STAMP"

reap() { # pid reason
	echo "reap: $1 — $2"
	[ "$DRY" -eq 1 ] && return
	kill "$1" 2>/dev/null
	KILLED+=("$1")
}

KILLED=()
# One pass over the process table, in awk (macOS bash 3.2 has no associative arrays). A headless JVM is a leak
# when, walking up through Ghidra's own launchers (analyzeHeadless -> launch.sh -> java), the first
# non-launcher ancestor is PID 1; the launchers go with it. decompile is reaped with its JVM, when orphaned,
# or past the CPU budget.
LEAKS="$(ps -axo pid=,ppid=,time=,command= | awk -v cpu_max="$CPU_MAX_MIN" '
	function minutes(t,   d, n, p) {
		sub(/\..*/, "", t); d = 0
		if (index(t, "-")) { d = substr(t, 1, index(t, "-") - 1); t = substr(t, index(t, "-") + 1) }
		n = split(t, p, ":")
		if (n == 3) return (d * 24 + p[1]) * 60 + p[2]
		if (n == 2) return d * 1440 + p[1]
		return d * 1440
	}
	function launcher(c) { return c ~ /ghidra\/support\/(analyzeHeadless|launch\.sh|pyghidraRun)/ }
	{
		pid = $1; ppid[pid] = $2; cpu[pid] = $3
		c = $0; sub(/^ *[0-9]+ +[0-9]+ +[^ ]+ +/, "", c); cmd[pid] = c
	}
	END {
		for (p in cmd) {
			if (cmd[p] ~ /^[^ ]*\/java .*ghidra\.app\.util\.headless\.AnalyzeHeadless/ && !launcher(cmd[p])) {
				a = ppid[p]; chain = ""
				while (a != 1 && (a in cmd) && launcher(cmd[a])) { chain = chain " " a; a = ppid[a] }
				if (a == 1) { print p "\torphaned headless Ghidra JVM"; dead[p] = 1
					n = split(chain, ls, " "); for (i = 1; i <= n; i++) print ls[i] "\tits Ghidra launcher" }
			}
		}
		for (p in cmd) {
			if (cmd[p] ~ /Decompiler\/(build\/)?os\/[^\/]+\/decompile/) {
				m = minutes(cpu[p])
				if (ppid[p] == 1) print p "\torphaned Ghidra decompile"
				else if (ppid[p] in dead) print p "\tdecompile of an orphaned JVM"
				else if (m >= cpu_max) print p "\trunaway Ghidra decompile (" m " min CPU)"
			} else if (cmd[p] ~ /(node.*\/rea mcp|npm exec rea mcp)/ && ppid[p] == 1) {
				print p "\trea mcp server without client"
			}
		}
	}')"
while IFS=$'\t' read -r pid reason; do
	[ -n "$pid" ] && reap "$pid" "$reason"
done <<< "$LEAKS"

if [ "${#KILLED[@]}" -gt 0 ]; then
	sleep 2
	for k in "${KILLED[@]}"; do kill -0 "$k" 2>/dev/null && kill -9 "$k" 2>/dev/null; done
fi

# session directories nobody references any more
LIVE="$(ps -axo command= | grep -oE 'rea-ghidra-[A-Za-z0-9]+' | sort -u)"
for d in "$TMP"/rea-ghidra-*; do
	[ -d "$d" ] || continue
	name="$(basename "$d")"
	printf '%s\n' "$LIVE" | grep -qx "$name" && continue
	echo "reap: $d — no live process"
	[ "$DRY" -eq 1 ] || rm -rf "$d"
done
exit 0
