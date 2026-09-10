#!/usr/bin/env bash
#
# Run examples/workspace/playbooks/vagrant.rs against whichever Vagrant
# machines are up, and prove it is idempotent.
#
# A playbook run that reports `changed` proves the operation did something. It
# does not prove the operation was right: an op that rewrites a correct file
# every time also reports `changed`, and every op in this repository claims not
# to. So the run happens twice and the second one must report nothing changed.
# That second run is the test; the first is only setup.
#
# Invoked by `make vm-test`. Takes optional host names to limit to.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
inventory="$root/dev/vagrant/hosts.vagrant.kdl"
playbook="vagrant"     # the name form; a path would resolve against the cwd

if [ ! -f "$inventory" ]; then
    cat >&2 <<MSG
no $inventory

That file is written by \`vagrant up\`, and it lists the machines that are
running. Bring one up first:

    make vm-up          # the architecture of this host: fast
    make vm-up-arm      # the aarch64 machine
    make vm-up-x86      # the x86_64 machine

See docs/DEVELOPING.md for what each costs on your platform.
MSG
    exit 1
fi

limit=()
if [ "$#" -gt 0 ]; then
    limit=(--limit "$(IFS=,; echo "$*")")
fi

run() {
    # `${limit[@]+"${limit[@]}"}` rather than `"${limit[@]}"`: macOS ships bash
    # 3.2, where expanding an empty array under `set -u` is an unbound-variable
    # error. bash 5 on Linux allows it, so this only fails on half the
    # platforms the machine tier is supposed to cover.
    "$root/target/release/rustible" \
        --workspace "$root/examples/workspace" \
        --inventory "$inventory" \
        playbook run ${limit[@]+"${limit[@]}"} "$playbook"
}

# The summary table has two row shapes. A host that ran gets seven columns,
# `host ok changed would-change skipped failed warnings`. A host that never got
# as far as running -- a connect error, a build failure -- gets
# `host  failed: <reason>` instead (render.rs), which is why matching on column
# count alone is not enough: such a row would be skipped and the host counted
# as fine.
assert_no_change() {
    awk '
        /^host  *ok  *changed/ { in_table = 1; next }
        in_table && $2 == "failed:" {
            seen++; bad = 1
            printf "%s: %s\n", $1, substr($0, index($0, "failed:")) > "/dev/stderr"
            next
        }
        # Columns: host ok changed would-change skipped failed warnings.
        # `skipped` is deliberately not checked: ctx.skip() is legitimate
        # playbook logic, not a failure. `would-change` is only ever nonzero
        # under --check, which this script does not pass, but it is asserted
        # anyway so that adding a --check pass later cannot pass vacuously.
        in_table && NF >= 7 {
            seen++
            if ($3 != 0) { printf "%s: %s changed on the second run\n", $1, $3 > "/dev/stderr"; bad = 1 }
            if ($4 != 0) { printf "%s: %s would still change\n", $1, $4 > "/dev/stderr"; bad = 1 }
            if ($6 != 0) { printf "%s: %s failed\n", $1, $6 > "/dev/stderr"; bad = 1 }
        }
        END {
            # No rows at all means nothing ran, which is not a pass.
            if (!seen) { print "no hosts in the summary: nothing ran" > "/dev/stderr"; bad = 1 }
            exit bad ? 1 : 0
        }
    '
}

echo "==> first run: converging"
run

echo
echo "==> second run: must change nothing"
second=$(run | tee /dev/stderr)

if ! printf '%s\n' "$second" | assert_no_change; then
    echo >&2
    echo "vm-test failed: the playbook is not idempotent on these machines." >&2
    exit 1
fi

echo
echo "vm-test passed: converged, and the second run changed nothing."
