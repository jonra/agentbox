//! Turning a container into a libkrun microVM.
//!
//! The VM's root filesystem is the container's rootfs directory (a copy-on-write clone
//! of the image). `krun_start_enter` takes over the calling process and `exit()`s with
//! the guest's exit code, so the VM always runs in a child process (`agentbox __vm`);
//! the parent (CLI, supervisor or MCP server) tracks its state and output.

use crate::container::Container;
use crate::store::{ImageMeta, Store};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::ffi::{c_char, CString};
use std::fs;
use std::path::Path;
use std::process::Command;

const SPEC_ENV: &str = "AGENTBOX_VM_SPEC";

/// A container's configuration, as given to `run`/`create` (CLI flags or MCP arguments).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunOpts {
    pub image: String,
    #[serde(default)]
    pub command: Vec<String>,
    /// Some(vec![]) clears the image entrypoint.
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    /// KEY=VALUE, or KEY to copy it from the host environment (resolved at create time).
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub workdir: Option<String>,
    /// host_path:guest_path[:ro], or volume_name:guest_path[:ro] for a named volume.
    #[serde(default)]
    pub volumes: Vec<String>,
    /// host_port:guest_port (also accepts ip:host:guest and a /tcp suffix).
    #[serde(default)]
    pub ports: Vec<String>,
    #[serde(default)]
    pub cpus: Option<u8>,
    #[serde(default)]
    pub memory_mib: Option<u32>,
    #[serde(default)]
    pub name: Option<String>,
    /// false = no network at all (default true: outbound via the host).
    #[serde(default)]
    pub network: Option<bool>,
    /// Delete the container when it exits.
    #[serde(default)]
    pub rm: bool,
    /// Extra /etc/hosts entries, "name:ip" (`--add-host`; compose service names).
    #[serde(default)]
    pub extra_hosts: Vec<String>,
    /// In-guest TCP forwarders "listen_ip:port=target_ip:port" (compose service names).
    #[serde(default)]
    pub forwards: Vec<String>,
    /// Free-form labels (compose project/service bookkeeping).
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
}

/// Everything the `__vm` child needs; passed to it as JSON in an env var.
#[derive(Debug, Serialize, Deserialize)]
pub struct VmSpec {
    root: String,
    exec: String,
    args: Vec<String>,
    env: Vec<String>,
    workdir: String,
    cpus: u8,
    memory_mib: u32,
    /// None: every port the guest listens on is exposed on the host at the same number.
    /// Some(list): only these "host:guest" mappings (empty = nothing exposed).
    ports: Option<Vec<String>>,
    virtiofs: Vec<(String, String)>,
    network: bool,
}

pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The environment a container's processes see: image ENV overlaid with the container's.
pub fn effective_env(image: &ImageMeta, config: &RunOpts) -> Vec<String> {
    let mut env: Vec<String> = image.env.clone();
    for kv in &config.env {
        let k = kv.split('=').next().unwrap_or_default();
        env.retain(|x| x.split('=').next() != Some(k));
        env.push(kv.clone());
    }
    for (k, v) in [("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"), ("HOME", "/root")] {
        if !env.iter().any(|x| x.starts_with(&format!("{k}="))) {
            env.push(format!("{k}={v}"));
        }
    }
    env
}

pub fn effective_workdir(image: &ImageMeta, config: &RunOpts) -> String {
    config.workdir.clone().filter(|w| !w.is_empty()).unwrap_or_else(|| {
        if image.workdir.is_empty() { "/".into() } else { image.workdir.clone() }
    })
}

/// Guest-side exec helper: watches /.agentbox/exec for `<id>.cmd` scripts the host drops
/// in (the rootfs is shared over virtio-fs), runs each in the background and leaves
/// `<id>.out`, `<id>.err` and finally `<id>.rc` for the host to collect.
const EXECD: &str = r#"__agentbox_execd() {
  set +e
  d=/.agentbox/exec
  while :; do
    for r in "$d"/*.cmd; do
      [ -e "$r" ] || continue
      j=${r%.cmd}
      mv "$r" "$j.run" 2>/dev/null || continue
      ( sh "$j.run" >"$j.out" 2>"$j.err" </dev/null; echo $? >"$j.rc.tmp"; mv "$j.rc.tmp" "$j.rc" ) &
    done
    sleep 0.2 2>/dev/null || sleep 1
  done
}
( __agentbox_execd ) >/dev/null 2>&1 </dev/null &
"#;

/// The static in-guest helper (guest/), when this build embeds one.
#[cfg(guest_helper)]
pub const GUEST_HELPER: Option<&[u8]> = Some(include_bytes!(env!("AGENTBOX_GUEST_HELPER")));
#[cfg(not(guest_helper))]
pub const GUEST_HELPER: Option<&[u8]> = None;

/// What to boot: a rootfs plus the process to run in it.
pub struct GuestRun<'a> {
    pub rootfs: &'a Path,
    /// For error messages.
    pub what: &'a str,
    pub argv: Vec<String>,
    pub env: Vec<String>,
    pub workdir: String,
    /// Normalized "abs_host:guest[:ro]".
    pub volumes: Vec<String>,
    pub ports: Option<Vec<String>>,
    pub extra_hosts: Vec<String>,
    pub forwards: Vec<String>,
    pub cpus: u8,
    pub memory_mib: u32,
    pub network: bool,
    /// Start the guest exec helper (containers; not build steps).
    pub execd: bool,
}

/// Build the VM configuration for starting `c`, writing the per-start files
/// (resolv.conf, hosts, entry.sh) into its rootfs.
pub fn build_spec(store: &Store, c: &Container) -> Result<VmSpec> {
    let image = store.image(&c.image)?.ok_or_else(|| anyhow!("image {} of container {} is gone", c.image, c.name))?;
    let o = &c.config;
    let rootfs = store.container_dir(&c.id).join("rootfs");
    let mut argv = o.entrypoint.clone().unwrap_or_else(|| image.entrypoint.clone());
    argv.extend(if o.command.is_empty() { image.cmd.clone() } else { o.command.clone() });
    if argv.is_empty() {
        bail!("no command given and image {} has no default command", image.reference);
    }
    let network = o.network.unwrap_or(true);
    if !network && !o.ports.is_empty() {
        bail!("port mappings need networking");
    }
    prepare(&GuestRun {
        rootfs: &rootfs,
        what: &image.reference,
        argv,
        env: effective_env(&image, o),
        workdir: effective_workdir(&image, o),
        volumes: o.volumes.clone(),
        // Without a port map libkrun exposes every port the guest listens on, at its own
        // number (this libkrun also treats an empty map that way).
        ports: if o.ports.is_empty() { None } else { Some(o.ports.clone()) },
        extra_hosts: o.extra_hosts.clone(),
        forwards: o.forwards.clone(),
        cpus: o.cpus.unwrap_or(2),
        memory_mib: o.memory_mib.unwrap_or(1024),
        network,
        execd: true,
    })
}

pub fn prepare(g: &GuestRun) -> Result<VmSpec> {
    let rootfs = g.rootfs;
    write_network_files(rootfs, &g.extra_hosts)?;
    let (argv, env, workdir) = (&g.argv, &g.env, &g.workdir);

    // Volumes are virtio-fs shares that the guest has to mount itself.
    let mut virtiofs = vec![];
    let mut mounts = vec![];
    for (i, v) in g.volumes.iter().enumerate() {
        let (host, guest, ro) = split_volume(v)?;
        let tag = format!("vol{i}");
        let o = if ro { "-o ro " } else { "" };
        let hp = Path::new(host);
        if hp.is_file() {
            // virtio-fs shares directories: share the file's directory at a hidden path
            // and symlink the file into place.
            let dir = hp.parent().ok_or_else(|| anyhow!("bad volume source {host}"))?;
            let name = hp.file_name().ok_or_else(|| anyhow!("bad volume source {host}"))?.to_string_lossy();
            let at = format!("/.agentbox/mnt/{tag}");
            let parent = Path::new(guest).parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| "/".into());
            mounts.push(format!(
                "mkdir -p {at} {p} && mount -t virtiofs {o}{tag} {at} && rm -rf {g} && ln -s {f} {g}",
                p = shell_quote(&parent),
                g = shell_quote(guest),
                f = shell_quote(&format!("{at}/{name}")),
            ));
            virtiofs.push((tag, dir.to_string_lossy().into_owned()));
        } else {
            mounts.push(format!("mkdir -p {g} && mount -t virtiofs {o}{tag} {g}", g = shell_quote(guest)));
            virtiofs.push((tag, host.to_string()));
        }
    }

    // libkrun hands argv/env to the guest on the kernel command line, which only carries
    // printable ASCII and mangles embedded quotes (tabs/newlines even panic the VMM). So
    // when the image has a shell, put the real argv/env in a launcher script and boot
    // `/bin/sh /.agentbox/entry.sh` instead. The script also starts the exec helper.
    let (exec, args, env) = if let Some(shell) = find_shell(rootfs) {
        let mut s = String::from("#!/bin/sh\n# written by agentbox on each start\nset -e\n");
        for kv in env {
            s.push_str(&format!("export {}\n", shell_quote(kv)));
        }
        for m in &mounts {
            s.push_str(m);
            s.push('\n');
        }
        if !g.forwards.is_empty() {
            let bin = GUEST_HELPER.ok_or_else(|| anyhow!("this agentbox build has no guest helper (run `make guest`)"))?;
            let p = rootfs.join(".agentbox/bin/agentbox-guest");
            fs::create_dir_all(p.parent().unwrap())?;
            if p.exists() {
                fs::remove_file(&p)?;
            }
            fs::write(&p, bin)?;
            fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
            s.push_str("/.agentbox/bin/agentbox-guest fwd");
            for f in &g.forwards {
                s.push(' ');
                s.push_str(&shell_quote(f));
            }
            s.push_str(" >/dev/null 2>&1 </dev/null &\n");
        }
        if g.execd {
            s.push_str(EXECD);
        }
        // init chdirs before our mounts exist, so change into the workdir here.
        s.push_str(&format!("mkdir -p {w} 2>/dev/null; cd {w}\nset +e\nexec", w = shell_quote(workdir)));
        for a in argv {
            s.push(' ');
            s.push_str(&shell_quote(a));
        }
        s.push('\n');
        let dir = rootfs.join(".agentbox");
        let exec_dir = dir.join("exec");
        if exec_dir.exists() {
            fs::remove_dir_all(&exec_dir)?; // stale requests from a previous start
        }
        fs::create_dir_all(&exec_dir)?;
        fs::write(dir.join("entry.sh"), s)?;
        let boot_env: Vec<String> = env.iter().filter(|e| e.starts_with("PATH=")).cloned().collect();
        (shell.to_string(), vec!["/.agentbox/entry.sh".to_string()], boot_env)
    } else {
        if !g.forwards.is_empty() {
            bail!("{} has no shell (/bin/sh), so it can't run the service-name forwarders", g.what);
        }
        if !mounts.is_empty() {
            bail!("volumes need a shell (/bin/sh or /busybox/sh) in the image, and {} has none", g.what);
        }
        if let Some(bad) = argv.iter().chain(env.iter()).find(|a| !cmdline_safe(a)) {
            bail!("{} has no /bin/sh, so arguments must be printable ASCII without double quotes; can't pass {bad:?}", g.what);
        }
        (argv[0].clone(), argv[1..].to_vec(), env.clone())
    };

    Ok(VmSpec {
        root: rootfs.to_string_lossy().into_owned(),
        exec,
        args,
        env,
        workdir: workdir.clone(),
        cpus: g.cpus,
        memory_mib: g.memory_mib,
        ports: g.ports.clone(),
        virtiofs,
        network: g.network,
    })
}

/// "host:guest[:ro]" (already normalized at create time) -> parts.
pub fn split_volume(v: &str) -> Result<(&str, &str, bool)> {
    let parts: Vec<&str> = v.split(':').collect();
    match parts.as_slice() {
        [h, g] => Ok((h, g, false)),
        [h, g, "ro"] => Ok((h, g, true)),
        [h, g, "rw"] => Ok((h, g, false)),
        _ => bail!("bad volume {v:?}; expected host_path:guest_path[:ro]"),
    }
}

fn cmdline_safe(s: &str) -> bool {
    s.bytes().all(|b| (0x20..0x7f).contains(&b) && b != b'"')
}

/// The image's POSIX shell, if any (guest path). Checked without following symlinks out
/// of the rootfs (an absolute `/bin -> /usr/bin` link would otherwise resolve on the host).
/// `/busybox/sh` covers distroless `:debug` images.
pub fn find_shell(rootfs: &Path) -> Option<&'static str> {
    let present = |p: &str| fs::symlink_metadata(rootfs.join(p)).is_ok();
    let is_dir = |p: &str| fs::symlink_metadata(rootfs.join(p)).map(|m| m.is_dir()).unwrap_or(false);
    if present("usr/bin/sh") {
        Some("/usr/bin/sh")
    } else if is_dir("bin") && present("bin/sh") {
        Some("/bin/sh")
    } else if is_dir("busybox") && present("busybox/sh") {
        Some("/busybox/sh")
    } else {
        None
    }
}

pub fn has_shell(rootfs: &Path) -> bool {
    find_shell(rootfs).is_some()
}

/// Like Docker, (re)write /etc/resolv.conf and /etc/hosts on every start. Guest sockets
/// are proxied through the host (libkrun TSI), so the host's nameservers are reachable;
/// loopback resolvers are skipped because they'd point at the guest itself. A guest
/// connecting to 127.0.0.1 reaches the host's loopback, so host.docker.internal maps there.
fn write_network_files(rootfs: &Path, extra_hosts: &[String]) -> Result<()> {
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
    // Rebuilt on every start from the image's own file (saved on first start), so entries
    // from an earlier configuration don't pile up.
    let hosts_path = etc.join("hosts");
    let orig_path = rootfs.join(".agentbox/hosts.image");
    if !orig_path.exists() {
        let orig = if fs::symlink_metadata(&hosts_path).map(|m| m.is_file()).unwrap_or(false) {
            fs::read_to_string(&hosts_path).unwrap_or_default()
        } else {
            String::new()
        };
        fs::create_dir_all(rootfs.join(".agentbox"))?;
        fs::write(&orig_path, orig)?;
    }
    let mut hosts = fs::read_to_string(&orig_path).unwrap_or_default();
    if !hosts.ends_with('\n') && !hosts.is_empty() {
        hosts.push('\n');
    }
    let has_name = |h: &str, name: &str| {
        h.lines().any(|l| !l.trim_start().starts_with('#') && l.split_whitespace().skip(1).any(|n| n == name))
    };
    // Images often ship an empty /etc/hosts (Docker writes one at runtime).
    if !has_name(&hosts, "localhost") {
        hosts.push_str("127.0.0.1 localhost\n::1 localhost ip6-localhost ip6-loopback\n");
    }
    if !has_name(&hosts, "host.docker.internal") {
        hosts.push_str("127.0.0.1 host.docker.internal host.agentbox.internal\n");
    }
    for h in extra_hosts {
        if let Some((name, ip)) = h.split_once(':').or_else(|| h.split_once('=')) {
            let ip = if ip == "host-gateway" { "127.0.0.1" } else { ip };
            hosts.push_str(&format!("{ip} {name}\n"));
        }
    }
    for (name, body) in [("resolv.conf", resolv), ("hosts", hosts)] {
        let p = etc.join(name);
        if fs::symlink_metadata(&p).map(|m| m.file_type().is_symlink() || m.is_dir()).unwrap_or(false) {
            let _ = fs::remove_file(&p);
        }
        fs::write(&p, body)?;
    }
    Ok(())
}

pub fn command(spec: &VmSpec) -> Result<Command> {
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("__vm").env(SPEC_ENV, serde_json::to_string(spec)?);
    Ok(cmd)
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
    // macOS has no PR_SET_PDEATHSIG: if whoever started us (CLI, supervisor, MCP server)
    // dies, take the VM down too instead of leaving an orphan holding ports.
    let parent = unsafe { libc::getppid() };
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if unsafe { libc::getppid() } != parent {
            unsafe { libc::_exit(137) };
        }
    });
    // virtio-fs keeps a host file descriptor per open guest file, and macOS's default
    // soft limit (256) is far too low for e.g. Python importing a large venv.
    unsafe {
        let mut r = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) == 0 {
            for want in [r.rlim_max, 1 << 20, 65536, 24576, 10240] {
                let cur = want.min(r.rlim_max);
                if cur <= r.rlim_cur {
                    continue;
                }
                if libc::setrlimit(libc::RLIMIT_NOFILE, &libc::rlimit { rlim_cur: cur, rlim_max: r.rlim_max }) == 0 {
                    break;
                }
            }
        }
    }
    let spec: VmSpec = serde_json::from_str(&std::env::var(SPEC_ENV).context("missing VM spec")?)?;
    let root = cstr(&spec.root)?;
    let exec = cstr(&spec.exec)?;
    let workdir = cstr(&spec.workdir)?;
    let (_a, argv) = cvec(&spec.args)?;
    let (_e, envp) = cvec(&spec.env)?;
    let (_p, ports) = cvec(spec.ports.as_deref().unwrap_or(&[]))?;
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
        }
        if spec.ports.is_some() {
            check("krun_set_port_map", krun_set_port_map(ctx, ports.as_ptr()))?;
        }
        check("krun_set_workdir", krun_set_workdir(ctx, workdir.as_ptr()))?;
        check("krun_set_exec", krun_set_exec(ctx, exec.as_ptr(), argv.as_ptr(), envp.as_ptr()))?;
        // Only returns on error.
        check("krun_start_enter", krun_start_enter(ctx))?;
    }
    bail!("krun_start_enter returned unexpectedly")
}
