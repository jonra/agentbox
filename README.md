# agentbox

A small Docker replacement written in Rust and built for AI agents. It has a
**Docker-compatible CLI** and runs ordinary OCI/Docker images, each container in its own
**libkrun microVM**: a real Linux kernel on Apple's Hypervisor.framework (or KVM on Linux).
There is no daemon and no root, and containers boot in well under a second. Agents drive
it through an **MCP server** (`agentbox mcp`) or the CLI.

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

It uses Docker's command names, flags and output formats:

```bash
agentbox run --rm alpine:3.20 sh -c 'echo hi from $(uname -m)'
agentbox run -d --name db -p 5432:5432 -e POSTGRES_PASSWORD=pw pgvector/pgvector:pg16
agentbox run -d --name web -p 8080:8000 -v "$PWD":/srv -w /srv python:3.12-slim python -m http.server 8000
agentbox ps -a
agentbox logs -f --tail 20 db
agentbox exec -e FOO=bar -w /tmp web sh -c 'echo $FOO; pwd'
agentbox stop web && agentbox start web    # filesystem kept, like Docker
agentbox restart | kill [-s SIG] | wait | inspect | rm [-f]
agentbox create ... && agentbox start -a NAME
agentbox images | pull | rmi | tag SRC DST
docker save myapp:latest | agentbox load    # bring images over from Docker
agentbox volume create|ls|rm   ·   agentbox container prune   ·   agentbox info|version
```

`run`/`create` flags: `-d`, `--rm`, `--name`, `-e`, `--env-file`, `-v SRC:DEST[:ro]`
(a host path or a named volume), `-p [ip:]HOST:CONTAINER[/tcp]`, `-w`, `--entrypoint`,
`--cpus`, `-m/--memory` (`512m`, `2g`; default 1g), `--network none`, and `-i`/`-t`
(accepted; attached runs are already interactive). agentbox adds `--timeout SECS`, which
captures output, kills the VM at the deadline and exits with 124. Exit codes follow
Docker: the container's own code, 143 after `stop`, 137 after `kill`, 125 for agentbox errors.

**Container networking:** each container's sockets go out through the host. Containers reach
each other through published ports on `127.0.0.1` (also available as
`host.docker.internal`); `--network none` cuts a container off entirely.

## MCP (Claude Code and other agents)

```bash
claude mcp add -s user agentbox -- ~/.local/bin/agentbox mcp
```

| Tool | What it does |
|---|---|
| `run` | One-off command in a fresh VM. Waits and returns the exit code and output; the container is removed unless `keep` or `name` is set. |
| `start_container` | Background container (`docker run -d`). Restarts a stopped container of that name with its filesystem intact. |
| `exec` | Command inside a running container. stdout and stderr come back separately. |
| `logs`, `stop_container`, `list_containers`, `remove_container` | Lifecycle management. |
| `pull_image`, `list_images`, `remove_image` | Images. |

A typical agent loop: `start_container` with `command: ["sleep","infinity"]`, then any
number of `exec` calls in the same environment, then `remove_container`.

## Tested with a real stack

The EPCOR Spring Boot compose stack ran unchanged under agentbox: `pgvector/pgvector:pg16`
from Docker Hub, plus the app and ML-worker images loaded from Docker with `docker save | agentbox load`.

| Container | Result |
|---|---|
| Postgres | Ran the entrypoint's init, `chown` and `gosu`, then loaded pgvector 0.8.7. |
| Spring Boot app | Applied 21 Flyway migrations and served `/health`. |
| ML worker | A distroless image with 28 layers (3.4 GB). It loaded PyTorch and served `:8090` to both the host and the app VM. |

The app and the ML worker shared one named volume.

## How it works

- **Images** (`src/registry.rs`): Registry v2 with anonymous bearer tokens. It picks the
  `linux/<host-arch>` manifest, downloads blobs into a cache and checks each sha256, then
  applies the layers in order with whiteout handling. Parent-dir and symlink escapes are
  refused. `load` reads `docker save` archives in both the legacy and OCI layouts.
- **Store** (`src/store.rs`): `~/.agentbox/data`. On macOS this is a case-sensitive APFS
  sparse bundle that is created and mounted on first use, because Linux trees contain
  paths that differ only by case. Each container's rootfs is a copy-on-write clone of its
  image (APFS clonefile).
- **Containers** (`src/container.rs`): daemonless, like podman. Each container has its own
  directory holding config, state, log and rootfs. `run -d` hands the VM to a detached
  `__supervise` process, which writes output to the log and records the exit code. An
  attached `run` forwards Ctrl-C/SIGTERM to the VM, and a VM whose parent dies shuts itself
  down, so no orphans are left holding ports. Host ports are checked before start
  ("port is already allocated").
- **VMs** (`src/vm.rs`): a hidden `__vm` child configures libkrun (root dir via virtio-fs,
  CPUs/RAM, port map, volumes) and calls `krun_start_enter`. libkrun passes argv on the
  kernel command line, which can't carry quotes, tabs, newlines or non-ASCII. So for images
  with a shell (`/bin/sh`, `/usr/bin/sh` or distroless `/busybox/sh`), agentbox writes
  `/.agentbox/entry.sh` (env, volume mounts, `cd`, `exec` with your exact argv) and boots that.
- **exec**: libkrun can't start a second process in a running VM, so `entry.sh` also
  starts a tiny shell helper in the guest. `agentbox exec` drops a script into
  `/.agentbox/exec/` through the shared rootfs; the helper runs it and leaves stdout,
  stderr and the exit code for the host to stream back (about 0.1–0.2 s overhead).

## Limits

- **Root:** the guest runs as root. Image `USER` and `--user` are ignored, and volume
  files are owned by your macOS user on the host side.
- **exec:** needs a shell in the image, and has no TTY or stdin. Commands still running
  when an exec times out are left running.
- **Logs:** they record detached runs only. Attached runs print to your terminal and
  aren't logged. Inside the VM, stdout and stderr share one console.
- **Networking:** containers have no private network and no DNS between containers; use
  published ports on `127.0.0.1`. Ports are TCP only.
- **Not supported yet:** zstd layers, private registries (`login`), `build`, and
  `compose`. For now, build with Docker or Podman and use `load`.

## License

Apache-2.0, see [LICENSE](LICENSE). libkrun (Apache-2.0) and libkrunfw (LGPL-2.1/GPL-2.0)
are separate projects installed through Homebrew; they are linked at runtime, not bundled.
