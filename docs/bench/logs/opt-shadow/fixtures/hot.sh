#!/usr/bin/env bash
# WSL owns build-queue, the source checkout and gpu-quiet. Windows owns DX12.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
mode=${1:-}
shift || true

# Same fingerprint as the original runner, including raw untracked file bytes.
if [[ $mode == _stamp ]]; then
    cd "${1:-$root}"
    paths=(-- . ':(exclude)docs/bench')
    git rev-parse HEAD
    temp=$(mktemp -d)
    trap 'rm -rf "$temp"' EXIT
    git diff --binary HEAD "${paths[@]}" > "$temp/diff"
    git ls-files -z --others --exclude-standard "${paths[@]}" | LC_ALL=C sort -z > "$temp/files"
    if [[ -s $temp/diff || -s $temp/files ]]; then
        while IFS= read -r -d '' name; do
            printf '%s\0' "$name" >> "$temp/diff"
            sha256sum < "$name" | cut -c1-64 | xxd -r -p >> "$temp/diff"
        done < "$temp/files"
        sha256sum < "$temp/diff" | cut -c1-64
    fi
    exit 0
fi

winpath() {
    local path=$1
    if [[ $path == [a-zA-Z]:* || $path == '\\'* ]]; then
        printf '%s\n' "$path"
    else
        wslpath -w "$(realpath -m -- "$path")"
    fi
}

quiet_dir="$HOME/.local/state/gpu-quiet"
pending="$quiet_dir/pending"
owner=
reservation_busy() {
    [[ -e $quiet_dir/state.json ]] || {
        [[ -e $pending ]] && { [[ -z $owner ]] || [[ $(cat "$pending" 2>/dev/null) != "$owner" ]]; }
    }
}
wait_for_window() {
    while reservation_busy; do
        # An outer build-queue cannot wait here while holding a slot that the
        # pending GPU reservation needs. Let that caller retry outside its slot.
        [[ -z ${BUILD_QUEUE_HELD:-} ]] || return 76
        sleep 1
    done
}
if [[ $mode == _queued ]]; then
    # Check after acquiring the slot too: build-queue can time out and run a
    # job that was queued before the pending file appeared.
    owner=$1
    shift
    if reservation_busy; then exit 76; fi
    exec build-queue "$@"
fi
release_pending() {
    if [[ -f $pending && $(cat "$pending" 2>/dev/null) == "$owner" ]]; then rm -- "$pending"; fi
}
claim_pending() {
    mkdir -p "$quiet_dir"
    local token="${VICTORY_BENCH_SESSION:-$(basename "$root")} pid=$$ $(date -u +%FT%TZ)"
    while :; do
        wait_for_window || return $?
        if (set -C; umask 077; printf '%s\n' "$token" > "$pending") 2>/dev/null; then
            owner=$token
            trap release_pending EXIT
            trap 'exit 130' INT
            trap 'exit 143' TERM HUP
            return
        fi
        [[ -e $pending ]] || { echo "cannot create $pending" >&2; return 1; }
    done
}
ps=/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe
driver=("$ps" -NoProfile -ExecutionPolicy Bypass -File "$(winpath "$root/target/shadow-triplets/hot.ps1")")
envfile=$(winpath "${VICTORY_BENCH_ENV:-$root/../toolchain/env.ps1}")
redact() {
    sed -u -E "s@[A-Z]:[/\\\\]Users[/\\\\][^/\\\\[:space:]\"']+@%USERPROFILE%@Ig; s/GPU-[a-f0-9-]{32,}/GPU-REDACTED/Ig"
}
invoke() {
    local rc
    while :; do
        wait_for_window || return $?
        if build-queue bash "$root/tools/bench/hot.sh" _queued "$owner" "${driver[@]}" "$@" 2> >(redact >&2) | redact; then
            return 0
        else
            rc=$?
        fi
        if [[ $rc != 76 || -n ${BUILD_QUEUE_HELD:-} ]]; then return "$rc"; fi
    done
}
if [[ $mode == compare && $# == 2 ]]; then
    invoke compare "$(winpath "$1")" "$(winpath "$2")" -EnvFile "$envfile"
    exit
fi
case "$mode" in build|smoke|run|ab) ;; *)
    echo "usage: hot.sh {build|smoke|run} [--runs N] [--out DIR] | ab --a DIR --b DIR [--runs N] [--out DIR] | compare BEFORE AFTER" >&2
    exit 2 ;;
esac
runs=30
a=
b=
third=
suite=
[[ $mode != smoke ]] || runs=3
out="$root/docs/bench/logs/hot-$(date +%Y%m%d-%H%M%S)"
while (($#)); do
    case "$1" in
        --suite) suite=$(winpath "${2:?missing suite manifest}"); shift 2 ;;
        --suite=*) suite=$(winpath "${1#*=}"); shift ;;
        --a) a=$(winpath "${2:?missing A directory}"); shift 2 ;;
        --a=*) a=$(winpath "${1#*=}"); shift ;;
        --c) third=$(winpath "${2:?missing B2 checkout}"); shift 2 ;;
        --b=*) b=$(winpath "${1#*=}"); shift ;;
        --b) b=$(winpath "${2:?missing B directory}"); shift 2 ;;
        --runs) runs=${2:?missing count}; shift 2 ;;
        --runs=*) runs=${1#*=}; shift ;;
        --out) out=${2:?missing directory}; shift 2 ;;
        --out=*) out=${1#*=}; shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
# Non-login WSL services do not always export WSL_DISTRO_NAME.
distro=${WSL_DISTRO_NAME:-$(wslpath -w / | cut -d '\' -f4)}
[[ $mode == ab || ( -z $a && -z $b ) ]] || { echo "--a/--b require ab" >&2; exit 2; }
common=(-EnvFile "$envfile" -Distro "${distro:?cannot identify WSL distribution}")
if [[ $mode == build ]]; then invoke build "${common[@]}"; exit; fi
[[ $runs =~ ^[0-9]+$ ]] && ((10#$runs >= 1)) || { echo "invalid run count" >&2; exit 2; }
out=$(wslpath -u "$(winpath "$out")")
args=(-Runs "$runs" -OutDir "$(winpath "$out")" "${common[@]}")
[[ -z $suite ]] || args+=(-SuiteFile "$suite")
if [[ $mode == smoke ]]; then
    state="$HOME/.local/state/gpu-quiet/state.json"
    invoke smoke "${args[@]}" -QuietState "$(winpath "$state")"
    exit
fi
sample_mode=sample
if [[ $mode == ab ]]; then
    [[ -n $a && -n $b ]] && ((10#$runs >= 2)) || { echo "ab requires --a, --b and at least two pairs" >&2; exit 2; }
    sample_mode=ab
    [[ -n $third ]] || { echo "triplet sampling requires --c DIR" >&2; exit 2; }
    args+=(-Before "$a" -After "$b" -Third "$third")
else
    ((10#$runs >= 30)) || { echo "formal sampling requires at least 30 runs" >&2; exit 2; }
fi
claim_pending
mkdir -p "$out"
# Some build-queue versions return 1 when there is no history yet even
# though they listed all slots. Validate the listing itself.
if ! status=$(build-queue --status); then
    [[ $status == *'槽位 '* ]] || { echo "cannot read build slots" >&2; exit 1; }
fi
slots=$(sed -n 's/^槽位 \([0-9][0-9]*\)：.*/\1/p' <<< "$status")
[[ -n $slots ]] || { echo "cannot identify build slots" >&2; exit 1; }
# Keep only current holders, excluding their potentially private arguments.
(set -C; {
    date -u '+%FT%TZ'
    sed -n '/^槽位 /{s/，命令 .*//;p;}' <<< "$status" | redact
} > "$out/queue-before.txt")
invoke build "${common[@]}"
state=${BUILD_QUEUE_STATE:-$HOME/.local/state/build-queue}

# Skip a slot already held by a caller. Release partial reservations before
# waiting for a busy slot, so independent reservations cannot deadlock.
ancestors=" "
pid=$$
while ((pid > 1)); do
    ancestors+="$pid "
    read -r _ pid _ <<< "$(sed -E 's/^.*\) //' "/proc/$pid/stat")"
done
while :; do
    held=()
    busy=
    for slot in $slots; do
        slot_owner=$(sed -n 's/^pid \([0-9][0-9]*\).*/\1/p' "$state/slot$slot.info" 2>/dev/null || :)
        [[ -z $slot_owner || $ancestors != *" $slot_owner "* ]] || continue
        busy="$state/slot$slot.lock"
        exec {fd}>>"$busy"
        held+=("$fd")
        flock -n "$fd" || break
        busy=
    done
    [[ -n $busy ]] || break
    for fd in "${held[@]}"; do exec {fd}>&-; done
    flock "$busy" true
done
set -C
exec 3>"$out/window.txt"
printf 'Local build-queue slots reserved: %s\n' "$(wc -w <<< "$slots")" >&3
# All slots are held for this command; build-queue recognizes nested work.
export BUILD_QUEUE_HELD=1
(
    # A desktop service restored by gpu-quiet must not inherit the slot locks.
    for fd in "${held[@]}"; do exec {fd}>&-; done
    exec 3>&-
    exec gpu-quiet run -- build-queue "${driver[@]}" "$sample_mode" "${args[@]}"
) 2>&1 |
    redact |
    tee -a /dev/fd/3
