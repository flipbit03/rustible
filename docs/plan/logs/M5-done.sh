#!/bin/bash
# The M5 brief's "Done when" block, run with the spike orchestrator
# (`cargo run -p rustible-cli -- --playbook <name> --host <h>` stands in for
# `rustible playbook run playbooks/<name>.rs` until M3 lands). Usage:
#   docs/plan/logs/M5-done.sh [host ...]      default: local
# Output goes to stdout; the milestone saves it as docs/plan/logs/M5-done.txt.
set -u
cd "$(dirname "$0")/../../.."
HOSTS=("${@:-local}")
HOST_ARGS=()
for h in "${HOSTS[@]}"; do HOST_ARGS+=(--host "$h"); done
WS=examples/workspace
run() { echo; echo "\$ $*"; "$@" 2>&1; echo "[exit $?]"; }
remote_sh() { # remote_sh <host> <script>
  if [ "$1" = local ]; then sh -c "$2"; else ssh -o BatchMode=yes "$1" "$2"; fi
}

echo "# M5 done-when, $(date -u +%FT%TZ), hosts: ${HOSTS[*]}"
echo "# rustible $(git rev-parse --short HEAD)"

echo; echo "## 1. cadu/escalation without escalate=true (twice: changed, then ok)"
for h in "${HOSTS[@]}"; do remote_sh "$h" 'sudo -n rm -f /etc/rustible-m5-test'; done
run cargo run -q -p rustible-cli -- --playbook cadu/escalation "${HOST_ARGS[@]}" -v
run cargo run -q -p rustible-cli -- --playbook cadu/escalation "${HOST_ARGS[@]}" -v
for h in "${HOSTS[@]}"; do
  echo "[$h] /etc/rustible-m5-test: $(remote_sh "$h" 'sudo -n cat /etc/rustible-m5-test; sudo -n stat -c "%U %a" /etc/rustible-m5-test')"
done

echo; echo "## 2. cadu/streaming: 50 MB local_file, local_secret in memory, fetch /etc/hostname to out/"
head -c 52428800 /dev/urandom > $WS/files/big.bin
printf 'tok3n-%s\n' "$(head -c 12 /dev/urandom | base64)" > $WS/files/secret.txt
rm -rf $WS/out
echo "local sha256: $(sha256sum $WS/files/big.bin | cut -c1-64) files/big.bin"
echo "local sha256: $(sha256sum $WS/files/secret.txt | cut -c1-64) files/secret.txt"
run cargo run -q -p rustible-cli -- --playbook cadu/streaming "${HOST_ARGS[@]}" -v
echo "fetched:"; find $WS/out -type f -exec sh -c 'printf "  %s: %s\n" "$1" "$(cat "$1")"' _ {} \;
for h in "${HOSTS[@]}"; do
  echo "[$h] leftover run temp dirs: $(remote_sh "$h" 'ls -d /tmp/.rustible-* 2>/dev/null || echo none')"
done

echo; echo "## 3. ctrl-c during cadu/slow (30 s sleep step): cancelled within 10 s, no zombie, next step never ran"
for h in "${HOSTS[@]}"; do
  remote_sh "$h" 'rm -f /tmp/rustible-m5-next-step-ran'
  cargo build -q -p rustible-cli
  LOG=$(mktemp)
  ./target/debug/rustible --playbook cadu/slow --host "$h" -v >"$LOG" 2>&1 &
  CLI=$!
  for i in $(seq 1 240); do grep -q 'hello: protocol' "$LOG" && break; sleep 0.5; done
  sleep 2
  T0=$(date +%s)
  echo "[$h] SIGINT to the orchestrator (pid $CLI)"
  kill -INT "$CLI"; wait "$CLI"; CODE=$?
  echo "[$h] orchestrator exited $CODE after $(( $(date +%s) - T0 )) s"
  sed 's/^/    /' "$LOG"; rm -f "$LOG"
  sleep 1
  echo "[$h] leftover processes: $(remote_sh "$h" 'pgrep -af "cache/rustible/bin/workspace-cadu_slow|sleep 30\$" | grep -v pgrep || echo none')"
  echo "[$h] marker: $(remote_sh "$h" 'ls /tmp/rustible-m5-next-step-ran 2>&1 || true')"
done

echo; echo "## 4. cargo test --workspace"
run cargo test --workspace

echo; echo "## cleanup"
for h in "${HOSTS[@]}"; do
  remote_sh "$h" 'sudo -n rm -f /etc/rustible-m5-test; rm -f /tmp/rustible-m5-next-step-ran'
  echo "[$h] removed /etc/rustible-m5-test and /tmp/rustible-m5-next-step-ran"
done
