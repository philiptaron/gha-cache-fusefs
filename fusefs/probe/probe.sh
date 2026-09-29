#!/usr/bin/env bash
# Probes the GitHub Actions cache v2 service: the measurements behind
# DESIGN.md §1. Run by the fusefs-probe workflow (manually); it deletes what it
# creates. Never prints the runtime token or SAS signatures.
set -uo pipefail

: "${ACTIONS_RESULTS_URL:?}" "${ACTIONS_RUNTIME_TOKEN:?}" "${GITHUB_TOKEN:?}"
ORIGIN=$(echo "$ACTIONS_RESULTS_URL" | sed -E 's#^(https?://[^/]+).*#\1#')
SVC="$ORIGIN/twirp/github.actions.results.api.v1.CacheService"
RUN="${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}"
P="probe/$RUN"
VERSION=$(printf 'gha-cache-fusefs-probe' | sha256sum | cut -d' ' -f1)
TMP=$(mktemp -d)
API="https://api.github.com/repos/$GITHUB_REPOSITORY"

redact() { sed -E 's/(sig=)[^&"]*/\1REDACTED/g'; }
section() { echo; echo "===== $* ====="; }

twirp() {
  local method=$1 body=$2 out
  out=$(curl -sS -o "$TMP/resp" -D "$TMP/hdrs" -w '%{http_code} %{time_total}' -X POST "$SVC/$method" \
    -H "Authorization: Bearer $ACTIONS_RUNTIME_TOKEN" -H 'Content-Type: application/json' \
    --data "$body")
  TW_STATUS=${out%% *}
  TW_TIME=${out##* }
  TW_BODY=$(cat "$TMP/resp")
  echo "[$method] http=$TW_STATUS t=${TW_TIME}s req=$(echo "$body" | redact | head -c 300) resp=$(echo "$TW_BODY" | redact | head -c 600)"
}

req() { jq -cn --arg key "$1" --arg version "${2:-$VERSION}" '{key:$key, version:$version}'; }
fin() { jq -cn --arg key "$1" --arg size "$2" --arg version "${3:-$VERSION}" '{key:$key, version:$version, size_bytes:$size}'; }
get() { jq -cn --arg key "$1" --arg version "$VERSION" --argjson rk "${2:-[]}" '{key:$key, version:$version, restore_keys:$rk}'; }

urlinfo() {
  local u=$1
  echo "  host+path: $(echo "$u" | sed -E 's/\?.*//' | sed -E 's#(https://[^/]+/[^/]+/).*#\1...#')"
  echo "  params: $(echo "$u" | sed -E 's/^[^?]*\?//' | tr '&' '\n' | grep -v '^sig=' | tr '\n' ' ')"
}

# put_blob KEY FILE [VERSION] -> creates, uploads (single shot), finalizes
put_entry() {
  local key=$1 file=$2 version=${3:-$VERSION} size up
  size=$(stat -c %s "$file")
  twirp CreateCacheEntry "$(req "$key" "$version")"
  up=$(echo "$TW_BODY" | jq -r '.signed_upload_url // empty')
  if [ -z "$up" ]; then echo "  !! no upload url"; return 1; fi
  code=$(curl -sS -o "$TMP/put" -w '%{http_code}' -X PUT -H 'x-ms-blob-type: BlockBlob' \
    -H 'Content-Type: application/octet-stream' --data-binary "@$file" "$up")
  echo "  PutBlob http=$code $(head -c 300 "$TMP/put")"
  twirp FinalizeCacheEntryUpload "$(fin "$key" "$size" "$version")"
}

section environment
env | grep -E '^(ACTIONS|GITHUB|RUNNER)_' | cut -d= -f1 | sort | tr '\n' ' '; echo
echo "ACTIONS_CACHE_SERVICE_V2=${ACTIONS_CACHE_SERVICE_V2:-<unset>}"
echo "ACTIONS_CACHE_MODE=${ACTIONS_CACHE_MODE:-<unset>}"
echo "results host: $(echo "$ACTIONS_RESULTS_URL" | sed -E 's#^https?://([^/]+)(.*)#\1 path=\2#')"
echo "GITHUB_REF=$GITHUB_REF GITHUB_BASE_REF=${GITHUB_BASE_REF:-} event=$GITHUB_EVENT_NAME"
jq -r '.repository.default_branch' "$GITHUB_EVENT_PATH"

section fuse-and-host
uname -a; head -2 /etc/os-release
ls -l /dev/fuse /dev/kvm 2>&1
for b in fusermount fusermount3 squashfuse squashfuse_ll mksquashfs mountpoint; do printf '%s: ' $b; command -v $b || echo missing; done
ls -l "$(command -v fusermount3 2>/dev/null || echo /nonexistent)" 2>&1
dpkg -l | grep -iE ' (fuse|libfuse|squash)' | awk '{print $2, $3}'
cat /etc/fuse.conf 2>&1 | grep -v '^#'
sudo -n true && echo "passwordless sudo: yes"
sudo -n -l 2>&1 | tail -3
df -h / /mnt "$RUNNER_TEMP" 2>&1; nproc; free -m | head -2
id

section methods
for m in CreateCacheEntry FinalizeCacheEntryUpload GetCacheEntryDownloadURL DeleteCacheEntry ListCacheEntries LookupCacheEntry GetCacheEntry UpdateCacheEntry ListCacheEntriesByKey; do
  twirp "$m" '{}'
done
echo "response headers of last call:"; grep -viE '^(set-cookie|strict-transport)' "$TMP/hdrs" | tr -d '\r' | sed 's/^/  /'

section roundtrip
K1="$P/hello"
printf 'hello, world\n' > "$TMP/hello"
twirp CreateCacheEntry "$(req "$K1")"
UP=$(echo "$TW_BODY" | jq -r '.signed_upload_url // empty')
urlinfo "$UP"
echo "  now: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
curl -sS -D - -o /dev/null -X PUT -H 'x-ms-blob-type: BlockBlob' --data-binary "@$TMP/hello" "$UP" | tr -d '\r' | redact | sed 's/^/  /'
twirp FinalizeCacheEntryUpload "$(fin "$K1" 13)"
twirp GetCacheEntryDownloadURL "$(get "$K1")"
DL=$(echo "$TW_BODY" | jq -r '.signed_download_url // empty')
urlinfo "$DL"
echo "--- HEAD"
curl -sS -I "$DL" | tr -d '\r' | redact | sed 's/^/  /'
echo "--- Range bytes=7-11"
curl -sS -D "$TMP/rh" -H 'Range: bytes=7-11' "$DL" | od -c | head -3
tr -d '\r' < "$TMP/rh" | grep -iE '^(HTTP|content-range|content-length|accept-ranges)' | sed 's/^/  /'
echo "--- Range bytes=0-0"
curl -sS -D "$TMP/rh" -o /dev/null -H 'Range: bytes=0-0' "$DL"
tr -d '\r' < "$TMP/rh" | grep -iE '^(HTTP|content-range|content-length)' | sed 's/^/  /'
echo "--- Range past end bytes=100-200"
curl -sS -D "$TMP/rh" -o /dev/null -H 'Range: bytes=100-200' "$DL"
tr -d '\r' < "$TMP/rh" | grep -iE '^(HTTP|content-range|content-length)' | sed 's/^/  /'

section duplicate-create
twirp CreateCacheEntry "$(req "$K1")"

section lookup-semantics
twirp GetCacheEntryDownloadURL "$(get "$P/hel")"
twirp GetCacheEntryDownloadURL "$(get "$P/")"
twirp GetCacheEntryDownloadURL "$(get "$P")"
twirp GetCacheEntryDownloadURL "$(get "zzz-nonexistent-$RUN" "[\"$P/\"]")"
twirp GetCacheEntryDownloadURL "$(get "$P/HELLO")"
twirp GetCacheEntryDownloadURL "$(get "$P/hello" '["x"]')"
printf 'second\n' > "$TMP/second"
put_entry "$P/hello2" "$TMP/second"
echo "-- after creating hello2: prefix $P/hel ->"
twirp GetCacheEntryDownloadURL "$(get "$P/hel")"
echo "-- exact hello still exact?"
twirp GetCacheEntryDownloadURL "$(get "$P/hello")"

section zero-byte
: > "$TMP/empty"
put_entry "$P/empty" "$TMP/empty"
twirp GetCacheEntryDownloadURL "$(get "$P/empty")"
DL0=$(echo "$TW_BODY" | jq -r '.signed_download_url // empty')
[ -n "$DL0" ] && curl -sS -I "$DL0" | tr -d '\r' | grep -iE '^(HTTP|content-length)' | sed 's/^/  /'

section block-upload
K5="$P/blocks"
head -c 5000000 /dev/urandom > "$TMP/b1"; head -c 3000000 /dev/urandom > "$TMP/b2"
cat "$TMP/b1" "$TMP/b2" > "$TMP/b12"
twirp CreateCacheEntry "$(req "$K5")"
UP5=$(echo "$TW_BODY" | jq -r '.signed_upload_url // empty')
id1=$(printf '%08d' 0 | base64); id2=$(printf '%08d' 1 | base64)
enc() { jq -rn --arg s "$1" '$s|@uri'; }
curl -sS -o "$TMP/o" -w '  PutBlock1 http=%{http_code}\n' -X PUT --data-binary "@$TMP/b1" "$UP5&comp=block&blockid=$(enc "$id1")"; head -c 300 "$TMP/o"
curl -sS -o "$TMP/o" -w '  PutBlock2 http=%{http_code}\n' -X PUT --data-binary "@$TMP/b2" "$UP5&comp=block&blockid=$(enc "$id2")"; head -c 300 "$TMP/o"
printf '<?xml version="1.0" encoding="utf-8"?><BlockList><Latest>%s</Latest><Latest>%s</Latest></BlockList>' "$id1" "$id2" > "$TMP/bl"
curl -sS -o "$TMP/o" -w '  PutBlockList(no x-ms-version) http=%{http_code}\n' -X PUT --data-binary "@$TMP/bl" "$UP5&comp=blocklist"; head -c 400 "$TMP/o"; echo
curl -sS -o "$TMP/o" -w '  PutBlockList(x-ms-version) http=%{http_code}\n' -X PUT -H 'x-ms-version: 2023-11-03' --data-binary "@$TMP/bl" "$UP5&comp=blocklist"; head -c 400 "$TMP/o"; echo
twirp FinalizeCacheEntryUpload "$(fin "$K5" 8000000)"
twirp GetCacheEntryDownloadURL "$(get "$K5")"
DL5=$(echo "$TW_BODY" | jq -r '.signed_download_url // empty')
curl -sS "$DL5" | sha256sum; sha256sum < "$TMP/b12"

section key-characters
printf 'x\n' > "$TMP/x"
long=$(printf 'a%.0s' $(seq 1 600))
i=0
while IFS= read -r suffix; do
  i=$((i+1))
  key="$P/chars/$suffix"
  key=${key:0:${#key}}
  echo "--- key #$i (len ${#key}): $(printf '%q' "$key" | head -c 120)"
  put_entry "$key" "$TMP/x" </dev/null >/dev/null 2>&1
  echo "  create/finalize: $(echo "$TW_BODY" | head -c 200)"
  twirp GetCacheEntryDownloadURL "$(get "$key")" </dev/null >/dev/null
  mk=$(echo "$TW_BODY" | jq -r '.matched_key // empty')
  if [ "$mk" = "$key" ]; then echo "  roundtrip: exact"; else echo "  roundtrip: MISMATCH matched=$(printf '%q' "$mk" | head -c 120) body=$(echo "$TW_BODY" | redact | head -c 200)"; fi
done <<EOF
with space
unicodé-日本-🎉
percent%41%2F
hash#frag?q=1&b
back\\slash
comma,here
UPPERlower
dot/./dotdot/../x
trailing/
double//slash
quote"'
${long:0:$((512 - ${#P} - 7))}
${long:0:$((513 - ${#P} - 7))}
EOF
echo "--- control chars"
for ctl in $'nl\nx' $'tab\tx'; do
  key="$P/chars/$ctl"
  put_entry "$key" "$TMP/x" </dev/null >/dev/null 2>&1
  echo "  $(printf '%q' "$key"): $(echo "$TW_BODY" | head -c 200)"
done
echo "--- case: upper/lower collision"
put_entry "$P/case/abc" "$TMP/x" >/dev/null; echo "  lower: $TW_BODY"
put_entry "$P/case/ABC" "$TMP/x" >/dev/null; echo "  upper: $TW_BODY"
twirp GetCacheEntryDownloadURL "$(get "$P/case/ABC")"

section version-format
put_entry "$P/ver" "$TMP/x" "human-readable-version-string"
twirp GetCacheEntryDownloadURL "$(jq -cn --arg key "$P/ver" '{key:$key, version:"human-readable-version-string"}')"
put_entry "$P/ver" "$TMP/x" "$(printf other | sha256sum | cut -d' ' -f1)"
echo "(same key, second version created above)"

section unfinalized-and-bad-finalize
twirp CreateCacheEntry "$(req "$P/unfinalized")"
twirp GetCacheEntryDownloadURL "$(get "$P/unfinalized")"
twirp CreateCacheEntry "$(req "$P/unfinalized")"
twirp CreateCacheEntry "$(req "$P/badsize")"
UPB=$(echo "$TW_BODY" | jq -r '.signed_upload_url // empty')
curl -sS -o /dev/null -w '  PutBlob http=%{http_code}\n' -X PUT -H 'x-ms-blob-type: BlockBlob' --data-binary "@$TMP/hello" "$UPB"
twirp FinalizeCacheEntryUpload "$(fin "$P/badsize" 99)"
twirp CreateCacheEntry "$(req "$P/noupload")"
twirp FinalizeCacheEntryUpload "$(fin "$P/noupload" 0)"

section rest-list
sleep 2
curl -sS -H "Authorization: Bearer $GITHUB_TOKEN" -H 'Accept: application/vnd.github+json' \
  "$API/actions/caches?key=$(enc "$P/")&per_page=100" > "$TMP/list.json"
jq -c '{total_count}' "$TMP/list.json"
jq -r '.actions_caches[] | [.id, .ref, .size_in_bytes, (.version|.[0:12]), .created_at, .last_accessed_at, .key] | @tsv' "$TMP/list.json" | head -60
echo "fields: $(jq -c '.actions_caches[0] | keys' "$TMP/list.json")"
echo "--- list without key filter, first page, sort=created_at"
curl -sS -H "Authorization: Bearer $GITHUB_TOKEN" "$API/actions/caches?per_page=5&sort=created_at&direction=desc" | jq -c '{total_count, first: [.actions_caches[] | .key]}'
curl -sS -I -H "Authorization: Bearer $GITHUB_TOKEN" "$API/actions/caches?per_page=1" | tr -d '\r' | grep -iE '^(x-ratelimit|link)' | sed 's/^/  /'
curl -sS -H "Authorization: Bearer $GITHUB_TOKEN" "$API/actions/cache/usage" | jq -c .

section rest-delete-and-recreate
ID1=$(jq -r --arg k "$K1" '.actions_caches[] | select(.key == $k) | .id' "$TMP/list.json" | head -1)
echo "K1 id=$ID1"
curl -sS -o "$TMP/o" -w '  DELETE by id http=%{http_code}\n' -X DELETE -H "Authorization: Bearer $GITHUB_TOKEN" "$API/actions/caches/$ID1"; head -c 300 "$TMP/o"; echo
twirp GetCacheEntryDownloadURL "$(get "$K1")"
put_entry "$K1" "$TMP/second"
twirp GetCacheEntryDownloadURL "$(get "$K1")"
DLR=$(echo "$TW_BODY" | jq -r '.signed_download_url // empty')
[ -n "$DLR" ] && curl -sS "$DLR"
echo "--- DELETE by key+ref"
curl -sS -o "$TMP/o" -w '  DELETE by key http=%{http_code}\n' -X DELETE -H "Authorization: Bearer $GITHUB_TOKEN" "$API/actions/caches?key=$(enc "$P/hello2")&ref=$(enc "$GITHUB_REF")"; head -c 600 "$TMP/o"; echo
echo "--- Twirp DeleteCacheEntry attempt"
twirp DeleteCacheEntry "$(req "$P/empty")"

section throughput
head -c $((128*1024*1024)) /dev/urandom > "$TMP/big"
BIGSHA=$(sha256sum < "$TMP/big" | cut -d' ' -f1)
twirp CreateCacheEntry "$(req "$P/big")"
UPG=$(echo "$TW_BODY" | jq -r '.signed_upload_url // empty')
split -b $((16*1024*1024)) -d -a 2 "$TMP/big" "$TMP/part."
start=$(date +%s.%N)
ids=()
for f in "$TMP"/part.*; do
  n=${f##*.}; id=$(printf '%08d' "$((10#$n))" | base64); ids+=("$id")
  curl -sS -o /dev/null -w "%{http_code} " -X PUT --data-binary "@$f" "$UPG&comp=block&blockid=$(enc "$id")" &
done; wait; echo
{ printf '<?xml version="1.0" encoding="utf-8"?><BlockList>'; for id in "${ids[@]}"; do printf '<Latest>%s</Latest>' "$id"; done; printf '</BlockList>'; } > "$TMP/bl"
curl -sS -o /dev/null -w '  PutBlockList http=%{http_code}\n' -X PUT --data-binary "@$TMP/bl" "$UPG&comp=blocklist"
end=$(date +%s.%N); echo "  upload 128MiB parallel(8): $(awk "BEGIN{print $end - $start}") s"
twirp FinalizeCacheEntryUpload "$(fin "$P/big" $((128*1024*1024)))"
twirp GetCacheEntryDownloadURL "$(get "$P/big")"
DLG=$(echo "$TW_BODY" | jq -r '.signed_download_url // empty')
curl -sS -o /dev/null -w '  single-stream download: %{time_total}s speed=%{speed_download}B/s\n' "$DLG"
start=$(date +%s.%N)
for i in $(seq 0 7); do s=$((i*16*1024*1024)); e=$((s+16*1024*1024-1)); curl -sS -o "$TMP/dl.$i" -H "Range: bytes=$s-$e" "$DLG" & done; wait
end=$(date +%s.%N); echo "  8x16MiB range download: $(awk "BEGIN{print $end - $start}") s"
cat "$TMP"/dl.? | sha256sum | cut -d' ' -f1; echo "$BIGSHA"
for n in 1 2 3 4 5; do curl -sS -o /dev/null -w '  4KiB range latency: %{time_starttransfer}s\n' -H "Range: bytes=$((n*1000000))-$((n*1000000+4095))" "$DLG"; done
for n in 1 2 3; do twirp GetCacheEntryDownloadURL "$(get "$P/big")" | sed -E 's/resp=.*//'; done

section cleanup
curl -sS -H "Authorization: Bearer $GITHUB_TOKEN" "$API/actions/caches?key=$(enc "$P/")&per_page=100" |
  jq -r '.actions_caches[].id' | while read -r id; do
    curl -sS -o /dev/null -w "%{http_code} " -X DELETE -H "Authorization: Bearer $GITHUB_TOKEN" "$API/actions/caches/$id"
  done; echo
echo done
