#!/usr/bin/env bash
# Trusts one measure repo under /tmp/d2 for rizr, as a user who answers the trust prompt once (D2 measure only).
set -euo pipefail
dir=$(realpath -e "${1:?usage: measure-trust.sh /tmp/d2/<repo>}")
case "$dir" in /tmp/d2/?*) ;; *) echo "refused: $dir is not under /tmp/d2" >&2; exit 2 ;; esac
cd "$dir"
XDG_DATA_HOME=/tmp/d2/data script -qec "/data/rizr/target/debug/rizr trust" /dev/null <<<y
