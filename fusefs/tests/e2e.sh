#!/usr/bin/env bash
# End-to-end test of gha-cache-fusefs through the kernel, in three phases that
# run as separate mounts (or separate jobs):
#
#   e2e.sh write    create files with ordinary tools
#   e2e.sh read     check them from a fresh mount, then change some
#   e2e.sh verify   check the changes, then remove everything
#
# Environment:
#   MNT    the mountpoint
#   RUN    a unique directory name inside the mount for this test run
#   BIN    the gha-cache-fusefs binary (default: from PATH)
#   EXTERNAL_MOUNT=1   the caller mounts and unmounts (e.g. the mount action)
# plus the Actions environment (ACTIONS_RESULTS_URL, ACTIONS_RUNTIME_TOKEN, ...)
# unless EXTERNAL_MOUNT is set.
set -euo pipefail

phase=${1:?usage: e2e.sh write|read|verify}
MNT=${MNT:?MNT must name the mountpoint}
RUN=${RUN:?RUN must name a unique directory}
BIN=${BIN:-gha-cache-fusefs}
D="$MNT/$RUN"
WORK=$(mktemp -d)
trap 'cd /; rm -rf "$WORK"' EXIT

log() { printf '\n== %s\n' "$*"; }
fail() {
  echo "FAIL: $*" >&2
  exit 1
}
# Deterministic pseudo-random data, so later phases can regenerate it.
gen() { openssl enc -aes-256-ctr -pbkdf2 -pass "pass:$1" -nosalt </dev/zero 2>/dev/null | head -c "$2"; }
hash() { sha256sum | cut -d' ' -f1; }
same() { [ "$(hash <"$1")" = "$(gen "$2" "$3" | hash)" ] || fail "$1 does not match gen $2 $3"; }
refuses() { if "$@" 2>/dev/null; then fail "expected this to fail: $*"; fi; }
t() { echo "+ $*"; "$@"; }

mount_fs() {
  [ -n "${EXTERNAL_MOUNT:-}" ] && return
  log "mount"
  t "$BIN" mount "$MNT" --daemon --settle 200ms
  mountpoint -q "$MNT" || fail "$MNT is not a mountpoint"
}

unmount_fs() {
  cd /
  [ -n "${EXTERNAL_MOUNT:-}" ] && return
  log "unmount"
  if ! "$BIN" unmount "$MNT" >"$WORK/summary.json"; then
    cat "$WORK/summary.json"
    fail "unmount reported changes that were not saved"
  fi
  cat "$WORK/summary.json"
  if mountpoint -q "$MNT"; then fail "$MNT is still mounted"; fi
}

# A deterministic tree to copy in.
make_tree() {
  local root=$1 i dir
  mkdir -p "$root"
  for i in $(seq 1 60); do
    dir="$root/dir$((i % 7))/sub$((i % 3))"
    mkdir -p "$dir"
    gen "tree$i" $((i * 997)) >"$dir/file$i"
  done
  echo readme >"$root/README"
  ln -s README "$root/readme-link"
  : >"$root/empty"
  printf '#!/bin/sh\necho tree\n' >"$root/tool.sh"
  chmod 755 "$root/tool.sh"
  touch -d '2021-01-01 00:00:00 UTC' "$root/README"
}

write_phase() {
  mount_fs
  rm -rf "$D"
  mkdir -p "$D"
  cd "$D"

  log "small files"
  echo "hello, world" >hello.txt
  : >empty
  touch empty2
  printf 'a,b,c\n' >"with spaces, commas & ünïcødé 日本.txt"
  [ "$(cat hello.txt)" = "hello, world" ] || fail "reading back hello.txt"

  log "large files"
  gen big5 $((5 << 20)) >big5
  gen big40 $((40 << 20)) >big40
  same big40 big40 $((40 << 20))

  log "directories, symlinks, executables, timestamps"
  mkdir -p d1/d2/d3 emptydir
  echo deep >d1/d2/d3/deep.txt
  ln -s hello.txt link
  [ "$(readlink link)" = hello.txt ] || fail readlink
  [ "$(cat link)" = "hello, world" ] || fail "reading through a symlink"
  printf '#!/bin/sh\necho ran\n' >run.sh
  chmod +x run.sh
  [ "$(./run.sh)" = ran ] || fail "executing from the mount"
  touch -d '2020-02-02 02:02:02 UTC' dated

  log "write-then-rename, overwrite, append, delete, local directory rename"
  echo payload >"tmp.$$"
  mv "tmp.$$" renamed
  echo v1 >over
  sleep 2 # let v1 be uploaded, so v2 replaces a committed version
  echo v2 >over
  echo line1 >log.txt
  echo line2 >>log.txt
  echo doomed >deleted
  rm deleted
  mkdir tmpdir
  echo x >tmpdir/f
  mv tmpdir moveddir

  log "cp -a, tar, dd, mksquashfs"
  make_tree "$WORK/tree"
  t cp -a "$WORK/tree" copied
  diff -r "$WORK/tree" copied || fail "cp -a"
  [ "$(stat -c %Y copied/README)" = "$(stat -c %Y "$WORK/tree/README")" ] || fail "cp -a timestamps"
  tar -C "$WORK" -cf "$WORK/tree.tar" tree
  mkdir tarred
  t tar -C tarred -xf "$WORK/tree.tar"
  diff -r "$WORK/tree" tarred/tree || fail "tar -x"
  t dd if=/dev/zero of=sparse bs=1 count=1 seek=$((10 << 20)) status=none
  [ "$(stat -c %s sparse)" = $(((10 << 20) + 1)) ] || fail "sparse file size"
  t dd if=big5 of=synced bs=1M conv=fsync status=none
  if command -v mksquashfs >/dev/null; then
    t mksquashfs "$WORK/tree" image.sqfs -quiet -noappend -no-progress
  fi

  log "unsupported operations fail cleanly"
  refuses ln hello.txt hardlink
  refuses mkfifo fifo

  log "metadata"
  ls -la >/dev/null
  [ "$(stat -c %a run.sh)" = 755 ] || fail "mode of run.sh"
  [ "$(stat -c %s empty)" = 0 ] || fail "size of empty"
  du -sh . >/dev/null
  df -h "$MNT"
  unmount_fs
}

read_phase() {
  mount_fs
  cd "$D"
  log "contents survive"
  [ "$(cat hello.txt)" = "hello, world" ] || fail hello.txt
  [ -f empty ] && [ ! -s empty ] && [ -f empty2 ] || fail "empty files"
  [ "$(cat "with spaces, commas & ünïcødé 日本.txt")" = "a,b,c" ] || fail "unusual names"
  same big5 big5 $((5 << 20))
  same big40 big40 $((40 << 20))
  [ "$(cat d1/d2/d3/deep.txt)" = deep ] || fail "nested directories"
  [ -d emptydir ] || fail "empty directory"
  [ "$(readlink link)" = hello.txt ] || fail symlink
  [ "$(./run.sh)" = ran ] || fail "executable bit"
  [ "$(stat -c %Y dated)" = "$(date -d '2020-02-02 02:02:02 UTC' +%s)" ] || fail "mtime"
  [ "$(cat renamed)" = payload ] || fail renamed
  [ "$(find . -maxdepth 1 -name 'tmp.*' | wc -l)" = 0 ] || fail "a temporary file was uploaded"
  [ "$(cat over)" = v2 ] || fail "overwrite: got $(cat over)"
  [ "$(cat log.txt)" = "$(printf 'line1\nline2')" ] || fail append
  [ ! -e deleted ] || fail "a deleted file came back"
  [ "$(cat moveddir/f)" = x ] && [ ! -e tmpdir ] || fail "directory rename"
  make_tree "$WORK/tree"
  diff -r "$WORK/tree" copied || fail "cp -a copy"
  diff -r "$WORK/tree" tarred/tree || fail "tar copy"
  [ "$(stat -c %Y tarred/tree/README)" = "$(stat -c %Y "$WORK/tree/README")" ] || fail "tar timestamps"
  [ "$(stat -c %s sparse)" = $(((10 << 20) + 1)) ] || fail "sparse file"
  cmp synced big5 || fail "fsync'd file"

  log "random access into a large file"
  dd if=big40 bs=4096 skip=5000 count=1 status=none | hash >"$WORK/slice"
  gen big40 $((40 << 20)) | dd bs=4096 skip=5000 count=1 iflag=fullblock status=none | hash | cmp - "$WORK/slice" ||
    fail "random read"

  if [ -f image.sqfs ] && command -v squashfuse >/dev/null; then
    log "a squashfs image, mounted lazily out of the cache"
    mkdir -p "$WORK/sq"
    t squashfuse image.sqfs "$WORK/sq"
    diff -r "$WORK/tree" "$WORK/sq" || fail "squashfs contents"
    fusermount3 -u "$WORK/sq" 2>/dev/null || umount "$WORK/sq"
  fi

  log "changing files that only exist remotely"
  t mv big5 big5-moved # not cached here: EXDEV, so mv copies
  same big5-moved big5 $((5 << 20))
  [ ! -e big5 ] || fail "mv left its source behind"
  echo more >>hello.txt # copy-on-write
  chmod -x run.sh
  t rm -r copied
  rmdir emptydir
  unmount_fs
}

verify_phase() {
  mount_fs
  cd "$D"
  log "changes survive"
  [ "$(cat hello.txt)" = "$(printf 'hello, world\nmore')" ] || fail "appending to a remote file"
  [ ! -e big5 ] && [ -f big5-moved ] || fail "mv of a remote file"
  [ "$(stat -c %a run.sh)" = 644 ] || fail "chmod of a remote file"
  [ ! -e copied ] || fail "rm -r"
  [ ! -e emptydir ] || fail rmdir
  [ "$(cat over)" = v2 ] || fail "overwrite"
  log "cleanup"
  cd /
  t rm -rf "$D"
  [ ! -e "$D" ] || fail "rm -rf"
  unmount_fs
}

case $phase in
write) write_phase ;;
read) read_phase ;;
verify) verify_phase ;;
*) fail "unknown phase $phase" ;;
esac
log "$phase: ok"
