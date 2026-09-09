# Rustible.
#
# The default target is the gate CI runs; everything else is opt-in. The `vm-`
# targets need Vagrant, which most work does not: see docs/DEVELOPING.md.

CARGO ?= cargo
VAGRANT ?= vagrant
VAGRANT_DIR := dev/vagrant

.PHONY: default check fmt clippy test doc example integration \
        vm-up vm-up-x86 vm-up-arm vm-test vm-halt vm-destroy vm-status

default: check

## ---------------------------------------------------------------- the gate

# What CI runs, in the order that fails soonest.
check: fmt clippy test doc example

fmt:
	$(CARGO) fmt --all --check

clippy:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

test:
	$(CARGO) test --workspace

doc:
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc --workspace --no-deps --lib

# examples/workspace is outside the cargo workspace on purpose, so
# `cargo test --workspace` never compiles it. This is what catches a public
# API change that breaks the generated layout.
example:
	$(CARGO) build --manifest-path examples/workspace/Cargo.toml

# The container tier. Needs docker.
integration:
	RUSTIBLE_INTEGRATION=1 $(CARGO) test -p rustible-std --tests

## ------------------------------------------------------- the machine tier

# Real virtual machines: a real SSH transport, real sudo, a live /proc/sys and
# a real init system, none of which a container models. Optional day to day,
# and expected of a new operation before it merges.

# The machine matching this host's architecture, which is the accelerated one.
vm-up:
	cd $(VAGRANT_DIR) && $(VAGRANT) up

vm-up-x86:
	cd $(VAGRANT_DIR) && $(VAGRANT) up x86

vm-up-arm:
	cd $(VAGRANT_DIR) && $(VAGRANT) up arm

# Runs against whichever machines are up, twice, and fails if the second run
# changes anything. Pass HOSTS=vagrant-arm to limit it.
vm-test: example
	$(CARGO) build --release -p rustible-cli
	$(VAGRANT_DIR)/vm-test.sh $(HOSTS)

vm-status:
	cd $(VAGRANT_DIR) && $(VAGRANT) status

# Stop the machines, keeping their disks.
vm-halt:
	cd $(VAGRANT_DIR) && $(VAGRANT) halt

# Delete them and their disks. The boxes stay in ~/.vagrant.d; remove those
# with `vagrant box prune`.
vm-destroy:
	cd $(VAGRANT_DIR) && $(VAGRANT) destroy -f
