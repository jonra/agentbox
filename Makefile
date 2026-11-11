PREFIX ?= $(HOME)/.local
BIN := target/release/agentbox

ARCH := $(shell uname -m | sed 's/arm64/aarch64/')

.PHONY: build install clean guest krunfw

# libkrunfw (the guest kernel) from jonra/libkrunfw: stock 5.6.2 plus a fix for servers that
# bind port 0. agentbox prefers ~/.agentbox/lib over Homebrew's copy (src/vm.rs).
KRUNFW_REPO ?= https://github.com/jonra/libkrunfw.git
KRUNFW_REF ?= v5.6.2-agentbox.1
AGENTBOX_HOME ?= $(HOME)/.agentbox

# macOS refuses Hypervisor.framework to unsigned binaries, so sign ad-hoc with the
# hypervisor entitlement after every build.
build:
	cargo build --release
	@if [ "$$(uname)" = "Darwin" ]; then codesign --force --sign - --entitlements entitlements.plist $(BIN); fi

install: build
	install -d $(PREFIX)/bin
	install -m 755 $(BIN) $(PREFIX)/bin/agentbox

# The in-guest helper is a static Linux binary; build it in a rust:alpine container
# (with agentbox itself, or docker) and embed it on the next `make build`.
guest:
	cd guest && $${CONTAINER_CLI:-agentbox} run --rm -m 4g -v "$$PWD":/src -w /src rust:1-alpine cargo build --release
	install -d guest/bin
	cp guest/target/release/agentbox-guest guest/bin/agentbox-guest-$(ARCH)

clean:
	cargo clean

# Builds the kernel in a debian container (with agentbox itself, or docker), then the dylib.
krunfw:
	install -d target/krunfw $(AGENTBOX_HOME)/lib
	$${CONTAINER_CLI:-agentbox} run --rm -m 12g --cpus 8 -v "$$PWD/scripts":/scripts:ro -v "$$PWD/target/krunfw":/out \
		debian:bookworm-slim sh /scripts/krunfw-kernel.sh $(KRUNFW_REPO) $(KRUNFW_REF) /out
	cc -fPIC -DABI_VERSION=5 -shared -install_name $(AGENTBOX_HOME)/lib/libkrunfw.5.dylib \
		-o $(AGENTBOX_HOME)/lib/libkrunfw.5.dylib target/krunfw/kernel.c
