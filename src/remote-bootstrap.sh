set -eu
umask 077
printf 'STALKER-BOOTSTRAP-1 %s %s\n' "$(uname -s)" "$(uname -m)"
IFS= read -r size
IFS= read -r expected
case "$size" in ''|*[!0-9]*) echo 'stalker: invalid helper size' >&2; exit 1;; esac
case "$expected" in ''|*[!0-9a-f]*) echo 'stalker: invalid helper digest' >&2; exit 1;; esac
[ "$size" -gt 0 ] && [ "$size" -le 67108864 ] && [ "${#expected}" -eq 64 ]
directory=$(mktemp -d /tmp/stalker.XXXXXXXXXX)
pid=
cleanup() {
    if [ -n "$pid" ]; then
        kill "$pid" 2>/dev/null || :
        wait "$pid" 2>/dev/null || :
    fi
    rm -rf -- "$directory"
}
trap cleanup 0
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
chmod 700 "$directory"
# Read exactly the payload, leaving the framed configuration on stdin.
# bs=1 is portable across GNU and BSD dd, including short reads from pipes.
dd bs=1 count="$size" of="$directory/agent" 2>/dev/null
case "$(uname -s)" in
    Linux) actual=$(sha256sum "$directory/agent");;
    Darwin) actual=$(shasum -a 256 "$directory/agent");;
    *) echo 'stalker: unsupported remote OS' >&2; exit 1;;
esac
actual=${actual%% *}
if [ "$actual" != "$expected" ]; then
    echo 'stalker: remote helper SHA-256 verification failed' >&2
    exit 1
fi
chmod 500 "$directory/agent"
# An explicit saved descriptor prevents background-shell stdin becoming /dev/null.
exec 3<&0
"$directory/agent" <&3 3<&- &
pid=$!
exec 3<&-
status=0
wait "$pid" || status=$?
pid=
exit "$status"
