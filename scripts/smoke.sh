#!/usr/bin/env bash
# End-to-end smoke test: control plane + MCP agent + admin console.
# Docker is absent here, so no worker joins; the test verifies everything up to
# and including task admission.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
RUN="${RC_SMOKE_DIR:-$(mktemp -d)}/rc-smoke"
BIN="${RC_BIN:-$(cd "$ROOT/.." && pwd)/target/debug}"
rm -rf "$RUN" && mkdir -p "$RUN"

export XDG_CONFIG_HOME="$RUN/config"
export XDG_CACHE_HOME="$RUN/cache"
HTTP=17700
GRPC=17701
PASS=0
FAIL=0

ok()   { echo "  ✓ $1"; PASS=$((PASS+1)); }
bad()  { echo "  ✗ $1"; echo "     $2"; FAIL=$((FAIL+1)); }
check(){ if [ "$1" = "0" ]; then ok "$2"; else bad "$2" "$3"; fi; }

echo "== 1. control plane starts =="
"$BIN/rc-server" --data-dir "$RUN/server" serve \
  --http-addr "127.0.0.1:$HTTP" --grpc-addr "127.0.0.1:$GRPC" \
  --allow-anonymous-agents > "$RUN/server.log" 2>&1 &
SERVER_PID=$!
trap 'kill $SERVER_PID 2>/dev/null' EXIT

for _ in $(seq 1 50); do
  curl -sf "http://127.0.0.1:$HTTP/healthz" >/dev/null 2>&1 && break
  sleep 0.2
done
curl -sf "http://127.0.0.1:$HTTP/healthz" >/dev/null
check $? "server answers /healthz" "$(tail -3 "$RUN/server.log")"

BOOT=$(curl -s "http://127.0.0.1:$HTTP/api/bootstrap")
echo "$BOOT" | grep -q '"needs_setup":true'
check $? "fresh install reports needs_setup" "$BOOT"

echo "== 2. admin console =="
"$BIN/rc-server" --data-dir "$RUN/server" admin --username admin --password supersecret >/dev/null 2>&1
check $? "admin account created" ""

curl -s -c "$RUN/cookies" -X POST "http://127.0.0.1:$HTTP/api/login" \
  -H 'content-type: application/json' -d '{"username":"admin","password":"supersecret"}' \
  | grep -q '"role":"admin"'
check $? "login issues an admin session" ""

curl -s -X POST "http://127.0.0.1:$HTTP/api/login" \
  -H 'content-type: application/json' -d '{"username":"admin","password":"wrong"}' \
  | grep -q '密码错误'
check $? "a wrong password is rejected" ""

curl -so /dev/null -w '%{http_code}' "http://127.0.0.1:$HTTP/api/overview" | grep -q 401
check $? "unauthenticated API access is refused" ""

OVERVIEW=$(curl -s -b "$RUN/cookies" "http://127.0.0.1:$HTTP/api/overview")
echo "$OVERVIEW" | grep -q '"counters"' && echo "$OVERVIEW" | grep -q '"phase_percentiles"'
check $? "overview returns dashboard data" "$OVERVIEW"

SPA=$(curl -s "http://127.0.0.1:$HTTP/")
echo "$SPA" | grep -q 'remote-compile' && echo "$SPA" | grep -q '/assets/'
check $? "embedded SPA is served from the binary" "$(echo "$SPA" | head -c 200)"

curl -s "http://127.0.0.1:$HTTP/tasks/t-123" | grep -q '<div id="root">'
check $? "client-side routes fall back to the shell" ""

curl -so /dev/null -w '%{http_code}' "http://127.0.0.1:$HTTP/api/nope" | grep -q 404
check $? "unknown API paths 404 instead of returning HTML" ""

curl -s "http://127.0.0.1:$HTTP/metrics" | grep -q '^rc_tasks_submitted_total'
check $? "prometheus endpoint exports declared metrics" ""

echo "== 3. agent + MCP =="
"$BIN/rc-agent" configure --server "http://127.0.0.1:$GRPC" >/dev/null 2>&1
check $? "agent configured" ""

PROJECT="$RUN/demo"
mkdir -p "$PROJECT/src"
cat > "$PROJECT/Cargo.toml" <<'EOF'
[package]
name = "demo"
version = "0.1.0"
edition = "2021"
EOF
echo 'fn main() { println!("hi"); }' > "$PROJECT/src/main.rs"
echo 'target/' > "$PROJECT/.gitignore"
mkdir -p "$PROJECT/target/debug" && head -c 200000 /dev/urandom > "$PROJECT/target/debug/big.o"
git -C "$PROJECT" init -q -b main
git -C "$PROJECT" config user.email t@example.com
git -C "$PROJECT" config user.name t
git -C "$PROJECT" add -A
git -C "$PROJECT" commit -qm init

mcp() { printf '%s\n' "$1" | "$BIN/rc-agent" serve 2>/dev/null; }

INIT=$(mcp '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}')
echo "$INIT" | grep -q '"serverInfo"' && echo "$INIT" | grep -q 'remote-compile'
check $? "MCP initialize handshake" "$INIT"

TOOLS=$(mcp '{"jsonrpc":"2.0","id":2,"method":"tools/list"}')
for t in check get_result get_log get_build_profile list_envs prepare_env get_env_status list_workers; do
  echo "$TOOLS" | grep -q "\"$t\"" || { bad "tools/list exposes $t" "$TOOLS"; continue; }
done
echo "$TOOLS" | grep -q '"check"'
check $? "tools/list exposes the §12 surface" ""

WORKERS=$(mcp '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_workers","arguments":{}}}')
echo "$WORKERS" | grep -q 'worker 0 台在线'
check $? "list_workers reaches the control plane over gRPC" "$WORKERS"

# No approved image exists yet, so check must refuse with env_error and tell
# the agent to call prepare_env (§8.3/§8.4) rather than fail obscurely.
CHECK1=$(mcp "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"check\",\"arguments\":{\"path\":\"$PROJECT\"}}}")
echo "$CHECK1" | grep -q 'env_error' && echo "$CHECK1" | grep -q 'prepare_env'
check $? "check without an approved image returns env_error + next step" "$CHECK1"

PREP=$(mcp '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"prepare_env","arguments":{"image":"docker.io/library/rust:1-bookworm","reason":"smoke test"}}}')
echo "$PREP" | grep -q 'pending_approval'
check $? "prepare_env returns immediately, pending approval" "$PREP"

ENV_ID=$(echo "$PREP" | sed -n 's/.*env_id=\(e-[a-z0-9]*\).*/\1/p' | head -1)
[ -n "$ENV_ID" ]
check $? "prepare_env reports an env id" "$PREP"

echo "== 4. image approval gate =="
PENDING=$(curl -s -b "$RUN/cookies" "http://127.0.0.1:$HTTP/api/images?status=pending_approval")
echo "$PENDING" | grep -q "$ENV_ID"
check $? "the approval queue shows the agent's request" "$PENDING"

# Approve it and pretend a worker reported the digest it built.
curl -s -b "$RUN/cookies" -X POST "http://127.0.0.1:$HTTP/api/images/$ENV_ID/approve" | grep -q '"ok":true'
check $? "admin can approve an image" ""

AUDIT=$(curl -s -b "$RUN/cookies" "http://127.0.0.1:$HTTP/api/audit")
echo "$AUDIT" | grep -q 'approve_image'
check $? "approval is written to the audit log" "$AUDIT"

# Give the image a digest so it can be pinned into a fingerprint (§5.1).
DIGEST="sha256:$(printf 'smoke' | shasum -a 256 | cut -c1-64)"
sqlite3 "$RUN/server/rc-server.sqlite" \
  "UPDATE images SET digest='$DIGEST', image_ref='docker.io/library/rust', status='healthy' WHERE id='$ENV_ID';" 2>/dev/null
check $? "image digest recorded (stand-in for a worker build)" ""

echo "== 5. submission path =="
CHECK2=$(mcp "{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"tools/call\",\"params\":{\"name\":\"check\",\"arguments\":{\"path\":\"$PROJECT\",\"wait_secs\":1}}}")
echo "$CHECK2" | grep -q 'task_id=t-'
check $? "check now admits a task and returns a handle" "$CHECK2"

echo "$CHECK2" | grep -q '仍在执行'
check $? "with no worker online the task stays queued (async handoff)" "$CHECK2"

TASKS=$(curl -s -b "$RUN/cookies" "http://127.0.0.1:$HTTP/api/tasks")
echo "$TASKS" | grep -q '"status":"queued"'
check $? "the task is visible in the admin API" "$(echo "$TASKS" | head -c 300)"

TASK_ID=$(echo "$TASKS" | grep -o '"id":"t-[A-Z0-9]*"' | head -1 | cut -d'"' -f4)
DETAIL=$(curl -s -b "$RUN/cookies" "http://127.0.0.1:$HTTP/api/tasks/$TASK_ID")
echo "$DETAIL" | grep -q '"placement"'
check $? "task detail explains why it is still queued" "$(echo "$DETAIL" | head -c 300)"

# The scanner must have excluded target/ (200KB of random bytes) from the sync.
SYNCED=$(echo "$TASKS" | grep -o '"bytes_synced":[0-9]*' | head -1 | cut -d: -f2)
[ "${SYNCED:-999999}" -lt 100000 ]
check $? "build output was excluded from the sync (synced=${SYNCED}B)" "$TASKS"

# Second identical check must hit a cache/dedup path, not enqueue a duplicate.
CHECK3=$(mcp "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/call\",\"params\":{\"name\":\"check\",\"arguments\":{\"path\":\"$PROJECT\",\"wait_secs\":1}}}")
COUNT=$(curl -s -b "$RUN/cookies" "http://127.0.0.1:$HTTP/api/tasks" | grep -o '"total":[0-9]*' | cut -d: -f2)
[ "${COUNT:-0}" = "1" ]
check $? "an identical resubmission dedupes instead of queuing again (total=$COUNT)" "$CHECK3"

PROFILE=$(mcp "{\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"tools/call\",\"params\":{\"name\":\"get_build_profile\",\"arguments\":{\"path\":\"$PROJECT\"}}}")
echo "$PROFILE" | grep -q "$DIGEST"
check $? "get_build_profile hands back a digest-pinned image" "$PROFILE"

echo "== 6. L2 dirty-layer sync =="
# Everything so far was clean and rode the L1 git baseline (synced=0B, which is
# the point). Dirty an uncommitted file and the content-addressed layer must
# carry it.
echo 'fn main() { println!("edited"); }' > "$PROJECT/src/main.rs"
echo 'pub fn helper() {}' > "$PROJECT/src/untracked.rs"
CHECK4=$(mcp "{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"tools/call\",\"params\":{\"name\":\"check\",\"arguments\":{\"path\":\"$PROJECT\",\"wait_secs\":1}}}")
echo "$CHECK4" | grep -q 'task_id=t-'
check $? "dirty workspace produces a new task" "$CHECK4"

TASKS2=$(curl -s -b "$RUN/cookies" "http://127.0.0.1:$HTTP/api/tasks")
SYNCED2=$(echo "$TASKS2" | grep -o '"bytes_synced":[0-9]*' | head -1 | cut -d: -f2)
[ "${SYNCED2:-0}" -gt 0 ]
check $? "modified + untracked files travel through the CAS (synced=${SYNCED2}B)" "$TASKS2"

BLOBS=$(find "$RUN/server/cas/blobs" -type f 2>/dev/null | wc -l | tr -d ' ')
[ "${BLOBS:-0}" -ge 2 ]
check $? "blobs landed in the server CAS (count=$BLOBS)" ""

PINNED=$(curl -s -b "$RUN/cookies" "http://127.0.0.1:$HTTP/api/storage" | grep -o '"pinned":[0-9]*' | head -1 | cut -d: -f2)
[ "${PINNED:-0}" -gt 0 ]
check $? "the queued task pins its dirty blobs against GC (pinned=$PINNED, §4.7)" ""

# Content addressing means a revert costs nothing: the old blob is still there.
echo 'fn main() { println!("hi"); }' > "$PROJECT/src/main.rs"
rm "$PROJECT/src/untracked.rs"
CHECK5=$(mcp "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"check\",\"arguments\":{\"path\":\"$PROJECT\",\"wait_secs\":1}}}")
BLOBS_AFTER=$(find "$RUN/server/cas/blobs" -type f 2>/dev/null | wc -l | tr -d ' ')
[ "$BLOBS_AFTER" = "$BLOBS" ]
check $? "reverting re-uses existing content, uploading nothing new ($BLOBS -> $BLOBS_AFTER)" "$CHECK5"

echo "== 7. alerts and storage =="
STORAGE=$(curl -s -b "$RUN/cookies" "http://127.0.0.1:$HTTP/api/storage")
echo "$STORAGE" | grep -q '"pinned"'
check $? "storage page reports CAS accounting" "$STORAGE"

# Back to a fully clean tree: every file rides the L1 git baseline, so the
# task references no CAS blob at all and nothing is pinned.
echo "$STORAGE" | grep -q '"pinned":0'
check $? "a clean worktree needs no CAS blobs at all (pure L1 baseline)" "$STORAGE"

echo
[ "$FAIL" -eq 0 ]

echo "== 8. multi-root sync =="
# A cargo `path` dependency pointing outside the repository. The literal
# `../sibling` in Cargo.toml has to keep meaning the same thing on the worker,
# which is what the anchor layout is for.
SIBLING="$RUN/sibling"
mkdir -p "$SIBLING/src"
cat > "$SIBLING/Cargo.toml" <<'EOF'
[package]
name = "sibling"
version = "0.1.0"
edition = "2021"
EOF
echo 'pub fn shared() {}' > "$SIBLING/src/lib.rs"
git -C "$SIBLING" init -q -b main
git -C "$SIBLING" config user.email t@example.com
git -C "$SIBLING" config user.name t
git -C "$SIBLING" add -A
git -C "$SIBLING" commit -qm init

cat >> "$PROJECT/Cargo.toml" <<'EOF'

[dependencies]
sibling = { path = "../sibling" }
EOF

# Nothing may leave the machine before the repository says it may.
CHECK_BLOCK=$(mcp "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"check\",\"arguments\":{\"path\":\"$PROJECT\",\"wait_secs\":1}}}")
echo "$CHECK_BLOCK" | grep -q '需要确认'
check $? "an unapproved external root blocks before any upload" "$CHECK_BLOCK"
echo "$CHECK_BLOCK" | grep -q 'extra_roots'
check $? "the block names the exact config to add" "$CHECK_BLOCK"

BLOBS_BEFORE_MR=$(find "$RUN/server/cas/blobs" -type f 2>/dev/null | wc -l | tr -d ' ')

# Approve it the way the message says to.
echo 'extra_roots = ["../sibling"]' >> "$PROJECT/.remote-compile.toml"
CHECK_MR=$(mcp "{\"jsonrpc\":\"2.0\",\"id\":21,\"method\":\"tools/call\",\"params\":{\"name\":\"check\",\"arguments\":{\"path\":\"$PROJECT\",\"wait_secs\":1}}}")
echo "$CHECK_MR" | grep -q 'task_id=t-'
check $? "an approved external root is admitted" "$CHECK_MR"

BLOBS_AFTER_MR=$(find "$RUN/server/cas/blobs" -type f 2>/dev/null | wc -l | tr -d ' ')
[ "${BLOBS_AFTER_MR:-0}" -gt "${BLOBS_BEFORE_MR:-0}" ]
check $? "the sibling's content is uploaded (L2 only, $BLOBS_BEFORE_MR -> $BLOBS_AFTER_MR)" ""

# The manifest must place both roots under a common anchor, with the repository
# itself one level down so `../sibling` resolves.
MR_TASK=$(curl -s -b "$RUN/cookies" "http://127.0.0.1:$HTTP/api/tasks" | grep -o '"id":"t-[^"]*"' | head -1 | cut -d'"' -f4)
MR_DETAIL=$(curl -s -b "$RUN/cookies" "http://127.0.0.1:$HTTP/api/tasks/$MR_TASK")
echo "$MR_DETAIL" | grep -q '"anchor_mount":"demo"'
check $? "the repository is anchored one level down (anchor_mount=demo)" "$MR_DETAIL"
echo "$MR_DETAIL" | grep -q '"mount":"sibling"'
check $? "the sibling is mounted beside it, so ../sibling still resolves" "$MR_DETAIL"
echo "$MR_DETAIL" | grep -q '"mount":"demo","primary":true'
check $? "the repository is recorded as the primary root" "$MR_DETAIL"

# Editing the sibling must invalidate the fingerprint: a cached result computed
# against the old dependency source would be wrong.
FP_BEFORE=$(echo "$MR_DETAIL" | grep -o '"fingerprint":"[^"]*"' | head -1 | cut -d'"' -f4)
echo 'pub fn shared() { let _x = 1; }' > "$SIBLING/src/lib.rs"
mcp "{\"jsonrpc\":\"2.0\",\"id\":22,\"method\":\"tools/call\",\"params\":{\"name\":\"check\",\"arguments\":{\"path\":\"$PROJECT\",\"wait_secs\":1}}}" >/dev/null
MR_TASK2=$(curl -s -b "$RUN/cookies" "http://127.0.0.1:$HTTP/api/tasks" | grep -o '"id":"t-[^"]*"' | head -1 | cut -d'"' -f4)
FP_AFTER=$(curl -s -b "$RUN/cookies" "http://127.0.0.1:$HTTP/api/tasks/$MR_TASK2" | grep -o '"fingerprint":"[^"]*"' | head -1 | cut -d'"' -f4)
[ -n "$FP_BEFORE" ] && [ "$FP_BEFORE" != "$FP_AFTER" ]
check $? "editing the external root changes the fingerprint" "$FP_BEFORE vs $FP_AFTER"

echo "=================================="
echo " passed: $PASS   failed: $FAIL"
echo "=================================="
