# agentbox

A small Docker replacement written in Rust and built for AI agents. It has a
**Docker-compatible CLI**, including `build` (Dockerfiles) and `compose`, and runs ordinary
OCI/Docker images, each container in its own **libkrun microVM**: a real Linux kernel on
Apple's Hypervisor.framework (or KVM on Linux). There is no daemon and no root, and containers
boot in well under a second. Agents drive it through an **MCP server** (`agentbox mcp`) or the CLI.

## Install

agentbox runs on Apple Silicon Macs and needs libkrun from Homebrew:

```bash
brew tap slp/krun
brew trust --formula slp/krun/libkrun slp/krun/virglrenderer-krun   # newer Homebrew only
brew install slp/krun/libkrun
```

**Ready-made binary:** each [release](https://github.com/jonra/agentbox/releases) has a
signed `agentbox-macos-arm64.tar.gz`, built by GitHub Actions:

```bash
curl -fsSL https://github.com/jonra/agentbox/releases/latest/download/agentbox-macos-arm64.tar.gz | tar xz
mkdir -p ~/.local/bin && mv agentbox/agentbox ~/.local/bin/
agentbox version
```

If you downloaded the archive in a browser, clear the quarantine flag first:
`xattr -d com.apple.quarantine ~/.local/bin/agentbox`.

**From source** (needs Rust; Linux with KVM works too, with libkrun built from source):

```bash
make install            # builds, signs with the hypervisor entitlement, copies to ~/.local/bin
```

Always build with `make`, not plain `cargo build`. macOS only allows Hypervisor.framework
to binaries signed with `entitlements.plist`, and the Makefile signs the binary for you.

## Dashboard

`agentbox tui` (or `agentbox ui`) is a terminal dashboard in the spirit of Docker Desktop.

- **Tabs:** containers, images, volumes and compose projects.
- **Live stats:** CPU and memory per VM, ports, and each container's compose project.
- **Containers:** `s` start/stop, `r` restart, `l` logs (follows), `x` exec a command,
  `o` open the published port in the browser, `i` inspect, `d` delete.
- **Images:** `r` runs one in the background, `p` pulls one.
- **Compose:** projects can be started, stopped, restarted or taken down as a whole.
- **Navigation:** `/` filters, `?` lists every key.

```text
 ◆ agentbox   3 running · 3 containers · 6 images · 1 volumes   CPU 8%  MEM 181 MB
  1 Containers (3)     2 Images (6)     3 Volumes (1)     4 Compose (1)
╭ Containers ──────────────────────────────────────────────────────────────────────────╮
│   NAME            IMAGE            STATUS          PORTS            CPU    MEM   PROJECT │
│▌● sleeper         alpine:3.20      Up 14 seconds                    2.7%   56 MB         │
│ ● demo-cache-1    redis:7-alpine   Up 16 seconds   16379->6379/tcp  3.1%   63 MB  demo   │
│ ● demo-web-1      nginx:alpine     Up 16 seconds   18088->80/tcp    2.2%   62 MB  demo   │
╰──────────────────────────────────────────────────────────────────────────────────────╯
  s start/stop  r restart  l logs  x exec  o open  i inspect  d delete  / filter  ? help
```

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
agentbox build -t myapp:dev --build-arg VERSION=1 .
agentbox compose up -d --wait     # compose.yaml in this directory
agentbox images | pull | rmi | tag SRC DST
docker save myapp:latest | agentbox load    # bring images over from Docker
agentbox tui                      # dashboard
agentbox volume create|ls|rm   ·   agentbox container prune   ·   agentbox builder prune   ·   agentbox info|version
```

`run`/`create` flags: `-d`, `--rm`, `--name`, `-e`, `--env-file`, `-v SRC:DEST[:ro]`
(a host directory or file, or a named volume), `-p [ip:]HOST:CONTAINER[/tcp]`, `-w`,
`--entrypoint`, `--cpus`, `-m/--memory` (`512m`, `2g`; default 1g), `--network none`,
`--add-host name:ip`, and `-i`/`-t` (accepted; attached runs are already interactive).
agentbox adds `--timeout SECS`, which captures output, kills the VM at the deadline and exits
with 124. Exit codes follow Docker: the container's own code, 143 after `stop`, 137 after
`kill`, 125 for agentbox errors.

**Container networking:** each container's sockets go out through the host (libkrun TSI).
With `-p`, only the published ports are reachable. Without `-p`, libkrun exposes every port
the container listens on at the same number on the host, unlike Docker. Containers reach
each other through those host ports on `127.0.0.1` (also available as
`host.docker.internal`). `--network none` cuts a container off entirely. Compose adds name
lookup on top (see below).

## Building images

`agentbox build [-t name:tag] [-f Dockerfile] [--build-arg K=V] [--target stage] [--no-cache] [--pull] PATH`
reads a classic Dockerfile and builds it without a daemon or BuildKit:

- **Instructions:** `FROM` (multi-stage, `AS`, digests, `scratch`, global `ARG`s), `ARG`,
  `ENV`, `WORKDIR`, `RUN`, `COPY`/`ADD`, `CMD`, `ENTRYPOINT`, `SHELL`, `EXPOSE`, `USER`,
  `HEALTHCHECK`, `LABEL`, `VOLUME`, `STOPSIGNAL`. Line continuations, comments, the `escape`
  directive and `$VAR`, `${VAR:-default}` and `${VAR:+alt}` work.
- **`COPY`/`ADD`:** `--from=stage|image`, `--chmod`, wildcards, `.dockerignore` (including
  `**` and `!` exceptions, and `<Dockerfile>.dockerignore`), and heredocs
  (`COPY <<EOF /path`). `ADD` also handles URLs and extracts local tar archives.
- **`RUN`:** `RUN <<EOF` heredocs, `--mount=type=cache,target=…` (persists between builds),
  `--mount=type=bind` and `--network=none`. Each `RUN` boots the stage's filesystem in a
  microVM with networking.
- **Layer cache:** after every filesystem step the tree is snapshotted with one APFS
  `clonefile(2)` call (copy-on-write). With a multi-GB PyTorch venv in the stage, a `COPY`
  step including its snapshot takes a few seconds.
  The key chains the parent key, the instruction, the environment and, for `COPY`/`ADD`, the
  content of the copied files. An unchanged prefix is reported as `CACHED`.
  `agentbox builder prune [-a]` clears the cache; `-a` also clears the cache mounts.

## Compose

`agentbox compose [-f FILE]… [-p NAME] [--profile P] [--env-file F] COMMAND` reads
`compose.yaml`, `compose.yml`, `docker-compose.yaml` or `docker-compose.yml`, plus the
matching `*.override.*` file. It searches the current directory, then its parents.

| Command | What it does |
|---|---|
| `up [-d] [--build] [--wait] [--force-recreate] [--no-deps] [--remove-orphans] [SERVICE…]` | Builds or pulls images, creates and starts services in `depends_on` order. Without `-d` it follows the logs; Ctrl-C stops the services. A container is recreated only when its configuration or image changed. |
| `down [-v] [--rmi local\|all] [--remove-orphans]` | Stops and removes the containers, and with `-v` the project's named volumes. |
| `ps [-a]`, `logs [-f] [-n N] [SERVICE…]` | Status, and logs prefixed by service. |
| `build [--no-cache]`, `pull` | Builds or pulls the images. |
| `start`, `stop`, `restart`, `rm [-s]` | Lifecycle of existing containers. |
| `exec SERVICE CMD…`, `run [--rm] SERVICE [CMD…]` | A command in a running service, or a one-off container (dependencies started first). |
| `config [--services]`, `ls`, `images` | The resolved configuration, running projects, and service images. |

Supported service keys:
- **Image and build:** `image`, `build` (`context`, `dockerfile`, `args`, `target`),
  `pull_policy`.
- **Command:** `command` and `entrypoint` (as a string or a list), `working_dir`.
- **Environment:** `environment` (as a map or a list), `env_file`.
- **Ports and volumes:** `ports` (short and long syntax, ranges), `expose`, and `volumes`
  (bind directories and single files, named volumes, `:ro`, long syntax). Missing bind
  directories are created.
- **Dependencies and health:** `depends_on` with `service_started`, `service_healthy` and
  `service_completed_successfully`; `healthcheck` (or the image's `HEALTHCHECK`).
- **Other:** `extra_hosts` (including `host-gateway`), `container_name`, `profiles`,
  `mem_limit`/`cpus`/`deploy.resources.limits`, `network_mode: none`,
  `stop_grace_period`.

At the file level it supports:
- **Interpolation:** `${VAR}`, `${VAR:-default}`, `${VAR:?error}` and `$$`. Values come
  from the shell environment, then `.env`.
- **Volumes:** top-level `volumes` (including `external` and `name`).
- **Merging:** several `-f` files merge, with the `!override` and `!reset` tags.

Services without limits get 4 GB of memory and up to 8 CPUs. libkrun only uses the memory
the guest actually touches.

**How services reach each other:** VMs have no private network, so compose builds name
lookup on the host's ports.
- **Host ports:** each service's container ports (its published `ports`, `expose`, and its
  image's `EXPOSE`) are mapped to host ports. A published port keeps its host number.
  Otherwise the same number is used if it's free, or else a free port is picked.
- **Names:** inside every container, each other service's name resolves to its own loopback
  address (`127.0.1.N`). There, the embedded guest helper (`guest/`, a 460 KB static
  binary) forwards the service's ports to those host ports.

So `postgres:5432` works even when a native Postgres, or a Docker stack, already uses 5432
on the host.

Caveats:
- **Moved ports inside the service:** when a port can't keep its number, it answers on the
  host number inside its own container. `localhost:5432` inside that postgres container
  reaches the host's 5432 instead. Healthchecks are rewritten for this automatically, and
  `up` warns.
- **Undeclared ports:** a service with no declared ports keeps libkrun's default: all of its
  ports are exposed at their own numbers, and other services reach it through
  `127.0.0.1`.
- **Exposure:** like published ports, the mapped internal ports listen on all host
  interfaces.
- **No scaling:** `scale`/`replicas` > 1 isn't supported. `networks` are ignored, as there
  is one shared namespace, and so is `restart:`. `tmpfs` mounts and UDP ports are skipped
  with a warning.

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
| `build_image` | Builds a Dockerfile and returns the end of the build log. |
| `compose` | `up` (detached; waits for healthchecks), `down`, `ps`, `logs`, `build`, `restart`, `stop`, `config` for a compose project directory. |
| `pull_image`, `list_images`, `remove_image` | Images. |

A typical agent loop: `start_container` with `command: ["sleep","infinity"]`, then any
number of `exec` calls in the same environment, then `remove_container`. For a whole stack:
`compose` `up`, then `exec` into the containers (named `<project>-<service>-1`), then `down`.

## Tested with a real stack

The EPCOR Spring Boot project builds and runs under agentbox:

- **`agentbox build` (ML worker):** the PyTorch worker's Dockerfile builds unchanged in about
  9.5 minutes. It has three stages (uv, build, and a distroless runtime). The build stage
  downloads Python, runs `uv pip install` from a hash-locked lockfile, bakes model weights
  and scans shared libraries with `ldd`/`dpkg`; the runtime stage uses `COPY --from --chown`.
  The image runs Python 3.12 and torch 2.14.
- **`agentbox build`:** the app's Dockerfile builds unchanged in about 5 minutes on the
  first run, mostly Maven downloads. It has two stages with pinned digests (Maven, then
  Temurin JRE), with `RUN`s that create users and `COPY --from`.
- **`agentbox compose up -d --wait`:** its `compose.yaml` comes up in about 26 seconds:
  - a one-shot `data-perms` job (`service_completed_successfully`)
  - pgvector with a healthcheck
  - the app (21 Flyway migrations, healthy via `wget localhost:8080/health`)
  - the PyTorch ML worker

  The app reaches `postgres:5432` and `ml:8090` by name, and ML reaches both. The stack ran
  next to the same project under Docker Desktop and a native Postgres on 5432, with an
  override file moving the two published ports.

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
  CPUs/RAM, port map, volumes) and calls `krun_start_enter`. It also raises the open-files
  limit, because virtio-fs needs a host descriptor per open guest file.
  - **Launcher:** libkrun passes argv on the kernel command line, which can't carry quotes,
    tabs, newlines or non-ASCII. So for images with a shell (`/bin/sh`, `/usr/bin/sh` or
    distroless `/busybox/sh`), agentbox writes `/.agentbox/entry.sh` and boots that. The
    script sets the environment, mounts volumes (a single file is mounted through its
    directory plus a symlink), starts the forwarders, `cd`s, and `exec`s your exact argv.
  - **Network files:** `/etc/hosts` and `/etc/resolv.conf` are regenerated on every start.
- **exec**: libkrun can't start a second process in a running VM, so `entry.sh` also
  starts a tiny shell helper in the guest. `agentbox exec` drops a script into
  `/.agentbox/exec/` through the shared rootfs; the helper runs it and leaves stdout,
  stderr and the exit code for the host to stream back (about 0.1–0.2 s overhead).
- **build** (`src/build.rs`): each stage is a plain directory. `RUN` boots it in a VM;
  `COPY`/`ADD` write into it from the host, resolving symlinks inside the stage root.
  Snapshots go to `~/.agentbox/data/build-cache`.
- **compose** (`src/compose.rs`): it parses YAML with `serde_yaml`, plans the port mapping,
  and drives the container module.
- **Guest helper** (`guest/`): a dependency-free Rust program. `make guest` builds it in a
  `rust:alpine` container (with agentbox itself) into `guest/bin/`, and `make build`
  embeds it.

## Limits

- **Root:** the guest runs as root. Image `USER`, `--user`, compose `user:` and
  `COPY --chown` are ignored. Files extracted or copied on the host are owned by your
  macOS user.
- **exec:** needs a shell in the image, and has no TTY or stdin. Commands still running
  when an exec times out are left running.
- **Logs:** they record detached runs only. Attached runs print to your terminal and
  aren't logged. Inside the VM, stdout and stderr share one console.
- **Networking:** as described above. Ports are TCP only.
- **Not supported yet:**
  - zstd layers and private registries (`login`)
  - build secrets and SSH (`RUN --mount=type=secret|ssh`), `ONBUILD` and cross-platform
    builds (`--platform` is ignored)
  - compose `restart:` policies and scaling
- **`ls` warnings:** `ls -l` on some symlinks prints a harmless `ls: <path>` line
  (extended attributes over virtio-fs).

## License

Apache-2.0, see [LICENSE](LICENSE). libkrun (Apache-2.0) and libkrunfw (LGPL-2.1/GPL-2.0)
are separate projects installed through Homebrew; they are linked at runtime, not bundled.
The embedded guest helper is built from `guest/` in this repository.
