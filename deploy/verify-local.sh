#!/usr/bin/env bash
# Isolated runtime checks. No real tunnel, credentials, agent or GitHub mutation.
# Shell expressions in command strings are evaluated in the container, not on the host.
# shellcheck disable=SC2016
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
scratch=$(mktemp -d)
project="helm-verify-$(date +%s)-$$"
image="${project}-helm"
export TUNNEL_TOKEN=local-verification-no-tunnel
export GH_TOKEN=local-verification-no-github
export HELM_PUBLIC_HOST=helm.example.com
export TARGET_REPO="$scratch/repo"
compose=(docker compose --project-directory "$root" -p "$project"
    -f "$root/docker-compose.yml" -f "$scratch/compose.verify.yml")

cleanup() {
    "${compose[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
    docker image rm "$image" >/dev/null 2>&1 || true
    rm -rf "$scratch"
}
trap cleanup EXIT

# A real clone of Helm, separate from the user's target repo and its .git metadata.
git clone --quiet --no-hardlinks "$root" "$scratch/repo"
cp "$root/deploy/helm.toml.example" "$scratch/helm.toml"
# No GitHub integration in this local test, so background refresh never contacts GitHub.
sed -i '/^\[github\]/,$d' "$scratch/helm.toml"
sed -i 's|command = "claude"|command = "/repo/tests/fixtures/fake-claude.sh"|' "$scratch/helm.toml"
cat > "$scratch/compose.verify.yml" <<EOF
services:
  cloudflared:
    image: $image
    entrypoint: ["/usr/bin/tini", "--"]
    command: ["sleep", "infinity"]
  helm:
    image: $image
    volumes:
      - type: bind
        source: $scratch/helm.toml
        target: /config/helm.toml
        read_only: true
EOF

wait_for_helm() {
    for ((attempt=0; attempt<60; attempt++)); do
        if "${compose[@]}" exec -T helm curl --fail --silent \
            http://127.0.0.1:7878/healthz >/dev/null 2>&1; then
            return
        fi
        sleep 1
    done
    "${compose[@]}" logs --tail=100 helm >&2
    echo 'FAIL: Helm did not start' >&2
    return 1
}

assert_private_network() {
    local helm_id tunnel_id listeners
    helm_id=$("${compose[@]}" ps -q helm)
    tunnel_id=$("${compose[@]}" ps -q cloudflared)
    test "$(docker inspect -f '{{.HostConfig.NetworkMode}}' "$helm_id")" = "container:$tunnel_id"
    for id in "$helm_id" "$tunnel_id"; do
        test "$(docker inspect -f '{{len .HostConfig.PortBindings}}' "$id")" = 0
    done
    listeners=$(ss -H -tlnp 'sport = :7878')
    test -z "$listeners"
}

assert_persistence() {
    "${compose[@]}" exec -T helm sh -ec '
        test "$(sqlite3 /data/helm.db "SELECT count(*) FROM cards WHERE title = '\''Verify persistence'\'';")" = 1
        test "$(cat /home/helm/.claude/verification-marker)" = persistent
        test "$(cat /home/helm/.claude.json)" = "{\"verification_only\":true}"
        test "$(cat /home/helm/.config/gh/verification-marker)" = persistent
        test "$(git config --global user.name)" = "Local verification"
        test "$(cat /worktrees/verification-marker)" = persistent
        git -C /worktrees/verification rev-parse --is-inside-work-tree
        test "$(git -C /worktrees/verification branch --show-current)" = helm/verification
    ' >/dev/null
}

echo 'Building the Helm image with Docker Compose'
"${compose[@]}" build helm > "$scratch/build.log" 2>&1 || {
    tail -n 100 "$scratch/build.log" >&2
    exit 1
}
"${compose[@]}" up -d --no-build
wait_for_helm
assert_private_network
test "$("${compose[@]}" exec -T helm id -u)" = 1000
test "$("${compose[@]}" exec -T helm cat /proc/1/comm)" = tini
"${compose[@]}" exec -T helm sh -ec '
    test "$HOME" = /home/helm
    git --version
    gh --version
    node --version
    claude --version
    cargo --version
    cargo fmt --version
    cargo clippy --version
'
echo 'Running the Helm target repo checks inside the non-root runtime container'
"${compose[@]}" exec -T helm sh -ec '
    cd /repo
    cargo fmt --check
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --locked
' > "$scratch/checks.log" 2>&1 || {
    tail -n 100 "$scratch/checks.log" >&2
    exit 1
}
tail -n 3 "$scratch/checks.log"

test "$("${compose[@]}" exec -T cloudflared curl --silent --output /dev/null \
    --write-out '%{http_code}' http://127.0.0.1:7878/)" = 200
test "$("${compose[@]}" exec -T helm curl --silent --output /dev/null \
    --write-out '%{http_code}' -H 'Host: HELM.EXAMPLE.COM' http://127.0.0.1:7878/)" = 200
test "$("${compose[@]}" exec -T helm curl --silent --output /dev/null \
    --write-out '%{http_code}' -H 'Host: other.example.com' http://127.0.0.1:7878/)" = 403
test "$("${compose[@]}" exec -T helm curl --silent --output /dev/null \
    --write-out '%{http_code}' -H 'Host: helm.example.com' -H 'Origin: https://evil.example' \
    -H 'Sec-Fetch-Site: same-origin' --data 'column_id=1&title=Rejected' \
    http://127.0.0.1:7878/cards)" = 403
test "$("${compose[@]}" exec -T helm curl --silent --output /dev/null \
    --write-out '%{http_code}' -H 'Host: helm.example.com' -H 'Origin: https://helm.example.com' \
    -H 'Sec-Fetch-Site: same-origin' --data 'column_id=1&title=Verify+persistence' \
    http://127.0.0.1:7878/cards)" = 303
"${compose[@]}" logs --no-color helm > "$scratch/helm.log"
if rg -q "aucune exécution d'agent ne sera lancée|agents disabled" "$scratch/helm.log"; then
    cat "$scratch/helm.log" >&2
    exit 1
fi
rg 'agents enabled' "$scratch/helm.log"

"${compose[@]}" exec -T helm sh -ec '
    printf persistent > /home/helm/.claude/verification-marker
    printf "%s" "{\"verification_only\":true}" > /home/helm/.claude.json
    printf persistent > /home/helm/.config/gh/verification-marker
    git config --global user.name "Local verification"
    printf persistent > /worktrees/verification-marker
    git -C /repo worktree add /worktrees/verification -b helm/verification
    sqlite3 /data/helm.db ".backup /data/helm-backup.db"
    test "$(sqlite3 /data/helm-backup.db "PRAGMA integrity_check;")" = ok
'
"${compose[@]}" restart
wait_for_helm
assert_persistence
assert_private_network
"${compose[@]}" up -d --no-build --force-recreate
wait_for_helm
assert_persistence
assert_private_network

# An active fixture agent must be marked interrupted BEFORE any startup recovery.
# That distinguishes graceful SIGTERM forwarding from Docker's eventual SIGKILL.
test "$("${compose[@]}" exec -T helm curl --silent --output /dev/null \
    --write-out '%{http_code}' --data 'column_id=2&title=Signal+verification&agent=claude&description=SCENARIO%3A+group_child' \
    http://127.0.0.1:7878/cards)" = 303
agent_ready=false
for ((attempt=0; attempt<60; attempt++)); do
    if "${compose[@]}" exec -T helm test -s /worktrees/HELM-2.child; then
        agent_ready=true
        break
    fi
    sleep 1
done
test "$agent_ready" = true
"${compose[@]}" stop helm
test "$("${compose[@]}" run --rm --no-deps -T helm sqlite3 /data/helm.db \
    'SELECT status FROM agent_runs ORDER BY id DESC LIMIT 1;')" = interrupted
echo 'PASS: HTTP, host/Origin guards, non-root, Tini/SIGTERM, target checks, private network, SQLite backup, restart and recreation persistence'
echo 'NOT VERIFIED: real Cloudflare Tunnel/Access, real Claude/GitHub logins or agent execution'
