#!/usr/bin/env bash
# End to end check for drive folders: two daemons (an anchor "mac" and a member "acer") on this
# machine, each with its own HOME and HILO_HOME under one temp dir, each with a drive labelled
# "notes". Nothing outside that temp dir is touched; the trap stops both daemons and removes it.
#
#   scripts/e2e-drive.sh target/release/hilo
#
# Every command for a machine goes through `on_a` / `on_b`. To run it between two real machines,
# make `on_b` run over ssh (same env vars, a temp dir there) and give `init` an address both reach.
set -euo pipefail

[ $# -eq 1 ] || { echo "usage: $0 PATH/TO/hilo" >&2; exit 2; }
HILO=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
T=$(mktemp -d "${TMPDIR:-/tmp}/hilo-e2e.XXXXXX")
T=$(cd "$T" && pwd -P)
PIDS=()
cleanup() {
    for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill -CONT "$p" 2>/dev/null; [ -n "$p" ] && kill "$p" 2>/dev/null || true; done
    wait 2>/dev/null || true
    rm -rf "$T"
}
trap cleanup EXIT

PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
for m in a b; do mkdir -p "$T/$m/home" "$T/$m/hilo" "$T/$m/code" "$T/$m/notes"; done
A=$T/a/notes
B=$T/b/notes

run_on() { local m=$1; shift; HOME=$T/$m/home HILO_HOME=$T/$m/hilo HILO_KEYCHAIN=0 HILO_NOTIFY=0 HILO_NO_SERVICE=1 "$HILO" "$@"; }
on_a() { run_on a "$@"; }
on_b() { run_on b "$@"; }
in_dir() { (cd "$1" && shift && "$@"); }

PASS=0
ok() { PASS=$((PASS + 1)); echo "ok   $*"; }
fail() {
    echo "FAIL $*" >&2
    for m in a b; do echo "--- daemon log $m" >&2; tail -n 30 "$T/$m/daemon.log" >&2 || true; done
    exit 1
}
# wait_for DESCRIPTION COMMAND...: retry for up to 40 s
wait_for() {
    local what=$1; shift
    for _ in $(seq 200); do "$@" >/dev/null 2>&1 && { ok "$what"; return; }; sleep 0.2; done
    fail "$what"
}
has() { [ -f "$1" ] && [ "$(cat "$1")" = "$2" ]; }
gone() { [ ! -e "$1" ]; }
# a plain command, not a function, so $! is the daemon itself (STOP/kill must reach it)
start() { HOME=$T/$1/home HILO_HOME=$T/$1/hilo HILO_KEYCHAIN=0 HILO_NOTIFY=0 HILO_NO_SERVICE=1 "$HILO" daemon 2>>"$T/$1/daemon.log" & PIDS+=($!); }
up() { run_on "$1" status >/dev/null 2>&1; }
synced() { run_on "$1" status 2>/dev/null | grep -q "drive .*notes, " && ! run_on "$1" status | grep -q "waiting to send"; }

echo "== setup in $T (anchor on 127.0.0.1:$PORT)"
on_a init --listen "127.0.0.1:$PORT" --no-service --name mac --folder "$T/a/code" >/dev/null
start a
wait_for "anchor daemon up" up a
CODE=$(on_a invite | awk '{print $3}')
on_b join "$CODE" --no-service --name acer --folder "$T/b/code" >/dev/null
start b
wait_for "member daemon up" up b
printf '*.log\n' >"$A/.gitignore"
echo one >"$A/a.txt"
echo "pre-existing on acer" >"$B/b.txt"
on_a add --drive "$A" | sed 's/^/     /'
on_b add "$B" --as notes | sed 's/^/     /' # no repos inside: a drive on its own
on_b add --drive "$T/b" --as all 2>/dev/null && fail "a drive around another drive was refused" || ok "a drive around another drive is refused"

echo "== create, edit, delete, both ways"
wait_for "a.txt reaches acer" has "$B/a.txt" one
wait_for "b.txt (already on acer) reaches mac" has "$A/b.txt" "pre-existing on acer"
mkdir -p "$A/deep/er"
echo nested >"$A/deep/er/n.md"
wait_for "a new file in a new subfolder reaches acer" has "$B/deep/er/n.md" nested
echo two >"$B/a.txt"
wait_for "an edit on acer reaches mac" has "$A/a.txt" two
echo three >"$A/a.txt"
wait_for "an edit on mac reaches acer" has "$B/a.txt" three
echo "K=1" >"$A/.env"
echo noise >"$A/x.log"
mkdir -p "$A/node_modules/m" "$A/inner/.git"
echo m >"$A/node_modules/m/i.js"
echo r >"$A/inner/r.txt"
echo marker >"$A/marker.txt"
wait_for "marker reaches acer" has "$B/marker.txt" marker
for f in .env x.log node_modules/m/i.js inner/r.txt; do gone "$B/$f" || fail "$f should not sync"; done
ok ".env, ignored files, node_modules and a nested repo stay on mac"
rm "$B/b.txt"
wait_for "a delete on acer reaches mac" gone "$A/b.txt"
rm "$A/deep/er/n.md"
wait_for "a delete on mac reaches acer" gone "$B/deep/er/n.md"

echo "== trash and restore on acer (deleted by mac)"
in_dir "$B" on_b trash | sed 's/^/     /'
in_dir "$B" on_b trash | grep -q "deep/er/n.md  deleted by mac" || fail "trash lists deep/er/n.md"
ok "hilo trash lists the file mac deleted"
on_b restore "$B/deep/er/n.md" | sed 's/^/     /'
has "$B/deep/er/n.md" nested || fail "restore brings it back on acer"
wait_for "the restored file syncs back to mac" has "$A/deep/er/n.md" nested

echo "== a concurrent edit to the same file"
echo base >"$A/c.txt"
wait_for "c.txt reaches acer" has "$B/c.txt" base
wait_for "acer settled" synced b
PB=${PIDS[1]}
kill -STOP "$PB" # acer's daemon can't send its edit before mac's lands
echo "acer's version" >"$B/c.txt"
echo "mac's version" >"$A/c.txt"
mac_sent() { [ "$(on_a log "$A/c.txt" | grep -c ' local ')" -ge 2 ] && synced a; }
wait_for "mac's edit is accepted" mac_sent
kill -CONT "$PB"
wait_for "mac's version wins on acer" has "$B/c.txt" "mac's version"
sleep 1
has "$A/c.txt" "mac's version" || fail "mac keeps its version"
ok "one winner on both disks"
on_b log "$B/c.txt" | sed 's/^/     /'
on_b log "$B/c.txt" | grep -q " conflict " || fail "acer's log shows the conflict"
on_b status | grep -q "1 in drives" || fail "status counts the drive conflict"
ok "acer's log and status show the conflict"
on_b restore "$B/c.txt" --conflict | sed 's/^/     /'
has "$B/c.txt" "acer's version" || fail "restore --conflict brings acer's version back"
wait_for "the restored version syncs to mac" has "$A/c.txt" "acer's version"

echo "== a version from outside the history is kept before hilo overwrites it"
# acer's history is lost (pruned, or from before hilo kept one): the copy on disk has no record
kill "$PB"; wait "$PB" 2>/dev/null || true; PIDS[1]=
rm -rf "$T/b/hilo/store"
start b
wait_for "acer back up" up b
wait_for "acer settled again" synced b
echo "mac again" >"$A/a.txt"
wait_for "the edit reaches acer" has "$B/a.txt" "mac again"
on_b log "$B/a.txt" | sed 's/^/     /'
on_b log "$B/a.txt" | grep -q " replaced " || fail "acer kept what was on disk as replaced"
ok "acer's log has the replaced version"
on_b log "$B/a.txt" | grep -q " remote " || fail "remote entry"
on_a log "$A/a.txt" | grep -q " local " || fail "local entry on mac"
on_a log "$A/a.txt" | sed 's/^/     /'
ok "logs show local and remote entries"

echo "== a sandbox whose token only sees notes/clientes"
mkdir -p "$T/c/home" "$T/c/hilo" "$T/c/code" "$T/c/notes" "$A/clientes"
C=$T/c/notes
on_c() { run_on c "$@"; }
echo "for the client" >"$A/clientes/plan.md"
TOKEN=$(on_a token create --addr "127.0.0.1:$PORT" --path "$A/clientes" 2>/dev/null | awk '/^token/ {print $2}')
on_c join "$TOKEN" --no-service --name sbx --folder "$T/c/code" >/dev/null
start c
wait_for "sandbox daemon up" up c
on_c add "$C" --as notes | sed 's/^/     /'
wait_for "its folder reaches the sandbox" has "$C/clientes/plan.md" "for the client"
for f in a.txt marker.txt c.txt deep/er/n.md; do gone "$C/$f" || fail "$f is outside the token's folder and reached the sandbox"; done
ok "nothing outside notes/clientes reaches the sandbox"
echo out >"$C/fuera.md"
echo in >"$C/clientes/nuevo.md"
wait_for "a file it writes inside reaches mac" has "$A/clientes/nuevo.md" in
kept_here() { run_on c status | grep -q "1 files outside the scope, kept here only" && ! run_on c status | grep -q "waiting to send"; }
wait_for "a file it writes outside stays there, and nothing waits forever" kept_here
gone "$A/fuera.md" || fail "fuera.md reached mac"
on_c status | grep -E "^(scope|  )" | sed 's/^/     /'
on_c status | grep -q "^scope     drive:notes/clientes$" || fail "status shows the scope"
grep -q "fuera.md is outside what this sandbox's token may sync (drive:notes/clientes)" "$T/c/daemon.log" || fail "the sandbox logs why"
ok "the sandbox's status and log say why fuera.md stays there"

echo "== acer's drive folder goes away (unmounted) and comes back"
paused() { run_on "$1" status | grep -q "^drive .*(paused: "; }
syncing() { run_on "$1" status | grep -q "^drive .*notes, "; }
mv "$B" "$B.away"
wait_for "acer pauses the drive" paused b
echo "mac while acer was away" >"$A/a.txt"
echo "created while acer was away" >"$A/new1.txt"
rm "$A/marker.txt"
wait_for "mac settled" synced a
mv "$B.away" "$B"
wait_for "acer syncs the drive again" syncing b
wait_for "acer gets the edit it missed" has "$B/a.txt" "mac while acer was away"
wait_for "acer gets the file created meanwhile" has "$B/new1.txt" "created while acer was away"
wait_for "acer gets the deletion it missed" gone "$B/marker.txt"
has "$A/a.txt" "mac while acer was away" || fail "mac's edit was reverted"
ok "mac's edit is not reverted"

echo "== the same, with acer restarting while the folder is back"
PB=${PIDS[2]}
mv "$B" "$B.away"
wait_for "acer pauses the drive" paused b
echo "mac again while away" >"$A/a.txt"
echo "second new file" >"$A/new2.txt"
wait_for "mac settled" synced a
mv "$B.away" "$B"
kill "$PB"; wait "$PB" 2>/dev/null || true; PIDS[2]=
start b
wait_for "acer back up" up b
wait_for "acer gets the edit it missed" has "$B/a.txt" "mac again while away"
wait_for "acer gets the file created meanwhile" has "$B/new2.txt" "second new file"
sleep 2
has "$A/a.txt" "mac again while away" || fail "acer's stale copy reverted mac's edit"
ok "acer's stale copy does not revert mac's edit"

echo "== mac's drive folder is swapped for an empty one (an unmounted mount point)"
mv "$A" "$A.real"
mkdir "$A"
wait_for "mac pauses the drive instead of deleting everything" paused a
on_a status | grep "^drive" | sed 's/^/     /'
sleep 5
for f in a.txt c.txt new1.txt new2.txt clientes/plan.md; do [ -f "$B/$f" ] || fail "$f was deleted on acer"; done
ok "acer keeps every file"
rmdir "$A"
mv "$A.real" "$A"
wait_for "the same folder back: mac syncs it again" syncing a
echo "after the swap" >"$A/new1.txt"
wait_for "and its edits reach acer" has "$B/new1.txt" "after the swap"

echo "== hilo's own folder never syncs"
out=$(on_a add --drive "$T/a" --as all 2>&1) && fail "a drive holding HILO_HOME was accepted"
echo "     $out"
echo "$out" | grep -q "hilo's own folder" || fail "the refusal says why"
on_a add --drive "$T/a/hilo/tmp" --as t >/dev/null 2>&1 && fail "a drive inside HILO_HOME was accepted"
ok "a drive around or inside HILO_HOME is refused"
mkdir -p "$T/d/home"
out=$(cd "$T/d/home" && HOME=$T/d/home HILO_HOME=$T/d/home/.hilo HILO_KEYCHAIN=0 HILO_NOTIFY=0 HILO_NO_SERVICE=1 HILO_TOKEN=$TOKEN "$HILO" up --drive notes --no-wait 2>&1) && fail "hilo up --drive in \$HOME was accepted"
echo "     $out"
echo "$out" | grep -q "hilo's own folder" || fail "hilo up --drive in \$HOME says why"
ok "hilo up --drive from \$HOME is refused with a clear message"

echo "== a repo inside a drive syncs as a repo; an empty folder stays a folder of repos"
mkdir -p "$A/proj"
git -C "$A/proj" init -q -b main
echo p >"$A/proj/p.txt"
git -C "$A/proj" add p.txt
git -C "$A/proj" -c user.email=t@t -c user.name=t commit -qm p
on_a add "$A/proj" | sed 's/^/     /'
on_a status | grep -q "^  $A/proj \[main\]" || fail "the repo inside the drive is synced as a repo"
ok "a repo inside a drive can be added"
mkdir -p "$T/a/empty"
out=$(on_a add "$T/a/empty")
echo "     $out"
echo "$out" | grep -q "hilo add --drive" || fail "the empty folder's message mentions --drive"
on_a status | grep -q "^folder .*/a/empty " || fail "an empty folder is linked as a folder of repos"
ok "an empty folder is linked as a folder of repos, with a hint about --drive"

echo "== status"
on_a status | sed 's/^/     /'
on_b status | grep -E "^(drive|conflicts| )" | sed 's/^/     /'
echo "PASS: $PASS checks"
