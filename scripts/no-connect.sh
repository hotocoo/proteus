#!/bin/sh
# The no-connect gate (blueprint section 9): runs proteus with a guest that stays idle, under strace,
# and fails if proteus opened any network connection or sent any datagram to an address that is not
# this machine's loopback. Linux only.
#
# usage: scripts/no-connect.sh <proteus> <seconds> <proteus arguments...>
set -eu

proteus=$1
seconds=$2
shift 2
work=$(mktemp -d)
socket=$work/ctl.sock
trace=$work/trace.txt

strace -f -qq -e trace=connect,sendto,sendmsg -o "$trace" \
    "$proteus" "$@" --ctl "$socket" </dev/null >"$work/console.txt" 2>&1 &
sleep "$seconds"
"$proteus" ctl "$socket" stop
wait

echo "calls traced: $(wc -l <"$trace")"
# Every IPv4 or IPv6 address proteus sent to or connected to, other than loopback.
outside=$(grep -oE 'inet_addr\("[^"]+"\)|inet_pton\(AF_INET6, "[^"]+"' "$trace" |
    grep -vE '"127\.[0-9.]+"|"::1"' || true)
if [ -n "$outside" ]; then
    echo "proteus reached outside this machine:"
    echo "$outside" | sort | uniq -c
    exit 1
fi
echo "no connection or datagram left this machine"
