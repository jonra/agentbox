//! Container lifecycle, daemonless (like podman): each container is a directory
//!
//! ```text
//! containers/<id>/container.json   id, name, image, config (RunOpts)
//!                 state.json       created | running (pid) | exited (exit code)
//!                 log              output of detached runs
//!                 rootfs/          copy-on-write clone of the image
//! ```
//!
//! A detached container is owned by an `agentbox __supervise <id>` process that runs
//! the VM, appends its output to `log` and records the exit code.

use crate::registry;
use crate::store::{clone_tree, remove_tree, Store};
use crate::vm::{self, shell_quote, RunOpts};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_CAPTURE: usize = 1 << 20;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub created: u64,
    pub config: RunOpts,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct State {
    pub status: String,
    pub pid: Option<i32>,
    pub exit_code: Option<i32>,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
}

impl State {
    pub fn running(&self) -> bool {
        self.status == "running"
    }
}

#[derive(Debug, Serialize)]
pub struct RunResult {
    pub container: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u128,
    pub stdout: String,
    pub stderr: String,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    if File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)).is_err() {
        let seed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() ^ std::process::id() as u128;
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (seed >> ((i % 16) * 8)) as u8 ^ i as u8;
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_name(taken: &[String]) -> String {
    const A: &[&str] = &["brave", "calm", "eager", "fervent", "gentle", "happy", "jolly", "keen", "lucid", "merry",
        "nimble", "quirky", "sharp", "swift", "tender", "vivid", "witty", "zealous", "bold", "clever"];
    const B: &[&str] = &["hopper", "turing", "lovelace", "curie", "knuth", "ritchie", "hamilton", "noether", "shannon",
        "dijkstra", "liskov", "torvalds", "babbage", "goodall", "franklin", "tesla", "bohr", "fermi", "darwin", "euler"];
    loop {
        let h = random_hex(2);
        let n = u16::from_str_radix(&h, 16).unwrap_or(0) as usize;
        let name = format!("{}_{}", A[n % A.len()], B[(n / A.len()) % B.len()]);
        if !taken.contains(&name) {
            return name;
        }
        if taken.len() > 300 {
            return format!("{name}_{}", random_hex(2));
        }
    }
}

fn alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) == 0 }
}

pub fn exit_code_of(s: ExitStatus) -> i32 {
    s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0))
}

impl Container {
    pub fn dir(&self, store: &Store) -> PathBuf {
        store.container_dir(&self.id)
    }
    pub fn rootfs(&self, store: &Store) -> PathBuf {
        self.dir(store).join("rootfs")
    }
    pub fn short_id(&self) -> &str {
        &self.id[..12.min(self.id.len())]
    }
    pub fn log_path(&self, store: &Store) -> PathBuf {
        self.dir(store).join("log")
    }

    /// Current state; a "running" record whose VM process is gone reads as exited.
    pub fn state(&self, store: &Store) -> State {
        let mut st: State = fs::read(self.dir(store).join("state.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_else(|| State { status: "created".into(), ..Default::default() });
        if st.running() && !st.pid.map(alive).unwrap_or(false) {
            st.status = "exited".into();
        }
        st
    }

    fn set_state(&self, store: &Store, st: &State) {
        let dir = self.dir(store);
        if !dir.exists() {
            return; // removed meanwhile (rm -f)
        }
        let tmp = dir.join("state.json.tmp");
        if fs::write(&tmp, serde_json::to_vec_pretty(st).unwrap_or_default()).is_ok() {
            let _ = fs::rename(&tmp, dir.join("state.json"));
        }
    }

    fn mark_running(&self, store: &Store, pid: i32) {
        self.set_state(store, &State { status: "running".into(), pid: Some(pid), started_at: Some(now()), ..Default::default() });
    }

    fn mark_exited(&self, store: &Store, code: Option<i32>) {
        let mut st = self.state(store);
        st.status = "exited".into();
        st.exit_code = code;
        st.finished_at = Some(now());
        self.set_state(store, &st);
        if self.config.rm {
            let _ = remove_tree(&self.dir(store));
        }
    }

    pub fn image_meta(&self, store: &Store) -> Result<crate::store::ImageMeta> {
        store.image(&self.image)?.ok_or_else(|| anyhow!("image {} of container {} is gone", self.image, self.name))
    }
}

pub fn list(store: &Store) -> Result<Vec<Container>> {
    let mut out = vec![];
    for e in fs::read_dir(store.data.join("containers"))? {
        let p = e?.path().join("container.json");
        if let Some(c) = fs::read(&p).ok().and_then(|b| serde_json::from_slice::<Container>(&b).ok()) {
            out.push(c);
        }
    }
    out.sort_by(|a, b| b.created.cmp(&a.created).then(a.name.cmp(&b.name)));
    Ok(out)
}

/// Look a container up by name, full id or unique id prefix.
pub fn find(store: &Store, key: &str) -> Result<Container> {
    let all = list(store)?;
    if let Some(c) = all.iter().find(|c| c.name == key || c.id == key) {
        return Ok(c.clone());
    }
    let matches: Vec<&Container> = all.iter().filter(|c| !key.is_empty() && c.id.starts_with(key)).collect();
    match matches.as_slice() {
        [c] => Ok((*c).clone()),
        [] => bail!("no such container: {key}"),
        _ => bail!("container id prefix {key} is ambiguous"),
    }
}

/// Docker-style port spec -> "host:guest".
fn normalize_port(p: &str) -> Result<String> {
    let (spec, proto) = p.split_once('/').unwrap_or((p, "tcp"));
    if proto != "tcp" {
        bail!("only tcp ports are supported ({p})");
    }
    let parts: Vec<&str> = spec.split(':').collect();
    let (host, guest) = match parts.as_slice() {
        [g] => (*g, *g),
        [h, g] => (*h, *g),
        [_ip, h, g] => (*h, *g),
        _ => bail!("bad port mapping {p:?}"),
    };
    let ok = |s: &str| s.parse::<u16>().map(|n| n > 0).unwrap_or(false);
    if !ok(host) || !ok(guest) {
        bail!("bad port mapping {p:?}; expected [ip:]host_port:guest_port");
    }
    Ok(format!("{host}:{guest}"))
}

/// Docker-style volume spec -> "abs_host_path:guest[:ro]"; names become named volumes.
fn normalize_volume(store: &Store, v: &str) -> Result<String> {
    let parts: Vec<&str> = v.split(':').collect();
    let (src, guest, mode) = match parts.as_slice() {
        [s, g] => (*s, *g, None),
        [s, g, m] if *m == "ro" || *m == "rw" => (*s, *g, Some(*m)),
        _ => bail!("bad volume {v:?}; expected host_path|volume_name:guest_path[:ro]"),
    };
    if !guest.starts_with('/') {
        bail!("volume guest path must be absolute: {guest}");
    }
    let host: PathBuf = if let Some(rest) = src.strip_prefix("~/") {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest)
    } else if src.starts_with('/') || src.starts_with('.') {
        PathBuf::from(src)
    } else {
        let d = store.volume_dir(src)?;
        fs::create_dir_all(&d)?;
        d
    };
    let host = fs::canonicalize(&host).with_context(|| format!("volume host path {}", host.display()))?;
    let host = host.to_string_lossy();
    if host.contains(':') {
        bail!("volume host paths can't contain ':' ({host})");
    }
    Ok(match mode {
        Some("ro") => format!("{host}:{guest}:ro"),
        _ => format!("{host}:{guest}"),
    })
}

/// Create a container (pulling the image if needed) without starting it.
pub fn create(store: &Store, opts: &RunOpts) -> Result<Container> {
    let reference = registry::Reference::parse(&opts.image)?.canonical();
    let image = match store.image(&reference)? {
        Some(i) => i,
        None => registry::pull(store, &opts.image, false)?,
    };
    let existing = list(store)?;
    let names: Vec<String> = existing.iter().map(|c| c.name.clone()).collect();
    let name = match &opts.name {
        Some(n) if !n.is_empty() => {
            if !n.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c)) {
                bail!("invalid container name {n:?} (use letters, digits, _ . -)");
            }
            if let Some(c) = existing.iter().find(|c| &c.name == n) {
                bail!("the container name {n:?} is already in use by container {}", c.short_id());
            }
            n.clone()
        }
        _ => random_name(&names),
    };

    let mut config = opts.clone();
    config.name = Some(name.clone());
    config.image = image.reference.clone();
    config.env = opts
        .env
        .iter()
        .map(|e| if e.contains('=') { e.clone() } else { format!("{e}={}", std::env::var(e).unwrap_or_default()) })
        .collect();
    config.ports = opts.ports.iter().map(|p| normalize_port(p)).collect::<Result<_>>()?;
    config.volumes = opts.volumes.iter().map(|v| normalize_volume(store, v)).collect::<Result<_>>()?;
    if config.network == Some(false) && !config.ports.is_empty() {
        bail!("port mappings need networking");
    }

    let c = Container { id: random_hex(32), name, image: image.reference.clone(), created: now(), config };
    let dir = c.dir(store);
    let result = (|| {
        clone_tree(&store.image_dir(&image.reference).join("rootfs"), &dir.join("rootfs"))?;
        fs::write(dir.join("container.json"), serde_json::to_vec_pretty(&c)?)?;
        c.set_state(store, &State { status: "created".into(), ..Default::default() });
        // Validate now so a bad config fails at create, not later in a detached supervisor.
        vm::build_spec(store, &c)?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = remove_tree(&dir);
        return Err(e);
    }
    Ok(c)
}

static ATTACHED_CHILD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Forward Ctrl-C / SIGTERM to the VM process, so `agentbox run` behaves like `docker run`
/// and records the exit code instead of dying and orphaning the VM.
extern "C" fn forward_signal(sig: libc::c_int) {
    let pid = ATTACHED_CHILD.load(std::sync::atomic::Ordering::SeqCst);
    if pid > 0 {
        unsafe { libc::kill(pid, sig) };
    }
}

/// Fail like Docker ("port is already allocated") instead of letting the VMM share or
/// silently skip a host port that is in use.
fn check_ports(c: &Container) -> Result<()> {
    for p in &c.config.ports {
        let host: u16 = p.split(':').next().and_then(|h| h.parse().ok()).unwrap_or(0);
        if std::net::TcpListener::bind(("0.0.0.0", host)).is_err() {
            bail!("port {host} is already allocated (container {})", c.name);
        }
    }
    Ok(())
}

/// Start attached to this terminal; returns the container's exit code.
pub fn start_attached(store: &Store, c: &Container) -> Result<i32> {
    if c.state(store).running() {
        bail!("container {} is already running", c.name);
    }
    check_ports(c)?;
    let spec = vm::build_spec(store, c)?;
    // A handler (unlike SIG_IGN) is reset on exec, so the VM process keeps default handling.
    unsafe {
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::signal(sig, forward_signal as *const () as libc::sighandler_t);
        }
    }
    let mut child = vm::command(&spec)?.spawn()?;
    ATTACHED_CHILD.store(child.id() as i32, std::sync::atomic::Ordering::SeqCst);
    c.mark_running(store, child.id() as i32);
    let status = child.wait()?;
    let code = exit_code_of(status);
    c.mark_exited(store, Some(code));
    Ok(code)
}

/// Start in the background under a supervisor process; returns once the VM is up.
pub fn start_detached(store: &Store, c: &Container) -> Result<()> {
    if c.state(store).running() {
        bail!("container {} is already running", c.name);
    }
    check_ports(c)?;
    vm::build_spec(store, c)?; // surface config errors here rather than in the supervisor
    let before = c.state(store).started_at;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("__supervise").arg(&c.id).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut sup = cmd.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let st = c.state(store);
        if st.started_at != before && st.started_at.is_some() {
            return Ok(());
        }
        if let Some(s) = sup.try_wait()? {
            bail!("supervisor exited early ({s}); see `agentbox logs {}`", c.name);
        }
        if Instant::now() > deadline {
            bail!("container {} did not start within 10s", c.name);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Body of `agentbox __supervise <id>`.
pub fn supervise(store: &Store, id: &str) -> Result<()> {
    let c = find(store, id)?;
    let log = OpenOptions::new().create(true).append(true).open(c.log_path(store))?;
    let spec = vm::build_spec(store, &c)?;
    let mut child = vm::command(&spec)?.stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log).spawn()?;
    c.mark_running(store, child.id() as i32);
    let status = child.wait()?;
    c.mark_exited(store, Some(exit_code_of(status)));
    Ok(())
}

pub fn signal(store: &Store, c: &Container, sig: i32) -> Result<()> {
    let st = c.state(store);
    match st.pid {
        Some(pid) if st.running() => {
            if unsafe { libc::kill(pid, sig) } != 0 {
                bail!("signalling {}: {}", c.name, std::io::Error::last_os_error());
            }
            Ok(())
        }
        _ => bail!("container {} is not running", c.name),
    }
}

fn wait_stopped(store: &Store, c: &Container, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !c.state(store).running() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !c.state(store).running()
}

/// SIGTERM, then SIGKILL after `timeout`. Stopping a stopped container is a no-op.
pub fn stop(store: &Store, c: &Container, timeout: Duration) -> Result<()> {
    if !c.state(store).running() {
        return Ok(());
    }
    let _ = signal(store, c, libc::SIGTERM);
    if !wait_stopped(store, c, timeout) {
        let _ = signal(store, c, libc::SIGKILL);
        wait_stopped(store, c, Duration::from_secs(5));
    }
    // Give the supervisor a moment to record the exit code.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && fs::read_to_string(c.dir(store).join("state.json")).map(|s| s.contains("\"running\"")).unwrap_or(false) {
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

pub fn remove(store: &Store, c: &Container, force: bool) -> Result<()> {
    if c.state(store).running() {
        if !force {
            bail!("container {} is running; stop it first or use -f", c.name);
        }
        let _ = signal(store, c, libc::SIGKILL);
        wait_stopped(store, c, Duration::from_secs(5));
    }
    remove_tree(&c.dir(store))
}

/// Block until the container stops; returns its exit code.
pub fn wait(store: &Store, c: &Container) -> Result<Option<i32>> {
    while c.state(store).running() {
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(50));
    Ok(c.state(store).exit_code)
}

/// The console turns "\n" into "\r\n"; undo that for logs and captured output.
pub fn clean_output(b: &[u8]) -> String {
    String::from_utf8_lossy(b).replace("\r\n", "\n")
}

/// Print (and optionally follow) a container's log.
pub fn logs(store: &Store, c: &Container, follow: bool, tail: Option<usize>, out: &mut dyn Write) -> Result<()> {
    let path = c.log_path(store);
    let mut pos = 0u64;
    if let Ok(mut f) = File::open(&path) {
        let mut buf = vec![];
        f.read_to_end(&mut buf)?;
        pos = buf.len() as u64;
        let text = clean_output(&buf);
        let text = match tail {
            Some(n) => {
                let lines: Vec<&str> = text.lines().collect();
                let mut t = lines[lines.len().saturating_sub(n)..].join("\n");
                if !t.is_empty() {
                    t.push('\n');
                }
                t
            }
            None => text,
        };
        out.write_all(text.as_bytes())?;
        out.flush()?;
    }
    while follow {
        let running = c.state(store).running();
        if let Ok(mut f) = File::open(&path) {
            f.seek(SeekFrom::Start(pos))?;
            let mut buf = vec![];
            f.read_to_end(&mut buf)?;
            if !buf.is_empty() {
                pos += buf.len() as u64;
                out.write_all(clean_output(&buf).as_bytes())?;
                out.flush()?;
                continue;
            }
        }
        if !running {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

pub struct ExecOpts {
    pub command: Vec<String>,
    pub env: Vec<String>,
    pub workdir: Option<String>,
    pub timeout: Option<Duration>,
}

/// Run a command inside a running container via the guest exec helper. Output chunks
/// are passed to `sink(is_stderr, bytes)` as they arrive. Returns None on timeout.
pub fn exec(store: &Store, c: &Container, o: &ExecOpts, sink: &mut dyn FnMut(bool, &[u8])) -> Result<Option<i32>> {
    if !c.state(store).running() {
        bail!("container {} is not running", c.name);
    }
    if o.command.is_empty() {
        bail!("exec needs a command");
    }
    let rootfs = c.rootfs(store);
    if !vm::has_shell(&rootfs) {
        bail!("exec needs /bin/sh in the image, and {} has none", c.image);
    }
    let image = c.image_meta(store)?;
    let mut config = c.config.clone();
    config.env.extend(
        o.env.iter().map(|e| if e.contains('=') { e.clone() } else { format!("{e}={}", std::env::var(e).unwrap_or_default()) }),
    );
    let mut s = String::from("#!/bin/sh\n");
    for kv in vm::effective_env(&image, &config) {
        s.push_str(&format!("export {}\n", shell_quote(&kv)));
    }
    let wd = o.workdir.clone().unwrap_or_else(|| vm::effective_workdir(&image, &c.config));
    s.push_str(&format!("cd {} || exit 126\nexec", shell_quote(&wd)));
    for a in &o.command {
        s.push(' ');
        s.push_str(&shell_quote(a));
    }
    s.push('\n');

    let dir = rootfs.join(".agentbox/exec");
    fs::create_dir_all(&dir)?;
    let job = random_hex(8);
    let base = dir.join(&job);
    let with = |ext: &str| PathBuf::from(format!("{}.{ext}", base.display()));
    fs::write(with("tmp"), s)?;
    fs::rename(with("tmp"), with("cmd"))?;

    let cleanup = || {
        for ext in ["cmd", "run", "out", "err", "rc", "rc.tmp", "tmp"] {
            let _ = fs::remove_file(with(ext));
        }
    };
    let start = Instant::now();
    let mut offsets = [0u64, 0u64];
    let mut drain = |sink: &mut dyn FnMut(bool, &[u8])| {
        for (i, ext) in ["out", "err"].iter().enumerate() {
            if let Ok(mut f) = File::open(with(ext)) {
                if f.seek(SeekFrom::Start(offsets[i])).is_ok() {
                    let mut buf = vec![];
                    if f.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                        offsets[i] += buf.len() as u64;
                        sink(i == 1, &buf);
                    }
                }
            }
        }
    };
    loop {
        drain(sink);
        if let Ok(rc) = fs::read_to_string(with("rc")) {
            drain(sink);
            cleanup();
            return Ok(Some(rc.trim().parse().unwrap_or(-1)));
        }
        if with("cmd").exists() && start.elapsed() > Duration::from_secs(5) {
            cleanup();
            bail!("the exec helper in {} isn't responding (it needs /bin/sh, mv and sleep in the image)", c.name);
        }
        if !c.state(store).running() {
            drain(sink);
            cleanup();
            bail!("container {} stopped while the command was running", c.name);
        }
        if o.timeout.map(|t| start.elapsed() > t).unwrap_or(false) {
            cleanup(); // the guest process keeps running; its output is discarded
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// exec with output collected (MCP).
pub fn exec_captured(store: &Store, c: &Container, o: &ExecOpts) -> Result<RunResult> {
    let start = Instant::now();
    let (mut out, mut err) = (vec![], vec![]);
    let code = exec(store, c, o, &mut |is_err, b| {
        let buf = if is_err { &mut err } else { &mut out };
        if buf.len() < MAX_CAPTURE {
            buf.extend_from_slice(&b[..b.len().min(MAX_CAPTURE - buf.len())]);
        }
    })?;
    Ok(RunResult {
        container: c.name.clone(),
        exit_code: code,
        timed_out: code.is_none(),
        duration_ms: start.elapsed().as_millis(),
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
    })
}

/// Create + start with output captured and a wall-clock timeout (MCP / scripted use).
/// The VM console merges the guest's stdout and stderr; it all lands in `stdout`.
pub fn run_captured(store: &Store, opts: &RunOpts, timeout: Duration) -> Result<RunResult> {
    let c = create(store, opts)?;
    if let Err(e) = check_ports(&c) {
        let _ = remove_tree(&c.dir(store));
        return Err(e);
    }
    let start = Instant::now();
    let spec = match vm::build_spec(store, &c) {
        Ok(s) => s,
        Err(e) => {
            let _ = remove_tree(&c.dir(store));
            return Err(e);
        }
    };
    let mut child = vm::command(&spec)?.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    c.mark_running(store, child.id() as i32);
    let read = |r: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = vec![];
            if let Some(r) = r {
                let _ = r.take(MAX_CAPTURE as u64).read_to_end(&mut buf);
            }
            buf
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
    let code = status.map(exit_code_of);
    let out = out.join().unwrap_or_default();
    let err = err.join().unwrap_or_default();
    // Keep the output in the container log too, so `logs` works for kept containers.
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(c.log_path(store)) {
        let _ = f.write_all(&out);
    }
    c.mark_exited(store, code);
    Ok(RunResult {
        container: c.name.clone(),
        exit_code: if timed_out { None } else { code },
        timed_out,
        duration_ms: start.elapsed().as_millis(),
        stdout: clean_output(&out),
        stderr: String::from_utf8_lossy(&err).into_owned(),
    })
}

pub fn prune(store: &Store) -> Result<Vec<String>> {
    let mut removed = vec![];
    for c in list(store)? {
        if !c.state(store).running() {
            remove_tree(&c.dir(store))?;
            removed.push(c.id.clone());
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::normalize_port;

    #[test]
    fn ports() {
        assert_eq!(normalize_port("8080:80").unwrap(), "8080:80");
        assert_eq!(normalize_port("127.0.0.1:8080:80").unwrap(), "8080:80");
        assert_eq!(normalize_port("8080:80/tcp").unwrap(), "8080:80");
        assert_eq!(normalize_port("5432").unwrap(), "5432:5432");
        assert!(normalize_port("53:53/udp").is_err());
        assert!(normalize_port("x:80").is_err());
    }
}
