PREFIX ?= $(HOME)/.local
BIN := target/release/agentbox

ARCH := $(shell uname -m | sed 's/arm64/aarch64/')

.PHONY: build install clean guest

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
