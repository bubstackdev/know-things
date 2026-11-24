#!/bin/sh
set -eu

if [ "$#" -lt 5 ] || [ "$#" -gt 6 ]; then
    echo 'Usage: sh scripts/render-sync-agent.sh /absolute/path/to/know-things /absolute/path/to/mirror /absolute/path/to/source git-remote https-repo-url [interval-seconds]' >&2
    exit 2
fi
for arg in "$1" "$2" "$3"; do
    case "$arg" in
        /*) ;;
        *) echo 'All paths must be absolute.' >&2; exit 2 ;;
    esac
done
interval=${6-900}
case "$interval" in
    ''|*[!0-9]*) echo 'Interval must be a positive integer in seconds.' >&2; exit 2 ;;
esac
if ! [ "$interval" -gt 0 ] || ! [ "$interval" -le 4294967295 ]; then
    echo 'Interval is outside the supported range.' >&2
    exit 2
fi
if [ ! -x "$1" ]; then
    echo 'Build the native know-things binary before rendering the LaunchAgent.' >&2
    exit 2
fi

log_dir=${KB_AGENT_LOG_DIR:-"$HOME/Library/Logs/know-things"}
case "$log_dir" in
    /*) ;;
    *) echo 'Agent log directory must be absolute.' >&2; exit 2 ;;
esac
install -d -m 700 "$log_dir"
plist=$(mktemp -t know-things-launch-agent)
trap 'rm -f "$plist"' EXIT
plutil -create xml1 "$plist"
plutil -insert Label -string local.know-things.sync "$plist"
plutil -insert StartInterval -integer "$interval" "$plist"
plutil -insert RunAtLoad -bool true "$plist"
plutil -insert StandardErrorPath -string "$log_dir/sync.stderr.log" "$plist"
plutil -insert ProgramArguments -array "$plist"
index=0
for arg in "$1" sync --mirror "$2" --source-dir "$3" --remote "$4" --repo-url "$5" --check-interval-seconds "$interval"; do
    plutil -insert "ProgramArguments.$index" -string "$arg" "$plist"
    index=$((index + 1))
done
cat "$plist"
