PREFIX ?= $(HOME)/.local
BIN := target/release/agentbox

.PHONY: build install clean

# macOS refuses Hypervisor.framework to unsigned binaries, so sign ad-hoc with the
# hypervisor entitlement after every build.
build:
	cargo build --release
	@if [ "$$(uname)" = "Darwin" ]; then codesign --force --sign - --entitlements entitlements.plist $(BIN); fi

install: build
	install -d $(PREFIX)/bin
	install -m 755 $(BIN) $(PREFIX)/bin/agentbox

clean:
	cargo clean
