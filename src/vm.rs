//! Running a container = boot a libkrun microVM whose root filesystem is a
//! copy-on-write clone of the image's rootfs.
//!
//! `krun_start_enter` takes over the calling process and `exit()`s with the guest's
//! exit code, so the VM always runs in a child process (`agentbox __vm`) and the
//! parent handles output capture, timeouts and cleanup.

use crate::registry;
use crate::store::{clone_tree, remove_tree, ContainerMeta, ImageMeta, Store};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::ffi::{c_char, CString};
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SPEC_ENV: &str = "AGENTBOX_VM_SPEC";
const MAX_CAPTURE: usize = 1 << 20;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RunOpts {
    pub image: String,
    #[serde(default)]
    pub command: Vec<String>,
    /// Some(vec![]) clears the image entrypoint.
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    /// KEY=VALUE, or KEY to copy it from the host environment.
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub workdir: Option<String>,
    /// host_path:guest_path[:ro]
    #[serde(default)]
    pub volumes: Vec<String>,
    /// host_port:guest_port
    #[serde(default)]
    pub ports: Vec<String>,
    #[serde(default)]
    pub cpus: Option<u8>,
    #[serde(default)]
    pub memory_mib: Option<u32>,
    /// Keep the container's filesystem under this name and reuse it on later runs.
    #[serde(default)]
    pub name: Option<String>,
    /// false = no network at all (default true: outbound via the host).
    #[serde(default)]
    pub network: Option<bool>,
}

/// Everything the `__vm` child needs; passed to it as JSON in an env var.
#[derive(Debug, Serialize, Deserialize)]
struct VmSpec {
    root: String,
    exec: String,
    args: Vec<String>,
    env: Vec<String>,
    workdir: String,
    cpus: u8,
    memory_mib: u32,
    ports: Vec<String>,
    virtiofs: Vec<(String, String)>,
    network: bool,
}

#[derive(Debug, Serialize)]
pub struct RunResult {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u128,
    pub output: String,
    pub stderr: String,
}

struct Prepared {
    spec: VmSpec,
    /// Container dir to delete afterwards (ephemeral runs only).
    cleanup: Option<PathBuf>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn prepare(store: &Store, o: &RunOpts) -> Result<Prepared> {
    let reference = registry::Reference::parse(&o.image)?.canonical();

    let (rootfs, image, cleanup): (PathBuf, ImageMeta, Option<PathBuf>) = match &o.name {
        Some(name) if store.container_dir(name).join("rootfs").exists() => {
            let dir = store.container_dir(name);
            let meta: ContainerMeta = serde_json::from_slice(&fs::read(dir.join("container.json"))?)?;
            let image = store.image(&meta.image)?.ok_or_else(|| anyhow!("image {} for container {name} is gone", meta.image))?;
            (dir.join("rootfs"), image, None)
        }
        _ => {
            let image = match store.image(&reference)? {
                Some(i) => i,
                None => registry::pull(store, &o.image, false)?,
            };
            let (dir, cleanup) = match &o.name {
                Some(n) => (store.container_dir(n), None),
                None => {
                    let d = store.container_dir(&format!(".run-{}-{}", now(), std::process::id()));
                    (d.clone(), Some(d))
                }
            };
            clone_tree(&store.image_dir(&image.reference).join("rootfs"), &dir.join("rootfs"))?;
            let meta = ContainerMeta { name: o.name.clone().unwrap_or_default(), image: image.reference.clone(), created: now() };
            fs::write(dir.join("container.json"), serde_json::to_vec_pretty(&meta)?)?;
            (dir.join("rootfs"), image, cleanup)
        }
    };

    write_network_files(&rootfs)?;

    let mut argv = o.entrypoint.clone().unwrap_or_else(|| image.entrypoint.clone());
    argv.extend(if o.command.is_empty() { image.cmd.clone() } else { o.command.clone() });
    if argv.is_empty() {
        bail!("no command given and image {} has no default command", image.reference);
    }

    let mut env: Vec<String> = image.env.clone();
    for e in &o.env {
        let kv = if e.contains('=') {
            e.clone()
        } else {
            format!("{e}={}", std::env::var(e).unwrap_or_default())
        };
        let k = kv.split('=').next().unwrap_or_default().to_string();
        env.retain(|x| x.split('=').next() != Some(&k));
        env.push(kv);
    }
    for (k, v) in [("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"), ("HOME", "/root")] {
        if !env.iter().any(|x| x.starts_with(&format!("{k}="))) {
            env.push(format!("{k}={v}"));
        }
    }

    // Volumes are virtio-fs shares that the guest has to mount itself.
    let mut virtiofs = vec![];
    let mut mounts = vec![];
    for (i, v) in o.volumes.iter().enumerate() {
        let parts: Vec<&str> = v.split(':').collect();
        let (host, guest, ro) = match parts.as_slice() {
            [h, g] => (*h, *g, false),
            [h, g, "ro"] => (*h, *g, true),
            _ => bail!("bad volume {v:?}; expected host_path:guest_path[:ro]"),
        };
        let host = fs::canonicalize(host).with_context(|| format!("volume host path {host}"))?;
        if !guest.starts_with('/') {
            bail!("volume guest path must be absolute: {guest}");
        }
        let tag = format!("vol{i}");
        mounts.push(format!(
            "mkdir -p {g} && mount -t virtiofs {o}{tag} {g}",
            g = shell_quote(guest),
            o = if ro { "-o ro " } else { "" }
        ));
        virtiofs.push((tag, host.to_string_lossy().into_owned()));
    }
    let workdir = o.workdir.clone().filter(|w| !w.is_empty()).unwrap_or_else(|| {
        if image.workdir.is_empty() { "/".into() } else { image.workdir.clone() }
    });

    // libkrun hands argv/env to the guest on the kernel command line, which only carries
    // printable ASCII and mangles embedded quotes (tabs/newlines even panic the VMM). So
    // when the image has a shell, put the real argv/env in a launcher script and boot
    // `/bin/sh /.agentbox/entry.sh` instead.
    let (exec, args, env) = if has_shell(&rootfs) {
        let mut s = String::from("#!/bin/sh\n# written by agentbox for each run\nset -e\n");
        for kv in &env {
            s.push_str(&format!("export {}\n", shell_quote(kv)));
        }
        for m in &mounts {
            s.push_str(m);
            s.push('\n');
        }
        // init chdirs before our mounts exist, so change into the workdir here.
        s.push_str(&format!("cd {}\nset +e\nexec", shell_quote(&workdir)));
        for a in &argv {
            s.push(' ');
            s.push_str(&shell_quote(a));
        }
        s.push('\n');
        let dir = rootfs.join(".agentbox");
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("entry.sh"), s)?;
        let boot_env: Vec<String> = env.iter().filter(|e| e.starts_with("PATH=")).cloned().collect();
        ("/bin/sh".to_string(), vec!["/.agentbox/entry.sh".to_string()], boot_env)
    } else {
        if !mounts.is_empty() {
            bail!("volumes need /bin/sh in the image, and {} has none", image.reference);
        }
        if let Some(bad) = argv.iter().chain(env.iter()).find(|a| !cmdline_safe(a)) {
            bail!(
                "{} has no /bin/sh, so arguments must be printable ASCII without double quotes; can't pass {bad:?}",
                image.reference
            );
        }
        (argv[0].clone(), argv[1..].to_vec(), env)
    };

    for p in &o.ports {
        let ok = p.split_once(':').map(|(a, b)| a.parse::<u16>().is_ok() && b.parse::<u16>().is_ok()).unwrap_or(false);
        if !ok {
            bail!("bad port mapping {p:?}; expected host_port:guest_port");
        }
    }

    Ok(Prepared {
        spec: VmSpec {
            root: rootfs.to_string_lossy().into_owned(),
            exec,
            args,
            env,
            workdir,
            cpus: o.cpus.unwrap_or(2),
            memory_mib: o.memory_mib.unwrap_or(1024),
            ports: o.ports.clone(),
            virtiofs,
            network: o.network.unwrap_or(true),
        },
        cleanup,
    })
}

fn cmdline_safe(s: &str) -> bool {
    s.bytes().all(|b| (0x20..0x7f).contains(&b) && b != b'"')
}

/// Does the image have a POSIX shell? Checked without following symlinks out of the
/// rootfs (an absolute `/bin -> /usr/bin` link would otherwise resolve on the host).
fn has_shell(rootfs: &std::path::Path) -> bool {
    let present = |p: &str| fs::symlink_metadata(rootfs.join(p)).is_ok();
    let bin_is_dir = fs::symlink_metadata(rootfs.join("bin")).map(|m| m.is_dir()).unwrap_or(false);
    present("usr/bin/sh") || (bin_is_dir && present("bin/sh"))
}

/// Like Docker, (re)write /etc/resolv.conf and /etc/hosts on every start. Guest sockets
/// are proxied through the host (libkrun TSI), so the host's nameservers are reachable;
/// loopback resolvers are skipped because they'd point at the guest itself.
fn write_network_files(rootfs: &std::path::Path) -> Result<()> {
    let etc = rootfs.join("etc");
    fs::create_dir_all(&etc)?;
    let mut servers: Vec<String> = fs::read_to_string("/etc/resolv.conf")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver").map(|s| s.trim().to_string()))
        .filter(|s| !s.starts_with("127.") && s != "::1" && !s.is_empty())
        .collect();
    if servers.is_empty() {
        servers = vec!["1.1.1.1".into(), "8.8.8.8".into()];
    }
    let resolv: String = servers.iter().map(|s| format!("nameserver {s}\n")).collect();
    for (name, body) in [("resolv.conf", resolv), ("hosts", "127.0.0.1 localhost\n::1 localhost\n".to_string())] {
        let p = etc.join(name);
        if fs::symlink_metadata(&p).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            fs::remove_file(&p)?;
        }
        if name == "hosts" && p.exists() {
            continue; // keep an image-provided hosts file
        }
        fs::write(&p, body)?;
    }
    Ok(())
}

fn vm_command(spec: &VmSpec) -> Result<Command> {
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("__vm").env(SPEC_ENV, serde_json::to_string(spec)?);
    Ok(cmd)
}

fn finish(p: &Prepared) {
    if let Some(d) = &p.cleanup {
        let _ = remove_tree(d);
    }
}

/// Run attached to the terminal (CLI use). Returns the guest's exit code.
pub fn run_interactive(store: &Store, o: &RunOpts) -> Result<i32> {
    let p = prepare(store, o)?;
    let status = vm_command(&p.spec)?.status();
    finish(&p);
    let status = status?;
    Ok(status.code().unwrap_or(128 + status_signal(&status)))
}

#[cfg(unix)]
fn status_signal(s: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    s.signal().unwrap_or(0)
}

/// Run with output captured and a wall-clock timeout (MCP / scripted use).
pub fn run_captured(store: &Store, o: &RunOpts, timeout: Duration) -> Result<RunResult> {
    let p = prepare(store, o)?;
    let start = Instant::now();
    let result = (|| {
        let mut child = vm_command(&p.spec)?
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let read = |r: Option<Box<dyn Read + Send>>| {
            std::thread::spawn(move || {
                let mut buf = vec![];
                if let Some(r) = r {
                    let _ = r.take(MAX_CAPTURE as u64).read_to_end(&mut buf);
                }
                String::from_utf8_lossy(&buf).into_owned()
            })
        };
        let out = read(child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>));
        let err = read(child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>));
        let mut timed_out = false;
        let status = loop {
            if let Some(s) = child.try_wait()? {
                break Some(s);
            }
            if start.elapsed() > timeout {
                timed_out = true;
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        Ok(RunResult {
            exit_code: status.and_then(|s| s.code()),
            timed_out,
            duration_ms: start.elapsed().as_millis(),
            // libkrun's console carries the guest's stdout and stderr together.
            output: out.join().unwrap_or_default().replace("\r\n", "\n"),
            stderr: err.join().unwrap_or_default(),
        })
    })();
    finish(&p);
    result
}

/// Pull (if needed) and create a named, persistent container without running anything.
pub fn create(store: &Store, image: &str, name: &str) -> Result<()> {
    if store.container_dir(name).exists() {
        bail!("container {name} already exists");
    }
    let o = RunOpts { image: image.into(), name: Some(name.into()), command: vec!["true".into()], ..Default::default() };
    prepare(store, &o).map(|_| ())
}

// ---------------------------------------------------------------------------
// The `__vm` child: configure libkrun and hand the process over to the guest.

extern "C" {
    fn krun_set_log_level(level: u32) -> i32;
    fn krun_create_ctx() -> i32;
    fn krun_set_vm_config(ctx: u32, cpus: u8, ram_mib: u32) -> i32;
    fn krun_set_root(ctx: u32, root: *const c_char) -> i32;
    fn krun_add_virtiofs(ctx: u32, tag: *const c_char, path: *const c_char) -> i32;
    fn krun_set_port_map(ctx: u32, ports: *const *const c_char) -> i32;
    fn krun_set_workdir(ctx: u32, dir: *const c_char) -> i32;
    fn krun_set_exec(ctx: u32, exec: *const c_char, argv: *const *const c_char, envp: *const *const c_char) -> i32;
    fn krun_disable_implicit_vsock(ctx: u32) -> i32;
    fn krun_add_vsock(ctx: u32, tsi_features: u32) -> i32;
    fn krun_start_enter(ctx: u32) -> i32;
}

fn cstr(s: &str) -> Result<CString> {
    CString::new(s).map_err(|_| anyhow!("string contains NUL: {s:?}"))
}

fn cvec(v: &[String]) -> Result<(Vec<CString>, Vec<*const c_char>)> {
    let owned: Vec<CString> = v.iter().map(|s| cstr(s)).collect::<Result<_>>()?;
    let mut ptrs: Vec<*const c_char> = owned.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    Ok((owned, ptrs))
}

fn check(what: &str, rc: i32) -> Result<()> {
    if rc < 0 {
        bail!("{what} failed: {}", std::io::Error::from_raw_os_error(-rc));
    }
    Ok(())
}

pub fn vm_child() -> Result<()> {
    let spec: VmSpec = serde_json::from_str(&std::env::var(SPEC_ENV).context("missing VM spec")?)?;
    let root = cstr(&spec.root)?;
    let exec = cstr(&spec.exec)?;
    let workdir = cstr(&spec.workdir)?;
    let (_a, argv) = cvec(&spec.args)?;
    let (_e, envp) = cvec(&spec.env)?;
    let (_p, ports) = cvec(&spec.ports)?;
    let fs: Vec<(CString, CString)> =
        spec.virtiofs.iter().map(|(t, p)| Ok((cstr(t)?, cstr(p)?))).collect::<Result<_>>()?;

    unsafe {
        let level = std::env::var("AGENTBOX_KRUN_LOG").ok().and_then(|l| l.parse().ok()).unwrap_or(0);
        krun_set_log_level(level);
        let ctx = krun_create_ctx();
        check("krun_create_ctx", ctx)?;
        let ctx = ctx as u32;
        check("krun_set_vm_config", krun_set_vm_config(ctx, spec.cpus, spec.memory_mib))?;
        check("krun_set_root", krun_set_root(ctx, root.as_ptr()))?;
        for (tag, path) in &fs {
            check("krun_add_virtiofs", krun_add_virtiofs(ctx, tag.as_ptr(), path.as_ptr()))?;
        }
        if !spec.network {
            // Keep a vsock device but without TSI socket hijacking: the guest gets no network.
            check("krun_disable_implicit_vsock", krun_disable_implicit_vsock(ctx))?;
            check("krun_add_vsock", krun_add_vsock(ctx, 0))?;
            if !spec.ports.is_empty() {
                bail!("port mappings need networking");
            }
        }
        if !spec.ports.is_empty() {
            check("krun_set_port_map", krun_set_port_map(ctx, ports.as_ptr()))?;
        }
        check("krun_set_workdir", krun_set_workdir(ctx, workdir.as_ptr()))?;
        check("krun_set_exec", krun_set_exec(ctx, exec.as_ptr(), argv.as_ptr(), envp.as_ptr()))?;
        // Only returns on error.
        check("krun_start_enter", krun_start_enter(ctx))?;
    }
    bail!("krun_start_enter returned unexpectedly")
}
