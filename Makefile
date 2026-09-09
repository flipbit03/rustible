# Rustible.
#
# The default target is the gate CI runs; everything else is opt-in. The `vm-`
# targets need Vagrant, which most work does not: see docs/DEVELOPING.md.

CARGO ?= cargo
VAGRANT ?= vagrant
VAGRANT_DIR := dev/vagrant

.PHONY: default check fmt clippy test doc example integration \
        vm-up vm-up-x86 vm-up-arm vm-test vm-halt vm-destroy vm-status \
        vm-ssh vm-orphans

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

# `vagrant up` with no argument brings up only the machine whose architecture
# matches this host -- x86 on an x86_64 host, arm on Apple silicon -- because
# that is the one the Vagrantfile marks `autostart`. It is the fastest one
# available here, though not necessarily an accelerated one: a host with no
# /dev/kvm interprets its own architecture too. The other machine is always
# emulated, so it is opt-in by name below.
#
# It is also the `primary` machine, which is what lets `make vm-ssh` and
# `vagrant ssh` work without naming a machine.
vm-up:
	cd $(VAGRANT_DIR) && $(VAGRANT) up

# By name, whichever host you are on. On a host of the other architecture this
# is the emulated one: minutes of your life, and the point of having it.
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
	@echo
	@if [ -f $(VAGRANT_DIR)/hosts.vagrant.kdl ]; then \
		echo "inventory: $(VAGRANT_DIR)/hosts.vagrant.kdl"; \
		grep '^    host ' $(VAGRANT_DIR)/hosts.vagrant.kdl || true; \
	else \
		echo "no inventory: nothing is up"; \
	fi

# A shell in a machine. With one running, `make vm-ssh` is enough; otherwise
# name it, `make vm-ssh M=arm`.
vm-ssh:
	cd $(VAGRANT_DIR) && $(VAGRANT) ssh $(M)

# Libvirt domains left behind by a checkout that was deleted before its
# machines were destroyed. Vagrant tracks a domain through
# dev/vagrant/.vagrant/, so removing that directory first orphans the domain:
# it keeps running, holds its disk, and `vagrant destroy` can no longer see it.
vm-orphans:
	@doms=$$(virsh -c qemu:///system list --all --name 2>/dev/null | grep '^vagrant_' || true); \
	known=$$(ls $(VAGRANT_DIR)/.vagrant/machines 2>/dev/null | sed 's/^/vagrant_/' || true); \
	orphans=""; \
	for d in $$doms; do \
		echo "$$known" | grep -qx "$$d" || orphans="$$orphans $$d"; \
	done; \
	if [ -z "$$orphans" ]; then \
		echo "no orphaned domains"; \
	else \
		echo "orphaned libvirt domains (not tracked by this checkout):"; \
		for d in $$orphans; do echo "  $$d"; done; \
		echo; \
		echo "If no other checkout owns them, remove each with:"; \
		for d in $$orphans; do \
			echo "  sudo virsh -c qemu:///system destroy $$d; \
sudo virsh -c qemu:///system undefine $$d --nvram; \
sudo virsh -c qemu:///system vol-delete $$d.img --pool default"; \
		done; \
	fi

# Stop the machines, keeping their disks.
vm-halt:
	cd $(VAGRANT_DIR) && $(VAGRANT) halt

# Delete them and their disks. The boxes stay in ~/.vagrant.d; remove those
# with `vagrant box prune`.
vm-destroy:
	cd $(VAGRANT_DIR) && $(VAGRANT) destroy -f
