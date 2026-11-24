#!/bin/sh
set -eu

if [ "$#" -lt 2 ] || [ "$#" -gt 4 ]; then
    echo 'Usage: sh scripts/render-ops-agent.sh /path/to/know-things /path/to/docker [context [container]]' >&2
    exit 2
fi
for executable in "$1" "$2"; do
    case "$executable" in
        /*) ;;
        *) echo 'Executable paths must be absolute.' >&2; exit 2 ;;
    esac
    if [ ! -x "$executable" ]; then
        echo "Executable is unavailable: $executable" >&2
        exit 2
    fi
done

log_dir=${KB_AGENT_LOG_DIR:-"$HOME/Library/Logs/know-things"}
case "$log_dir" in
    /*) ;;
    *) echo 'Agent log directory must be absolute.' >&2; exit 2 ;;
esac
install -d -m 700 "$log_dir"
plist=$(mktemp -t know-things-ops-agent)
trap 'rm -f "$plist"' EXIT
plutil -create xml1 "$plist"
plutil -insert Label -string local.know-things.ops "$plist"
plutil -insert StartInterval -integer 15 "$plist"
plutil -insert RunAtLoad -bool true "$plist"
plutil -insert StandardErrorPath -string "$log_dir/ops.stderr.log" "$plist"
plutil -insert ProgramArguments -array "$plist"
index=0
for arg in "$1" relay-ops --docker "$2" --context "${3:-desktop-linux}" --container "${4:-know-things}"; do
    plutil -insert "ProgramArguments.$index" -string "$arg" "$plist"
    index=$((index + 1))
done
cat "$plist"
