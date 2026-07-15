#!/bin/sh
# End-to-end tests for the journal engine on the linux FUSE mount: the editor
# dances validated against real traces (vim, exiftool, ReplaceFile), plus the
# lease/conflict model, against a live filestash server.
#
# Assumes fdrive is mounted (default ~/Downloads/mnt); server url and token
# come from fdrive.toml.
set -u

MNT="${MNT:-$HOME/Downloads/mnt}"
DATA="$HOME/.local/share/filestash"
LOG="$DATA/fdrive.log"
E2E="e2e-journal"
DIR="$MNT/$E2E"

URL=$(sed -n 's/^url = "\(.*\)"$/\1/p' "$DATA/fdrive.toml")
TOKEN=$(sed -n 's/^token = "\(.*\)"$/\1/p' "$DATA/fdrive.toml")
[ -n "$URL" ] && [ -n "$TOKEN" ] || { echo "no session in $DATA/fdrive.toml"; exit 1; }
mountpoint -q "$MNT" || { echo "$MNT is not mounted"; exit 1; }

srv() {
    method=$1; call=$2; body=${3:-}
    if [ -n "$body" ]; then
        curl -s -X "$method" -H "X-Requested-With: SDKHttpRequest" \
            -H "Authorization: Bearer $TOKEN" --data-binary "@$body" "$URL/api/files/$call"
    else
        curl -s -X "$method" -H "X-Requested-With: SDKHttpRequest" \
            -H "Authorization: Bearer $TOKEN" "$URL/api/files/$call"
    fi
}
srv_ls() { srv GET "ls?path=/$E2E/$1/"; }
srv_cat() { srv GET "cat?path=/$E2E/$1"; }
srv_save() { # path content
    tmp=$(mktemp); printf '%s' "$2" > "$tmp"
    srv POST "cat?path=/$E2E/$1" "$tmp" > /dev/null; rm -f "$tmp"
}

PASS=0; FAIL=0
ok() { PASS=$((PASS+1)); echo "ok   - $1"; }
bad() { FAIL=$((FAIL+1)); echo "FAIL - $1"; }
is() { # desc expected actual
    if [ "$2" = "$3" ]; then ok "$1"; else bad "$1 (expected [$2] got [$3])"; fi
}
has() { # desc haystack needle
    case "$2" in *"$3"*) ok "$1";; *) bad "$1 (missing [$3])";; esac
}
lacks() {
    case "$2" in *"$3"*) bad "$1 (has [$3])";; *) ok "$1";; esac
}

MARK=0
mark() { MARK=$(wc -l < "$LOG"); }
since() { tail -n "+$((MARK + 1))" "$LOG"; }
settle() { sleep 2; }

vim_edit() { # file keys
    timeout 30 script -qec "vim -c 'normal G$2' -c 'wq' '$1'" /dev/null > /dev/null 2>&1
}

echo "# journal e2e against $URL via $MNT"
rm -rf "$DIR" 2>/dev/null
mkdir -p "$DIR"
settle

# --- 1. vim dance nets to a single save -----------------------------------
mkdir -p "$DIR/t1"
printf 'line one\n' > "$DIR/t1/vim.txt"
settle
mark
vim_edit "$DIR/t1/vim.txt" "oline two"
settle
is  "vim: server got the edit" "line one
line two" "$(srv_cat t1/vim.txt)"
has "vim: journal netted a save" "$(since)" "-> [save $E2E/t1/vim.txt"
lacks "vim: no server-side backup dance" "$(since)" "move $E2E/t1/vim.txt~"
lacks "vim: backup file never reached the server" "$(srv_ls t1)" "vim.txt~"
lacks "vim: swap file never reached the server" "$(srv_ls t1)" ".swp"

# --- 2. exiftool keeps its backup: move then save --------------------------
if command -v exiftool > /dev/null && [ -f "$MNT/1784013243970.jpg" ]; then
    mkdir -p "$DIR/t2"
    cp "$MNT/1784013243970.jpg" "$DIR/t2/photo.jpg"
    settle
    mark
    exiftool -q -Artist='journal e2e' "$DIR/t2/photo.jpg"
    settle
    has "exiftool: journal moved the original aside" "$(since)" "move $E2E/t2/photo.jpg->$E2E/t2/photo.jpg_original"
    has "exiftool: journal saved the new bytes" "$(since)" "save $E2E/t2/photo.jpg"
    has "exiftool: backup exists on the server" "$(srv_ls t2)" "photo.jpg_original"
else
    echo "skip - exiftool not available"
fi

# --- 3. ReplaceFile dance nets to a single save ----------------------------
mkdir -p "$DIR/t3"
printf 'v1' > "$DIR/t3/doc.txt"
settle
mark
printf 'v2' > "$DIR/t3/doc.txt.new_tmp"
mv "$DIR/t3/doc.txt" "$DIR/t3/doc.txt~RF9999.TMP"
mv "$DIR/t3/doc.txt.new_tmp" "$DIR/t3/doc.txt"
rm "$DIR/t3/doc.txt~RF9999.TMP"
settle
is  "replacefile: server got the new bytes" "v2" "$(srv_cat t3/doc.txt)"
has "replacefile: journal netted a save" "$(since)" "-> [save $E2E/t3/doc.txt"
lacks "replacefile: temp never reached the server" "$(srv_ls t3)" "RF9999"

# --- 4. a temp file that dies is nothing ----------------------------------
mkdir -p "$DIR/t4"
settle
mark
printf 'scratch' > "$DIR/t4/scratch.tmp"
rm "$DIR/t4/scratch.tmp"
settle
has "temp: journal netted nothing" "$(since)" "-> []"
lacks "temp: server never saw it" "$(srv_ls t4)" "scratch"

# --- 5. deleting a listed-but-never-opened file works ----------------------
mkdir -p "$DIR/t5"
settle
srv_save "t5/listed.txt" "server made me"
ls "$DIR/t5/" > /dev/null   # a listing is an observation
rm "$DIR/t5/listed.txt"
settle
lacks "listed delete: gone from the server" "$(srv_ls t5)" "listed.txt"

# --- 6. rename chains fold -------------------------------------------------
mkdir -p "$DIR/t6"
printf 'chained' > "$DIR/t6/f1"
settle
mark
mv "$DIR/t6/f1" "$DIR/t6/f2"
mv "$DIR/t6/f2" "$DIR/t6/f3"
settle
has "chain: one folded move" "$(since)" "move $E2E/t6/f1->$E2E/t6/f3"
is  "chain: content followed" "chained" "$(srv_cat t6/f3)"
lacks "chain: middle name never existed upstream" "$(srv_ls t6)" "\"f2\""

# --- 7. a stale lease diverts to a conflicted copy -------------------------
mkdir -p "$DIR/t7"
printf 'base' > "$DIR/t7/c.txt"
settle
srv_save "t7/c.txt" "theirs"   # the server moves on without us
printf 'ours' > "$DIR/t7/c.txt"
settle
is  "conflict: their version holds the name" "theirs" "$(srv_cat t7/c.txt)"
has "conflict: ours landed as a copy" "$(srv_ls t7)" "conflicted copy"

# --- 8. a slow save (truncate, think, write) is one upload ------------------
mkdir -p "$DIR/t8"
printf 'original content' > "$DIR/t8/slow.txt"
settle
mark
exec 3> "$DIR/t8/slow.txt"   # truncates and stays open, like an exporting app
sleep 0.6
printf 'rendered output' >&3
exec 3>&-
settle
is  "slow save: server got the final bytes" "rendered output" "$(srv_cat t8/slow.txt)"
saves=$(since | grep -c "uploaded e2e-journal/t8/slow.txt")
is  "slow save: exactly one upload" "1" "$saves"

# --- 9. pinning hydrates without opening ------------------------------------
mkdir -p "$DIR/t9"
settle
srv_save "t9/pinned.txt" "keep me local"
ls "$DIR/t9/" > /dev/null
setfattr -n user.fdrive.pin -v always "$DIR/t9"
sleep 3
CACHE="$HOME/.local/share/filestash/cache/$E2E/t9/pinned.txt"
[ -f "$CACHE" ] && ok "pin: content arrived in the cache unopened" || bad "pin: nothing hydrated at $CACHE"
is  "pin: getfattr answers" "always" "$(getfattr --only-values -n user.fdrive.pin "$DIR/t9" 2>/dev/null)"
setfattr -n user.fdrive.pin -v auto "$DIR/t9"
is  "pin: unpin clears the answer" "" "$(getfattr --only-values -n user.fdrive.pin "$DIR/t9" 2>/dev/null)"

# --- cleanup ---------------------------------------------------------------
rm -rf "$DIR"
settle
echo "# done: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
