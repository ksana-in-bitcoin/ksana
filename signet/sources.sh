#!/usr/bin/env bash
# The sources the hosted signet's images are built from (signet/Dockerfile): this repository's committed HEAD, and the
# web wallet's and the testnet points' own repositories, ksana-web and ksana-points, each at its committed HEAD with
# the part of this repository it pins as its core submodule. signet/deploy.sh pushes them to the server, and
# signet/rehearse.sh builds them on this machine:
#
#   .                   this repository at HEAD
#   web/                ksana-web at HEAD
#   web/core/sdk/ts     the TypeScript SDK at the commit ksana-web pins, which the page is built with, so that the
#                       page's commit alone gives the same files (the "Verify this page" section of its README)
#   points/             ksana-points at HEAD (with --points)
#   points/core/        at the commit ksana-points pins: the SDK, the contract SDK, the example contracts, the
#                       playground's sources (its ABI codec, the examples' builds) and the toolchain file
#
#   signet/sources.sh commits [--points]         KSANA_COMMIT=, KSANA_WEB_COMMIT= and KSANA_POINTS_COMMIT= lines
#   signet/sources.sh tar [--points]             the sources as tar archives one after another on standard output
#                                                (GNU tar reads them with --ignore-zeros)
#   signet/sources.sh dir <directory> [--points] the sources extracted into a new directory
#
# The two repositories are found beside this one (../ksana-web, ../ksana-points) unless KSANA_WEB_DIR or
# KSANA_POINTS_DIR names them; the commit each pins must be in this repository's clone or in its own core/ checkout.
# Only committed HEADs are used: uncommitted changes are left out, with a note. Runs with the bash 3.2 of macOS.
set -euo pipefail

usage() {
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0" >&2
    exit 2
}

REPO=$(cd "$(dirname "$0")/.." && pwd)
WEB_REPO=${KSANA_WEB_DIR:-$REPO/../ksana-web}
POINTS_REPO=${KSANA_POINTS_DIR:-$REPO/../ksana-points}
# what points/core/ takes from the pinned commit
POINTS_CORE=(Cargo.toml rust-toolchain.toml crates/ksana-contract-sdk sdk/ts contracts playground/src)

[ $# -ge 1 ] || usage
MODE=$1
shift
OUT=
if [ "$MODE" = dir ]; then
    OUT=${1:-}
    [ -n "$OUT" ] || usage
    shift
fi
POINTS=no
for arg in "$@"; do
    case "$arg" in
        --points) POINTS=yes ;;
        *) usage ;;
    esac
done
case "$MODE" in commits | tar | dir) ;; *) usage ;; esac

# committed <repository> <name> <variable>: its committed HEAD, short; fails with a message when it is not a git
# checkout
committed() {
    local commit
    if ! commit=$(git -C "$1" rev-parse --short=9 HEAD 2>/dev/null); then
        echo "$2 not found at $1: clone it there (github.com/ksana-in-bitcoin/$2) or name its checkout with" \
            "$3" >&2
        exit 1
    fi
    if [ -n "$(git -C "$1" status --porcelain --untracked-files=no)" ]; then
        echo "note: $2 has uncommitted changes; only its commit $commit is used" >&2
    fi
    echo "$commit"
}

# pinned <repository> <name>: the commit of this repository its core submodule pins at HEAD, and where the commit is
# (this clone, or the repository's own core/ checkout), as "<commit> <git directory>"
pinned() {
    local pin
    if ! pin=$(git -C "$1" rev-parse "HEAD:core" 2>/dev/null); then
        echo "$2 at $1 has no core submodule at HEAD" >&2
        exit 1
    fi
    if git -C "$REPO" cat-file -e "$pin^{commit}" 2>/dev/null; then
        echo "$pin $REPO"
    elif git -C "$1/core" cat-file -e "$pin^{commit}" 2>/dev/null; then
        echo "$pin $1/core"
    else
        echo "$2 pins core commit $pin, which is in neither this clone nor $1/core: git fetch, or run" \
            "git submodule update --init in $1" >&2
        exit 1
    fi
}

core=$(committed "$REPO" ksana "a clone of it")
web=$(committed "$WEB_REPO" ksana-web KSANA_WEB_DIR)
web_pin=$(pinned "$WEB_REPO" ksana-web)
points=
points_pin=
if [ "$POINTS" = yes ]; then
    points=$(committed "$POINTS_REPO" ksana-points KSANA_POINTS_DIR)
    points_pin=$(pinned "$POINTS_REPO" ksana-points)
fi

# archive <n>: the n-th archive, from 0 (this repository) to 2, or 4 with --points
archive() {
    case "$1" in
        0) git -C "$REPO" archive --format=tar HEAD ;;
        1) git -C "$WEB_REPO" archive --format=tar --prefix=web/ HEAD ;;
        2) git -C "${web_pin#* }" archive --format=tar --prefix=web/core/ "${web_pin%% *}" sdk/ts ;;
        3) git -C "$POINTS_REPO" archive --format=tar --prefix=points/ HEAD ;;
        4) git -C "${points_pin#* }" archive --format=tar --prefix=points/core/ "${points_pin%% *}" "${POINTS_CORE[@]}" ;;
    esac
}
last=2
[ "$POINTS" = no ] || last=4

case "$MODE" in
    commits)
        echo "KSANA_COMMIT=$core"
        echo "KSANA_WEB_COMMIT=$web"
        echo "KSANA_POINTS_COMMIT=$points"
        echo "sources: ksana $core, ksana-web $web (core ${web_pin:0:9})" \
            "${points:+ksana-points $points (core ${points_pin:0:9})}" >&2
        ;;
    tar)
        i=0
        while [ "$i" -le "$last" ]; do
            archive "$i"
            i=$((i + 1))
        done
        ;;
    dir)
        if [ -e "$OUT" ] && [ -n "$(ls -A "$OUT")" ]; then
            echo "$OUT is not empty" >&2
            exit 1
        fi
        mkdir -p "$OUT"
        # one tar per archive: the tar of macOS stops at the end of the first archive in a stream
        i=0
        while [ "$i" -le "$last" ]; do
            archive "$i" | tar -x -C "$OUT"
            i=$((i + 1))
        done
        ;;
esac
