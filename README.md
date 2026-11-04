# agentbox

A small Docker replacement written in Rust and built for AI agents. It pulls ordinary
OCI/Docker images (Docker Hub, ghcr.io, any v2 registry) and runs each command in its own
**libkrun microVM**, a real Linux kernel on Apple's Hypervisor.framework. It needs no
Docker daemon and no root, and boots in well under a second. Agents drive it through
an **MCP server** (`agentbox mcp`) or the CLI.

## Install

Needs Rust plus libkrun (Apple Silicon macOS, or Linux with KVM):

```bash
brew trust --formula slp/krun/libkrun && brew trust --formula slp/krun/virglrenderer-krun
brew install slp/krun/libkrun
make install            # builds, signs with the hypervisor entitlement, copies to ~/.local/bin
```

Always build with `make`, not plain `cargo build`. macOS only allows Hypervisor.framework
to binaries signed with `entitlements.plist`, and the Makefile signs the binary for you.

## CLI

```bash
agentbox pull python:3.12-slim
agentbox run alpine:3.20 sh -c 'echo hi from $(uname -m)'
agentbox run -v "$PWD":/work -w /work python:3.12-slim python script.py
agentbox run -p 8080:8000 python:3.12-slim python -m http.server 8000
agentbox run --no-network --timeout 30 python:3.12-slim python untrusted.py
agentbox run --name dev alpine:3.20 apk add git    # named: filesystem persists...
agentbox run --name dev alpine:3.20 git --version  # ...across runs
agentbox images | agentbox ps | agentbox rm dev | agentbox rmi alpine:3.20
```

Flags: `-e KEY=VAL`, `-v host:guest[:ro]`, `-p host:guest`, `-w dir`, `--entrypoint`,
`--cpus` (default 2), `--memory` MiB (default 1024), `--timeout` secs (captures output and
exits 124 when the VM is killed), `--no-network`, `--name`. The guest's exit code becomes
`agentbox`'s exit code.

## MCP (Claude Code and other agents)

```bash
claude mcp add agentbox -- ~/.local/bin/agentbox mcp
```

Tools: `run` (image, command, env, workdir, volumes, ports, cpus, memory_mib, name, network,
timeout_secs), `pull_image`, `list_images`, `list_containers`, `remove_container`,
`remove_image`. `run` returns the exit code, duration and combined output, and sets
`isError` for a non-zero exit or a timeout.

## How it works

- **Images** (`src/registry.rs`): Registry v2 with anonymous bearer tokens. It picks the
  `linux/<host-arch>` manifest, downloads blobs into a cache and checks each sha256, then
  applies the layers in order with whiteout handling. Parent-dir and symlink escapes are refused.
- **Store** (`src/store.rs`): `~/.agentbox/data`. On macOS this is a case-sensitive APFS
  sparse bundle that is created and mounted on first use, because Linux trees contain
  paths that differ only by case. Each run gets a copy-on-write clone of the image rootfs
  (APFS clonefile), which is deleted afterwards unless you pass `--name`.
- **VMs** (`src/vm.rs`): the CLI re-executes itself as a hidden `__vm` child that configures
  libkrun (root dir via virtio-fs, CPUs/RAM, port map, volumes) and calls
  `krun_start_enter`. The parent captures output, enforces the timeout and cleans up.
  libkrun passes argv on the kernel command line, which can't carry quotes, tabs, newlines
  or non-ASCII. So for images with `/bin/sh`, agentbox writes `/.agentbox/entry.sh`
  (env exports, volume mounts, `cd`, `exec` with your exact argv) and boots that instead.
- **Network**: libkrun TSI proxies guest sockets through the host process. Outbound
  works by default, and `/etc/resolv.conf` is written on each start. `--no-network` removes
  network access entirely.

## Limits (v0.1)

- **Root:** the guest runs as root, and the image `USER` is ignored. Files in the rootfs
  and in volumes are owned by your macOS user on the host side.
- **zstd layers:** not supported yet (gzip and uncompressed only).
- **Private registries:** no login yet, only anonymous pulls.
- **Process model:** each `run` is one VM. There is no `exec` into a running VM and no
  detached/background mode.
- **Concurrent named runs:** two simultaneous runs with the same `--name` share one
  filesystem without locking.
- **Volumes:** they need `/bin/sh` in the image. Shell-less images such as distroless only
  accept printable-ASCII arguments without `"`.
- **Default network:** outbound access is open by default. Use `--no-network` or
  `"network": false` for untrusted code.

## License

Apache-2.0, see [LICENSE](LICENSE). libkrun (Apache-2.0) and libkrunfw (LGPL-2.1/GPL-2.0) are separate projects installed through Homebrew; they are linked at runtime, not bundled.
