#!/bin/bash
# The M5 brief's "Done when" block, run through the real CLI that M3 shipped
# (`rustible playbook run <path> [--check] [-v] [--limit <hosts>]`). Usage:
#   docs/plan/logs/M5-done.sh [host ...]      default: local
# The hosts must be members of the `lab` group the three playbooks target.
# Output goes to stdout; the milestone appends it to docs/plan/logs/M5-done.txt.
set -u
ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
HOSTS=("${@:-local}")
LIMIT=$(IFS=,; echo "${HOSTS[*]}")
BIN=/tmp/rustible-m5-bin
export PATH="$BIN/bin:$PATH"

run() { echo; echo "\$ $*"; "$@" 2>&1; echo "[exit $?]"; }
# The inventory's ssh parameters for a host, so a check runs where the
# playbook ran. `local` is this VM; `arm` is the ARM VM over Tailscale.
remote_sh() { # remote_sh <host> <script>
  case "$1" in
    local) sh -c "$2" ;;
    arm)   ssh -o BatchMode=yes cadu@cadu-cogram-vm-arm "$2" ;;
    *)     echo "remote_sh: unknown host $1" >&2; return 1 ;;
  esac
}

echo "# M5 done-when, $(date -u +%FT%TZ), hosts: ${HOSTS[*]} (--limit $LIMIT)"
echo "# rustible $(cd "$ROOT" && git rev-parse --short HEAD), through the M3 CLI"
cargo install -q --path "$ROOT/crates/rustible-cli" --root "$BIN" --force 2>&1 | tail -2
echo "# CLI: $(command -v rustible), $(rustible --version)"
cd "$ROOT/examples/workspace" || exit 1

echo; echo "## 1. cadu/escalation without escalate=true"
echo "## (--check changes nothing, then twice for real: changed, then ok)"
for h in "${HOSTS[@]}"; do remote_sh "$h" 'sudo -n rm -f /etc/rustible-m5-test'; done
run rustible playbook run playbooks/cadu/escalation.rs --check -v --limit "$LIMIT"
for h in "${HOSTS[@]}"; do
  echo "[$h] after --check, /etc/rustible-m5-test: $(remote_sh "$h" 'sudo -n stat -c "%U %a" /etc/rustible-m5-test 2>&1 || true')"
done
run rustible playbook run playbooks/cadu/escalation.rs -v --limit "$LIMIT"
run rustible playbook run playbooks/cadu/escalation.rs -v --limit "$LIMIT"
for h in "${HOSTS[@]}"; do
  echo "[$h] /etc/rustible-m5-test: $(remote_sh "$h" 'sudo -n cat /etc/rustible-m5-test; sudo -n stat -c "%U %a" /etc/rustible-m5-test')"
done

echo; echo "## 2. cadu/streaming: 50 MB local_file, local_secret in memory, fetch /etc/hostname to out/"
head -c 52428800 /dev/urandom > files/big.bin
printf 'tok3n-%s\n' "$(head -c 12 /dev/urandom | base64)" > files/secret.txt
rm -rf out
echo "local sha256: $(sha256sum files/big.bin | cut -c1-64) files/big.bin"
echo "local sha256: $(sha256sum files/secret.txt | cut -c1-64) files/secret.txt"
run rustible playbook run playbooks/cadu/streaming.rs -v --limit "$LIMIT"
echo "fetched:"; find out -type f -exec sh -c 'printf "  %s: %s\n" "$1" "$(cat "$1")"' _ {} \;
for h in "${HOSTS[@]}"; do
  echo "[$h] leftover run temp dirs: $(remote_sh "$h" 'ls -d /tmp/.rustible-* 2>/dev/null || echo none')"
done

echo; echo "## 3. ctrl-c during cadu/slow (30 s sleep step): cancelled within 10 s, no zombie, next step never ran"
for h in "${HOSTS[@]}"; do
  remote_sh "$h" 'rm -f /tmp/rustible-m5-next-step-ran'
  LOG=$(mktemp)
  rustible playbook run playbooks/cadu/slow.rs -v --limit "$h" >"$LOG" 2>&1 &
  CLI=$!
  # `facts:` is printed once the binary is running and has sent its first
  # event, so the 30 s step is in flight two seconds later. The step's own
  # line only prints when it finishes, which is exactly what must not happen.
  for _ in $(seq 1 480); do grep -q 'facts:' "$LOG" && break; sleep 0.5; done
  sleep 2
  # The exact pids to look for afterwards: the playbook binary on the target
  # and its children (the step's `sleep 30`). Matching on the command line
  # instead would also catch anything else on the box that says `sleep 30`,
  # and, over ssh, the very shell that runs this check (its command line
  # carries the pattern), so the candidates are filtered by /proc/<pid>/exe.
  PIDS=$(remote_sh "$h" '
    for p in $(pgrep -f cache/rustible/bin/cadu_slow); do
      case "$(readlink /proc/$p/exe 2>/dev/null)" in
        */cadu_slow-*) echo "$p"; pgrep -P "$p" ;;
      esac
    done' | tr '\n' ' ' | sed 's/ *$//')
  PIDLIST=$(echo "$PIDS" | tr ' ' ',')
  echo "[$h] running before the interrupt:"
  remote_sh "$h" "ps -o pid=,cmd= -p $PIDLIST" | cut -c1-110 | sed 's/^/    /'
  T0=$(date +%s)
  echo "[$h] SIGINT to the orchestrator (pid $CLI)"
  kill -INT "$CLI"; wait "$CLI"; CODE=$?
  echo "[$h] orchestrator exited $CODE after $(( $(date +%s) - T0 )) s"
  sed 's/^/    /' "$LOG"; rm -f "$LOG"
  sleep 1
  echo "[$h] leftover of those pids ($PIDS): $(remote_sh "$h" "ps -o pid=,cmd= -p $PIDLIST || echo none")"
  echo "[$h] marker: $(remote_sh "$h" 'ls /tmp/rustible-m5-next-step-ran 2>&1 || true')"
done

echo; echo "## 4. ctrl-c DURING the 50 MB transfer: answered inside the transfer, not after it"
# Only worth running where the transfer is slow enough to interrupt: locally
# the 50 MB stream takes about a third of a second.
for h in "${HOSTS[@]}"; do
  [ "$h" = arm ] || { echo "[$h] skipped: the local stream finishes too fast to interrupt"; continue; }
  LOG=$(mktemp)
  rustible playbook run playbooks/cadu/streaming.rs -v --limit "$h" >"$LOG" 2>&1 &
  CLI=$!
  for _ in $(seq 1 480); do grep -q 'facts:' "$LOG" && break; sleep 0.5; done
  sleep 15   # well inside a transfer that takes minutes on this link
  echo "[$h] bytes on the target when interrupted: $(remote_sh "$h" 'cat /tmp/.rustible-*/*/big.bin 2>/dev/null | wc -c')"
  T0=$(date +%s)
  echo "[$h] SIGINT to the orchestrator (pid $CLI) mid-transfer"
  kill -INT "$CLI"; wait "$CLI"; CODE=$?
  echo "[$h] orchestrator exited $CODE after $(( $(date +%s) - T0 )) s"
  sed 's/^/    /' "$LOG"; rm -f "$LOG"
  sleep 1
  echo "[$h] leftover run temp dirs: $(remote_sh "$h" 'ls -d /tmp/.rustible-* 2>/dev/null || echo none')"
done

echo; echo "## 5. cargo test --workspace"
(cd "$ROOT" && run cargo test --workspace)

echo; echo "## cleanup"
rm -f files/big.bin files/secret.txt
for h in "${HOSTS[@]}"; do
  remote_sh "$h" 'sudo -n rm -f /etc/rustible-m5-test; rm -f /tmp/rustible-m5-next-step-ran'
  echo "[$h] removed /etc/rustible-m5-test and /tmp/rustible-m5-next-step-ran"
done
