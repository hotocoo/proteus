#!/bin/sh
# The no-connect gate (blueprint section 9): runs emu with a guest that stays idle, under strace,
# and fails if emu opened any network connection or sent any datagram to an address that is not
# this machine's loopback. Linux only.
#
# usage: scripts/no-connect.sh <emu> <seconds> <emu arguments...>
set -eu

emu=$1
seconds=$2
shift 2
work=$(mktemp -d)
socket=$work/ctl.sock
trace=$work/trace.txt

strace -f -qq -e trace=connect,sendto,sendmsg -o "$trace" \
    "$emu" "$@" --ctl "$socket" </dev/null >"$work/console.txt" 2>&1 &
sleep "$seconds"
"$emu" ctl "$socket" stop
wait

echo "calls traced: $(wc -l <"$trace")"
# Every IPv4 or IPv6 address emu sent to or connected to, other than loopback.
outside=$(grep -oE 'inet_addr\("[^"]+"\)|inet_pton\(AF_INET6, "[^"]+"' "$trace" |
    grep -vE '"127\.[0-9.]+"|"::1"' || true)
if [ -n "$outside" ]; then
    echo "emu reached outside this machine:"
    echo "$outside" | sort | uniq -c
    exit 1
fi
echo "no connection or datagram left this machine"
