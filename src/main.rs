mod mcp;
mod registry;
mod store;
mod vm;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::time::Duration;
use store::Store;

/// agentbox: run OCI/Docker images in lightweight microVMs (libkrun), built for AI agents.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pull an image from a registry (Docker Hub, ghcr.io, ...)
    Pull { image: String },
    /// List local images
    Images,
    /// Run a command in a new microVM
    Run {
        /// Keep the filesystem under this name and reuse it on later runs
        #[arg(long)]
        name: Option<String>,
        /// Environment variable KEY=VALUE (or KEY to copy from the host)
        #[arg(short, long)]
        env: Vec<String>,
        /// Bind a host directory: host_path:guest_path[:ro]
        #[arg(short, long)]
        volume: Vec<String>,
        /// Publish a port: host_port:guest_port
        #[arg(short, long)]
        publish: Vec<String>,
        /// Working directory inside the guest
        #[arg(short, long)]
        workdir: Option<String>,
        /// Override the image entrypoint ("" clears it)
        #[arg(long)]
        entrypoint: Option<String>,
        #[arg(long, default_value_t = 2)]
        cpus: u8,
        /// Memory in MiB
        #[arg(long, default_value_t = 1024)]
        memory: u32,
        /// Disable networking inside the VM
        #[arg(long)]
        no_network: bool,
        /// Capture output and kill the VM after this many seconds
        #[arg(long)]
        timeout: Option<u64>,
        image: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Create a named container from an image without running it
    Create { image: String, name: String },
    /// List named containers
    Ps,
    /// Remove named containers
    Rm { names: Vec<String> },
    /// Remove images
    Rmi { images: Vec<String> },
    /// Serve the MCP protocol on stdio (for Claude Code and other agents)
    Mcp,
    #[command(name = "__vm", hide = true)]
    Vm,
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

fn real_main() -> Result<i32> {
    let cli = Cli::parse();
    if let Cmd::Vm = cli.cmd {
        vm::vm_child()?;
        return Ok(0);
    }
    let store = Store::open()?;
    match cli.cmd {
        Cmd::Pull { image } => {
            let m = registry::pull(&store, &image, false)?;
            println!("{} {}", m.reference, m.digest);
        }
        Cmd::Images => {
            println!("{:<40} {:<20} {:>10}", "IMAGE", "DIGEST", "SIZE");
            for i in store.images()? {
                println!("{:<40} {:<20} {:>8.1}MB", i.reference, &i.digest[..19.min(i.digest.len())], i.size_bytes as f64 / 1e6);
            }
        }
        Cmd::Run { name, env, volume, publish, workdir, entrypoint, cpus, memory, no_network, timeout, image, command } => {
            let opts = vm::RunOpts {
                image,
                command,
                entrypoint: entrypoint.map(|e| if e.is_empty() { vec![] } else { vec![e] }),
                env,
                workdir,
                volumes: volume,
                ports: publish,
                cpus: Some(cpus),
                memory_mib: Some(memory),
                name,
                network: Some(!no_network),
            };
            return match timeout {
                None => vm::run_interactive(&store, &opts),
                Some(t) => {
                    let r = vm::run_captured(&store, &opts, Duration::from_secs(t))?;
                    print!("{}", r.output);
                    eprint!("{}", r.stderr);
                    if r.timed_out {
                        eprintln!("agentbox: timed out after {t}s");
                        return Ok(124);
                    }
                    Ok(r.exit_code.unwrap_or(125))
                }
            };
        }
        Cmd::Create { image, name } => vm::create(&store, &image, &name)?,
        Cmd::Ps => {
            println!("{:<24} {:<40}", "NAME", "IMAGE");
            for c in store.containers()? {
                if !c.name.is_empty() {
                    println!("{:<24} {:<40}", c.name, c.image);
                }
            }
        }
        Cmd::Rm { names } => {
            for n in names {
                store.remove_container(&n)?;
            }
        }
        Cmd::Rmi { images } => {
            for i in images {
                store.remove_image(&registry::Reference::parse(&i)?.canonical())?;
            }
        }
        Cmd::Mcp => mcp::serve(store)?,
        Cmd::Vm => unreachable!(),
    }
    Ok(0)
}
