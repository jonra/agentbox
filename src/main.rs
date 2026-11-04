mod container;
mod mcp;
mod registry;
mod store;
mod vm;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use container::{Container, ExecOpts};
use serde_json::json;
use std::io::Write;
use std::time::Duration;
use store::Store;

/// agentbox: a Docker-compatible CLI that runs OCI/Docker images in libkrun microVMs.
#[derive(Parser)]
#[command(name = "agentbox", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// Flags shared by `run` and `create` (Docker's names and short forms).
#[derive(Args, Clone)]
struct CreateArgs {
    /// Assign a name to the container
    #[arg(long)]
    name: Option<String>,
    /// Set environment variables (KEY=VALUE, or KEY to copy from the host)
    #[arg(short = 'e', long = "env")]
    env: Vec<String>,
    /// Read environment variables from a file
    #[arg(long = "env-file")]
    env_file: Vec<String>,
    /// Bind mount a host path or named volume: SRC:DEST[:ro]
    #[arg(short = 'v', long = "volume")]
    volume: Vec<String>,
    /// Publish a port: [ip:]host_port:container_port
    #[arg(short = 'p', long = "publish")]
    publish: Vec<String>,
    /// Working directory inside the container
    #[arg(short = 'w', long = "workdir")]
    workdir: Option<String>,
    /// Overwrite the image's ENTRYPOINT ("" clears it)
    #[arg(long)]
    entrypoint: Option<String>,
    /// Number of CPUs (rounded up)
    #[arg(long)]
    cpus: Option<f32>,
    /// Memory limit, e.g. 512m, 2g (default 1g)
    #[arg(short = 'm', long)]
    memory: Option<String>,
    /// Network mode: "none" disables networking; anything else is outbound via the host
    #[arg(long)]
    network: Option<String>,
    /// Same as --network none
    #[arg(long, hide = true)]
    no_network: bool,
    /// Automatically remove the container when it exits
    #[arg(long)]
    rm: bool,
    /// Accepted for Docker compatibility (attached runs are always interactive)
    #[arg(short = 'i', long)]
    interactive: bool,
    /// Accepted for Docker compatibility
    #[arg(short = 't', long)]
    tty: bool,
    /// Not supported yet: the guest runs as root
    #[arg(short = 'u', long)]
    user: Option<String>,
    image: String,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create and run a new container from an image
    Run {
        /// Run in the background and print the container ID
        #[arg(short = 'd', long)]
        detach: bool,
        /// (agentbox) Capture output and kill the container after this many seconds
        #[arg(long)]
        timeout: Option<u64>,
        #[command(flatten)]
        args: CreateArgs,
    },
    /// Create a new container without starting it
    Create(CreateArgs),
    /// Start one or more stopped containers
    Start {
        /// Attach to the container's output (single container)
        #[arg(short = 'a', long)]
        attach: bool,
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Stop one or more running containers
    Stop {
        /// Seconds to wait before killing
        #[arg(short = 't', long = "time", default_value_t = 10)]
        time: u64,
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Restart one or more containers
    Restart {
        #[arg(short = 't', long = "time", default_value_t = 10)]
        time: u64,
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Kill one or more running containers
    Kill {
        /// Signal to send (default KILL)
        #[arg(short = 's', long, default_value = "KILL")]
        signal: String,
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Remove one or more containers
    Rm {
        /// Force the removal of a running container
        #[arg(short = 'f', long)]
        force: bool,
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// List containers
    #[command(alias = "ls")]
    Ps {
        /// Show all containers (default shows just running)
        #[arg(short = 'a', long)]
        all: bool,
        /// Only display container IDs
        #[arg(short = 'q', long)]
        quiet: bool,
    },
    /// Fetch the logs of a container
    Logs {
        /// Follow log output
        #[arg(short = 'f', long)]
        follow: bool,
        /// Number of lines to show from the end ("all" by default)
        #[arg(short = 'n', long, default_value = "all")]
        tail: String,
        container: String,
    },
    /// Execute a command in a running container
    Exec {
        /// Set environment variables
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        /// Working directory inside the container
        #[arg(short = 'w', long = "workdir")]
        workdir: Option<String>,
        /// Accepted for Docker compatibility
        #[arg(short = 'i', long)]
        interactive: bool,
        /// Accepted for Docker compatibility
        #[arg(short = 't', long)]
        tty: bool,
        container: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Return low-level information on containers or images (JSON)
    Inspect {
        #[arg(required = true)]
        targets: Vec<String>,
    },
    /// Block until containers stop, then print their exit codes
    Wait {
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Pull an image from a registry (Docker Hub, ghcr.io, ...)
    Pull { image: String },
    /// List images
    Images {
        /// Only show image IDs
        #[arg(short = 'q', long)]
        quiet: bool,
    },
    /// Remove one or more images
    Rmi {
        /// Remove even if containers use it
        #[arg(short = 'f', long)]
        force: bool,
        #[arg(required = true)]
        images: Vec<String>,
    },
    /// Load images from a `docker save` tar archive (stdin by default)
    Load {
        /// Read from a tar archive file instead of stdin
        #[arg(short = 'i', long)]
        input: Option<String>,
        /// (agentbox) Name to give the image (single-image archives only)
        #[arg(long)]
        tag: Option<String>,
    },
    /// Create a tag TARGET_IMAGE that refers to SOURCE_IMAGE
    Tag { source: String, target: String },
    /// Manage named volumes
    Volume {
        #[command(subcommand)]
        cmd: VolumeCmd,
    },
    /// Manage containers
    Container {
        #[command(subcommand)]
        cmd: ContainerCmd,
    },
    /// Show version information
    Version,
    /// Display system-wide information
    Info,
    /// Serve the MCP protocol on stdio (for Claude Code and other agents)
    Mcp,
    #[command(name = "__vm", hide = true)]
    Vm,
    #[command(name = "__supervise", hide = true)]
    Supervise { id: String },
}

#[derive(Subcommand)]
enum VolumeCmd {
    /// List volumes
    Ls,
    /// Create a volume
    Create { name: String },
    /// Remove volumes
    Rm {
        #[arg(required = true)]
        names: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ContainerCmd {
    /// List containers
    Ls {
        #[arg(short = 'a', long)]
        all: bool,
        #[arg(short = 'q', long)]
        quiet: bool,
    },
    /// Remove all stopped containers
    Prune {
        /// Accepted for Docker compatibility (never prompts)
        #[arg(short = 'f', long)]
        force: bool,
    },
}

fn main() {
    match real_main() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("agentbox: {e:#}");
            std::process::exit(125);
        }
    }
}

fn parse_memory(s: &str) -> Result<u32> {
    let t = s.trim().to_ascii_lowercase();
    let t = t.strip_suffix('b').filter(|x| x.ends_with(['k', 'm', 'g'])).unwrap_or(&t);
    let (num, mult) = match t.chars().last() {
        Some('k') => (&t[..t.len() - 1], 1u64 << 10),
        Some('m') => (&t[..t.len() - 1], 1 << 20),
        Some('g') => (&t[..t.len() - 1], 1 << 30),
        Some('b') => (&t[..t.len() - 1], 1),
        _ => (t, 1), // plain number = bytes, as in Docker
    };
    let bytes = (num.parse::<f64>().with_context(|| format!("bad memory size {s:?}"))? * mult as f64) as u64;
    let mib = bytes >> 20;
    if mib < 128 {
        bail!("memory must be at least 128m (got {s})");
    }
    Ok(mib.min(u32::MAX as u64) as u32)
}

fn read_env_file(path: &str) -> Result<Vec<String>> {
    Ok(std::fs::read_to_string(path)
        .with_context(|| format!("reading env file {path}"))?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect())
}

fn to_opts(a: &CreateArgs) -> Result<vm::RunOpts> {
    if a.user.is_some() {
        eprintln!("agentbox: warning: --user is not supported yet; the container runs as root");
    }
    let mut env = vec![];
    for f in &a.env_file {
        env.extend(read_env_file(f)?);
    }
    env.extend(a.env.iter().cloned());
    let network = if a.no_network || a.network.as_deref() == Some("none") { Some(false) } else { Some(true) };
    Ok(vm::RunOpts {
        image: a.image.clone(),
        command: a.command.clone(),
        entrypoint: a.entrypoint.as_ref().map(|e| if e.is_empty() { vec![] } else { vec![e.clone()] }),
        env,
        workdir: a.workdir.clone(),
        volumes: a.volume.clone(),
        ports: a.publish.clone(),
        cpus: a.cpus.map(|c| c.ceil().clamp(1.0, 64.0) as u8),
        memory_mib: a.memory.as_deref().map(parse_memory).transpose()?,
        name: a.name.clone(),
        network,
        rm: a.rm,
    })
}

fn parse_signal(s: &str) -> Result<i32> {
    let u = s.to_ascii_uppercase();
    let u = u.strip_prefix("SIG").unwrap_or(&u);
    Ok(match u {
        "KILL" => libc::SIGKILL,
        "TERM" => libc::SIGTERM,
        "INT" => libc::SIGINT,
        "HUP" => libc::SIGHUP,
        "QUIT" => libc::SIGQUIT,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        n => n.parse().map_err(|_| anyhow!("unknown signal {s}"))?,
    })
}

fn ago(t: u64) -> String {
    let d = container::now().saturating_sub(t);
    match d {
        0..=1 => "Less than a second".into(),
        2..=59 => format!("{d} seconds"),
        60..=119 => "About a minute".into(),
        120..=3599 => format!("{} minutes", d / 60),
        3600..=7199 => "About an hour".into(),
        7200..=172_799 => format!("{} hours", d / 3600),
        _ => format!("{} days", d / 86400),
    }
}

fn status_text(store: &Store, c: &Container) -> String {
    let st = c.state(store);
    match st.status.as_str() {
        "running" => format!("Up {}", ago(st.started_at.unwrap_or(c.created))),
        "exited" => format!(
            "Exited ({}) {} ago",
            st.exit_code.map(|x| x.to_string()).unwrap_or_else(|| "?".into()),
            ago(st.finished_at.or(st.started_at).unwrap_or(c.created))
        ),
        _ => "Created".into(),
    }
}

fn command_text(store: &Store, c: &Container) -> String {
    let img = store.image(&c.image).ok().flatten().unwrap_or_default();
    let mut argv = c.config.entrypoint.clone().unwrap_or(img.entrypoint);
    argv.extend(if c.config.command.is_empty() { img.cmd } else { c.config.command.clone() });
    let s = argv.join(" ");
    let s: String = if s.chars().count() > 20 { s.chars().take(19).collect::<String>() + "…" } else { s };
    format!("\"{s}\"")
}

fn print_table(rows: Vec<Vec<String>>) {
    let cols = rows.first().map(|r| r.len()).unwrap_or(0);
    let widths: Vec<usize> = (0..cols).map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0)).collect();
    for r in rows {
        let line: Vec<String> = r
            .iter()
            .enumerate()
            .map(|(i, c)| if i + 1 == cols { c.clone() } else { format!("{c:<w$}", w = widths[i]) })
            .collect();
        println!("{}", line.join("   ").trim_end());
    }
}

fn ps(store: &Store, all: bool, quiet: bool) -> Result<()> {
    let cs: Vec<Container> = container::list(store)?.into_iter().filter(|c| all || c.state(store).running()).collect();
    if quiet {
        for c in cs {
            println!("{}", c.short_id());
        }
        return Ok(());
    }
    let mut rows = vec![["CONTAINER ID", "IMAGE", "COMMAND", "CREATED", "STATUS", "PORTS", "NAMES"].map(String::from).to_vec()];
    for c in cs {
        let ports: Vec<String> = c
            .config
            .ports
            .iter()
            .filter_map(|p| p.split_once(':').map(|(h, g)| format!("0.0.0.0:{h}->{g}/tcp")))
            .collect();
        rows.push(vec![
            c.short_id().to_string(),
            c.image.clone(),
            command_text(store, &c),
            format!("{} ago", ago(c.created)),
            status_text(store, &c),
            ports.join(", "),
            c.name.clone(),
        ]);
    }
    print_table(rows);
    Ok(())
}

fn split_ref(r: &str) -> (String, String) {
    if let Some((repo, d)) = r.split_once('@') {
        return (repo.to_string(), format!("@{d}"));
    }
    match r.rfind(':') {
        Some(i) if !r[i..].contains('/') => (r[..i].to_string(), r[i + 1..].to_string()),
        _ => (r.to_string(), "latest".into()),
    }
}

/// Run `f` on each named container, reporting per-container errors like Docker does.
fn each(store: &Store, names: &[String], mut f: impl FnMut(&Container) -> Result<()>) -> Result<i32> {
    let mut failed = false;
    for n in names {
        match container::find(store, n).and_then(|c| f(&c)) {
            Ok(()) => println!("{n}"),
            Err(e) => {
                eprintln!("agentbox: {n}: {e:#}");
                failed = true;
            }
        }
    }
    Ok(if failed { 1 } else { 0 })
}

fn real_main() -> Result<i32> {
    let cli = Cli::parse();
    if let Cmd::Vm = cli.cmd {
        vm::vm_child()?;
        return Ok(0);
    }
    let store = Store::open()?;
    match cli.cmd {
        Cmd::Run { detach, timeout, args } => {
            let opts = to_opts(&args)?;
            if let Some(t) = timeout {
                let mut opts = opts;
                if detach {
                    bail!("--timeout and -d don't mix");
                }
                opts.rm = opts.rm || opts.name.is_none();
                let r = container::run_captured(&store, &opts, Duration::from_secs(t))?;
                print!("{}", r.stdout);
                eprint!("{}", r.stderr);
                if r.timed_out {
                    eprintln!("agentbox: timed out after {t}s");
                    return Ok(124);
                }
                return Ok(r.exit_code.unwrap_or(125));
            }
            let c = container::create(&store, &opts)?;
            if detach {
                container::start_detached(&store, &c)?;
                println!("{}", c.id);
            } else {
                return container::start_attached(&store, &c);
            }
        }
        Cmd::Create(args) => {
            let c = container::create(&store, &to_opts(&args)?)?;
            println!("{}", c.id);
        }
        Cmd::Start { attach, containers } => {
            if attach {
                if containers.len() != 1 {
                    bail!("you cannot start and attach multiple containers at once");
                }
                let c = container::find(&store, &containers[0])?;
                return container::start_attached(&store, &c);
            }
            return each(&store, &containers, |c| container::start_detached(&store, c));
        }
        Cmd::Stop { time, containers } => {
            return each(&store, &containers, |c| container::stop(&store, c, Duration::from_secs(time)));
        }
        Cmd::Restart { time, containers } => {
            return each(&store, &containers, |c| {
                container::stop(&store, c, Duration::from_secs(time))?;
                container::start_detached(&store, c)
            });
        }
        Cmd::Kill { signal, containers } => {
            let sig = parse_signal(&signal)?;
            return each(&store, &containers, |c| container::signal(&store, c, sig));
        }
        Cmd::Rm { force, containers } => return each(&store, &containers, |c| container::remove(&store, c, force)),
        Cmd::Ps { all, quiet } | Cmd::Container { cmd: ContainerCmd::Ls { all, quiet } } => ps(&store, all, quiet)?,
        Cmd::Container { cmd: ContainerCmd::Prune { .. } } => {
            let removed = container::prune(&store)?;
            if !removed.is_empty() {
                println!("Deleted Containers:");
                for id in &removed {
                    println!("{id}");
                }
            }
            println!("\nTotal containers removed: {}", removed.len());
        }
        Cmd::Logs { follow, tail, container: name } => {
            let c = container::find(&store, &name)?;
            let tail = if tail == "all" { None } else { Some(tail.parse().context("--tail must be a number or \"all\"")?) };
            container::logs(&store, &c, follow, tail, &mut std::io::stdout())?;
        }
        Cmd::Exec { env, workdir, container: name, command, .. } => {
            let c = container::find(&store, &name)?;
            let o = ExecOpts { command, env, workdir, timeout: None };
            let code = container::exec(&store, &c, &o, &mut |is_err, b| {
                if is_err {
                    let _ = std::io::stderr().write_all(b);
                } else {
                    let mut out = std::io::stdout();
                    let _ = out.write_all(b);
                    let _ = out.flush();
                }
            })?;
            return Ok(code.unwrap_or(124));
        }
        Cmd::Inspect { targets } => {
            let mut out = vec![];
            for t in &targets {
                if let Ok(c) = container::find(&store, t) {
                    let st = c.state(&store);
                    out.push(json!({
                        "Id": c.id, "Name": c.name, "Image": c.image, "Created": c.created,
                        "State": st, "Config": c.config,
                        "Path": c.dir(&store), "LogPath": c.log_path(&store),
                    }));
                } else if let Some(i) = store.image(&registry::Reference::parse(t)?.canonical())? {
                    out.push(serde_json::to_value(i)?);
                } else {
                    bail!("no such object: {t}");
                }
            }
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        Cmd::Wait { containers } => {
            for n in &containers {
                let c = container::find(&store, n)?;
                println!("{}", container::wait(&store, &c)?.map(|x| x.to_string()).unwrap_or_else(|| "-1".into()));
            }
        }
        Cmd::Pull { image } => {
            let m = registry::pull(&store, &image, false)?;
            println!("Digest: {}\nStatus: image is up to date for {}", m.digest, m.reference);
        }
        Cmd::Images { quiet } => {
            let imgs = store.images()?;
            if quiet {
                for i in imgs {
                    println!("{}", &i.digest[7..19.min(i.digest.len())]);
                }
            } else {
                let mut rows = vec![["REPOSITORY", "TAG", "IMAGE ID", "SIZE"].map(String::from).to_vec()];
                for i in imgs {
                    let (repo, tag) = split_ref(&i.reference);
                    rows.push(vec![repo, tag, i.digest.get(7..19).unwrap_or("").to_string(), format!("{:.1}MB", i.size_bytes as f64 / 1e6)]);
                }
                print_table(rows);
            }
        }
        Cmd::Rmi { force, images } => {
            for i in &images {
                let r = registry::Reference::parse(i)?.canonical();
                let users: Vec<String> = container::list(&store)?.into_iter().filter(|c| c.image == r).map(|c| c.name).collect();
                if !users.is_empty() && !force {
                    bail!("image {r} is used by container(s) {}; remove them or use -f", users.join(", "));
                }
                store.remove_image(&r)?;
                println!("Untagged: {r}");
            }
        }
        Cmd::Load { input, tag } => {
            for m in registry::load(&store, input.as_deref().unwrap_or("-"), tag.as_deref())? {
                println!("Loaded image: {}", m.reference);
            }
        }
        Cmd::Tag { source, target } => {
            let src = registry::Reference::parse(&source)?.canonical();
            let dst = registry::Reference::parse(&target)?.canonical();
            let mut meta = store.image(&src)?.ok_or_else(|| anyhow!("no such image: {source}"))?;
            if store.image(&dst)?.is_some() {
                store.remove_image(&dst)?;
            }
            store::clone_tree(&store.image_dir(&src), &store.image_dir(&dst))?;
            meta.reference = dst.clone();
            std::fs::write(store.image_dir(&dst).join("image.json"), serde_json::to_vec_pretty(&meta)?)?;
        }
        Cmd::Volume { cmd } => match cmd {
            VolumeCmd::Ls => {
                let mut rows = vec![vec!["DRIVER".to_string(), "VOLUME NAME".to_string()]];
                for v in store.volumes()? {
                    rows.push(vec!["local".into(), v]);
                }
                print_table(rows);
            }
            VolumeCmd::Create { name } => {
                std::fs::create_dir_all(store.volume_dir(&name)?)?;
                println!("{name}");
            }
            VolumeCmd::Rm { names } => {
                for n in names {
                    let d = store.volume_dir(&n)?;
                    if !d.exists() {
                        bail!("no such volume: {n}");
                    }
                    let path = std::fs::canonicalize(&d)?.to_string_lossy().into_owned();
                    let users: Vec<String> = container::list(&store)?
                        .into_iter()
                        .filter(|c| c.config.volumes.iter().any(|v| v.starts_with(&format!("{path}:"))))
                        .map(|c| c.name)
                        .collect();
                    if !users.is_empty() {
                        bail!("volume {n} is in use by {}", users.join(", "));
                    }
                    store::remove_tree(&d)?;
                    println!("{n}");
                }
            }
        },
        Cmd::Version => {
            println!("agentbox {}\nruntime: libkrun microVMs ({}/{})", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH);
        }
        Cmd::Info => {
            let cs = container::list(&store)?;
            let running = cs.iter().filter(|c| c.state(&store).running()).count();
            println!("Containers: {}\n Running: {running}\n Stopped: {}", cs.len(), cs.len() - running);
            println!("Images: {}\nVolumes: {}", store.images()?.len(), store.volumes()?.len());
            println!("Storage: {}", store.data.display());
            println!("Runtime: libkrun (Hypervisor.framework / KVM)");
        }
        Cmd::Mcp => mcp::serve(store)?,
        Cmd::Supervise { id } => container::supervise(&store, &id)?,
        Cmd::Vm => unreachable!(),
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::{parse_memory, parse_signal, split_ref};

    #[test]
    fn memory_sizes() {
        assert_eq!(parse_memory("512m").unwrap(), 512);
        assert_eq!(parse_memory("2g").unwrap(), 2048);
        assert_eq!(parse_memory("2GB").unwrap(), 2048);
        assert_eq!(parse_memory("1.5g").unwrap(), 1536);
        assert_eq!(parse_memory("268435456").unwrap(), 256); // plain number = bytes, like Docker
        assert!(parse_memory("64m").is_err());
        assert!(parse_memory("lots").is_err());
    }

    #[test]
    fn signals_and_refs() {
        assert_eq!(parse_signal("SIGTERM").unwrap(), libc::SIGTERM);
        assert_eq!(parse_signal("kill").unwrap(), libc::SIGKILL);
        assert_eq!(parse_signal("15").unwrap(), 15);
        assert_eq!(split_ref("ghcr.io/o/a:v1"), ("ghcr.io/o/a".into(), "v1".into()));
        assert_eq!(split_ref("localhost:5000/app"), ("localhost:5000/app".into(), "latest".into()));
    }
}
