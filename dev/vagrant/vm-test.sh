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
#
# It recreates the guests first, and that is not politeness about disk space.
# A guest that converged an hour ago is already in the desired state, so both
# runs report `ok`, the second-run assertion holds, and the test passes having
# exercised only the satisfied path -- the one path that cannot be wrong.
# CI always starts from a destroyed machine and so never had this problem;
# a laptop always has it. `make vm-test QUICK=1` skips the recreate for
# iteration, and says plainly that what it ran is not the full test.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
inventory="$root/dev/vagrant/hosts.vagrant.kdl"
playbook="vagrant"     # the name form; a path would resolve against the cwd


limit=()
if [ "$#" -gt 0 ]; then
    limit=(--limit "$(IFS=,; echo "$*")")
fi

quick=${QUICK:-0}

# Which Vagrant machines to recreate. The inventory names a host
# `vagrant-x86`; the Vagrant machine behind it is `x86`, so the prefix comes
# off. With no host arguments, recreate whatever is currently running, and
# fall back to the autostart machine when nothing is.
machines_to_recreate() {
    if [ "$#" -gt 0 ]; then
        for h in "$@"; do echo "${h#vagrant-}"; done
        return
    fi
    # `--machine-readable` is `<ts>,<machine>,state,<state>`, which is stable
    # across Vagrant versions in a way the human table is not.
    running=$(cd "$root/dev/vagrant" \
        && vagrant status --machine-readable 2>/dev/null \
        | awk -F, '$3 == "state" && $4 == "running" { print $2 }')
    echo "$running"
}

recreate() {
    local machines
    machines=$(machines_to_recreate "$@")

    echo "==> recreating the guests, so the first run has something to do"
    if [ -z "$machines" ]; then
        # Nothing up: `vagrant up` with no name brings up the autostart
        # machine, which is this host's own architecture.
        echo "    (none running; bringing up the autostart machine)"
        (cd "$root/dev/vagrant" && vagrant up)
        return
    fi
    for m in $machines; do
        echo "    $m: destroy, then up"
        (cd "$root/dev/vagrant" && vagrant destroy -f "$m" && vagrant up "$m")
    done
}

if [ "$quick" = 1 ]; then
    cat >&2 <<'MSG'
==> QUICK=1: not recreating the guests.

    The playbook will run against whatever state they are already in. If they
    are converged, both runs report `ok` and this proves only that the ops
    agree the work is done -- it does not prove they can do it. Drop QUICK=1
    before trusting a green.

MSG
else
    recreate "$@"
fi

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

# The summary table has more than one row shape (render.rs). A host that ran
# gets seven columns, `host ok changed would-change skipped failed warnings`,
# optionally suffixed `  exit N` when the binary exited non-zero with no failed
# step. A host that never got as far as running -- a connect error, a build
# failure -- gets `host  failed: <reason>` instead. Matching on column count
# alone is therefore not enough: both of the other shapes would slip through as
# clean. `set -e` also catches a non-zero rustible, so the `exit N` arm is a
# second line of defence rather than the only one.
assert_no_change() {
    awk '
        /^host  *ok  *changed/ { in_table = 1; next }
        in_table && /  exit [0-9]+$/ {
            seen++; bad = 1
            printf "%s: the run binary %s\n", $1, substr($0, index($0, "exit")) > "/dev/stderr"
            next
        }
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

# The mirror of assert_no_change, and the reason the recreate above exists: on
# a guest that is already converged the first run changes nothing, both runs
# report `ok`, and the suite passes having proved only that the ops recognise
# work already done. Reading the same table, this insists the first run had
# something to do. Skipped under QUICK=1, where converged is the expected case.
assert_changed_something() {
    awk '
        /^host  *ok  *changed/ { in_table = 1; next }
        in_table && NF >= 7 && $2 != "failed:" {
            seen++
            if ($3 == 0) {
                printf "%s: nothing changed on the FIRST run\n", $1 > "/dev/stderr"
                bad = 1
            }
        }
        END {
            if (!seen) { print "no hosts in the summary: nothing ran" > "/dev/stderr"; bad = 1 }
            exit bad ? 1 : 0
        }
    '
}

echo "==> first run: converging"
first=$(run | tee /dev/stderr)

if [ "$quick" != 1 ] && ! printf '%s\n' "$first" | assert_changed_something; then
    echo >&2
    cat >&2 <<'MSG'
vm-test failed: the first run changed nothing, so this proved nothing.

The guests were supposed to be recreated before it. A converged guest makes
both runs report `ok` and the second-run assertion hold vacuously, which is
exactly the false green this check exists to stop. Either the recreate did
not take, or the playbook has a step that is satisfied on a fresh guest.
MSG
    exit 1
fi

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
