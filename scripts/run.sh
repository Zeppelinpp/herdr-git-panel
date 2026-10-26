#!/bin/sh
set -eu
root="${HERDR_PLUGIN_ROOT:-$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)}"
exec "$root/bin/herdr-git-panel" "$@"
