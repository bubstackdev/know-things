"""Shared layout contract for a bundled embedding runtime."""
EMBEDDING_RUNTIME_LAUNCHER = '''#!/bin/sh
set -eu
asset_dir=${0%/*}
if [ "$asset_dir" = "$0" ]; then asset_dir=.; fi
case "$asset_dir" in /*) ;; *) asset_dir="./$asset_dir" ;; esac
asset_dir=$(CDPATH= cd -P "$asset_dir" && pwd)
exec "$asset_dir/runtime/python-runtime" "$asset_dir/embedding.py" "$@"
'''
