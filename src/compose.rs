//! `agentbox compose`: Compose files (compose.yaml / docker-compose.yml) on top of the
//! container and build modules.
//!
//! Networking: agentbox VMs have no private network; their sockets go through the host.
//! Each service's container ports (published ones, `expose`, and the image's EXPOSE) are
//! mapped to host ports: the published port, else the same number if it's free, else a
//! free one. Inside every container, each other service's name resolves to its own
//! loopback address (127.0.1.N), where a small forwarder (the guest helper) passes its
//! ports on to those host ports. So `postgres:5432` works even when something else on
//! the host already uses 5432.

use crate::build::{self, parse_dollar, parse_duration_ns, BuildOpts};
use crate::container::{self, Container, ExecOpts};
use crate::registry;
use crate::store::Store;
use crate::vm::RunOpts;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const L_PROJECT: &str = "agentbox.compose.project";
pub const L_SERVICE: &str = "agentbox.compose.service";
pub const L_HASH: &str = "agentbox.compose.config-hash";
pub const L_ONEOFF: &str = "agentbox.compose.oneoff";
pub const L_DIR: &str = "agentbox.compose.project-dir";
pub const L_PUBLISHED: &str = "agentbox.compose.published";

/// Port plan: service -> (container port -> host port).
pub struct Net {
    pub ports: BTreeMap<String, BTreeMap<u16, u16>>,
}

#[derive(Default, Clone)]
pub struct ComposeOpts {
    pub files: Vec<PathBuf>,
    pub project_name: Option<String>,
    pub project_dir: Option<PathBuf>,
    pub env_files: Vec<PathBuf>,
    pub profiles: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct BuildSpec {
    pub context: PathBuf,
    pub dockerfile: Option<PathBuf>,
    pub args: Vec<String>,
    pub target: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Health {
    pub test: Vec<String>,
    pub interval: Duration,
    pub timeout: Duration,
    pub retries: u32,
    pub start_period: Duration,
}

#[derive(Debug, Clone)]
pub struct Service {
    pub name: String,
    pub image: String,
    pub build: Option<BuildSpec>,
    pub pull_policy: String,
    pub opts: RunOpts,
    pub depends_on: Vec<(String, String)>,
    pub healthcheck: Option<Health>,
    pub health_disabled: bool,
    pub container_name: String,
    /// (host, container) published ports.
    pub published: Vec<(u16, u16)>,
    pub expose: Vec<u16>,
    pub stop_timeout: Duration,
    pub profiles: Vec<String>,
    pub explicit_image: bool,
}

pub struct Project {
    pub name: String,
    pub dir: PathBuf,
    pub files: Vec<PathBuf>,
    /// All services, in dependency order.
    pub services: Vec<Service>,
    /// Declared volume key -> store volume name, and whether it is external.
    pub volumes: BTreeMap<String, (String, bool)>,
    pub active_profiles: Vec<String>,
}

// ---------------------------------------------------------------------------
// Loading

const DEFAULT_FILES: &[&str] = &["compose.yaml", "compose.yml", "docker-compose.yaml", "docker-compose.yml"];
const OVERRIDE_FILES: &[&str] = &["compose.override.yaml", "compose.override.yml", "docker-compose.override.yaml", "docker-compose.override.yml"];

fn find_files(opts: &ComposeOpts) -> Result<Vec<PathBuf>> {
    if !opts.files.is_empty() {
        return opts.files.iter().map(|f| fs::canonicalize(f).with_context(|| format!("compose file {}", f.display()))).collect();
    }
    if let Ok(env_files) = std::env::var("COMPOSE_FILE") {
        let sep = std::env::var("COMPOSE_PATH_SEPARATOR").unwrap_or_else(|_| ":".into());
        return env_files.split(sep.as_str()).map(|f| fs::canonicalize(f).with_context(|| format!("compose file {f}"))).collect();
    }
    let mut dir = match &opts.project_dir {
        Some(d) => fs::canonicalize(d)?,
        None => std::env::current_dir()?,
    };
    loop {
        if let Some(f) = DEFAULT_FILES.iter().map(|f| dir.join(f)).find(|p| p.is_file()) {
            let mut files = vec![f];
            if let Some(o) = OVERRIDE_FILES.iter().map(|f| dir.join(f)).find(|p| p.is_file()) {
                files.push(o);
            }
            return Ok(files);
        }
        if !dir.pop() {
            bail!("no compose file found (looked for {} here and in parent directories)", DEFAULT_FILES.join(", "));
        }
    }
}

pub fn parse_env_file(text: &str) -> Vec<(String, String)> {
    let mut out = vec![];
    for l in text.lines() {
        let l = l.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let l = l.strip_prefix("export ").unwrap_or(l);
        let (k, v) = match l.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => (l, ""),
        };
        let v = if v.len() >= 2 && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\''))) {
            let inner = &v[1..v.len() - 1];
            if v.starts_with('"') { inner.replace("\\n", "\n").replace("\\\"", "\"") } else { inner.to_string() }
        } else {
            // strip an inline comment: `KEY=value # note`
            match v.find(" #") {
                Some(i) => v[..i].trim_end().to_string(),
                None => v.to_string(),
            }
        };
        out.push((k.to_string(), v));
    }
    out
}

/// Compose interpolation: `$$` is a literal `$`.
fn interpolate(s: &str, lookup: build::Lookup) -> Result<String> {
    let c: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < c.len() {
        if c[i] == '$' && i + 1 < c.len() && c[i + 1] == '$' {
            out.push('$');
            i += 2;
        } else if c[i] == '$' {
            let (v, n) = parse_dollar(&c, i, lookup)?;
            out.push_str(&v);
            i = n;
        } else {
            out.push(c[i]);
            i += 1;
        }
    }
    Ok(out)
}

fn interpolate_value(v: &mut Value, lookup: build::Lookup) -> Result<()> {
    match v {
        Value::String(s) => *s = interpolate(s, lookup)?,
        Value::Array(a) => {
            for x in a {
                interpolate_value(x, lookup)?;
            }
        }
        Value::Object(m) => {
            for (_, x) in m.iter_mut() {
                interpolate_value(x, lookup)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn yaml_to_json(text: &str, path: &Path) -> Result<Value> {
    let mut y: serde_yaml::Value = serde_yaml::from_str(text).with_context(|| format!("parsing {}", path.display()))?;
    y.apply_merge().ok();
    let y = strip_tags(y);
    Ok(serde_json::to_value(&y)?)
}

/// Compose's `!reset` / `!override` tags become markers that `merge` honours.
fn strip_tags(v: serde_yaml::Value) -> serde_yaml::Value {
    use serde_yaml::Value as Y;
    match v {
        Y::Tagged(t) => {
            let tag = t.tag.to_string();
            let inner = strip_tags(t.value);
            let mut m = serde_yaml::Mapping::new();
            match tag.trim_start_matches('!') {
                "reset" => {
                    m.insert(Y::String("$reset".into()), Y::Bool(true));
                    Y::Mapping(m)
                }
                "override" => {
                    m.insert(Y::String("$override".into()), inner);
                    Y::Mapping(m)
                }
                _ => inner,
            }
        }
        Y::Sequence(s) => Y::Sequence(s.into_iter().map(strip_tags).collect()),
        Y::Mapping(m) => Y::Mapping(m.into_iter().map(|(k, v)| (k, strip_tags(v))).collect()),
        other => other,
    }
}

/// Lists of KEY=VAL -> maps, so files can be merged key by key.
fn normalize_kv(v: &Value) -> Value {
    match v {
        Value::Array(a) => {
            let mut m = Map::new();
            for x in a {
                if let Some(s) = scalar(x) {
                    match s.split_once('=') {
                        Some((k, val)) => m.insert(k.to_string(), json!(val)),
                        None => m.insert(s, Value::Null),
                    };
                }
            }
            Value::Object(m)
        }
        other => other.clone(),
    }
}

fn merge(base: &mut Value, over: Value, key: &str) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                if v.get("$reset").is_some() {
                    b.remove(&k);
                    continue;
                }
                if let Some(x) = v.get("$override") {
                    b.insert(k, x.clone());
                    continue;
                }
                match b.get_mut(&k) {
                    Some(existing) => merge(existing, v, &k),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (Value::Array(b), Value::Array(o)) if matches!(key, "ports" | "expose" | "volumes" | "extra_hosts" | "env_file" | "dns" | "profiles") => {
            for x in o {
                if !b.contains(&x) {
                    b.push(x);
                }
            }
        }
        (b, o) => *b = o,
    }
}

/// Markers left over when the tagged key didn't exist in an earlier file.
fn unwrap_markers(v: &mut Value) {
    match v {
        Value::Object(m) => {
            if let Some(x) = m.get("$override").cloned() {
                *v = x;
                unwrap_markers(v);
                return;
            }
            m.retain(|_, x| x.get("$reset").is_none());
            for (_, x) in m.iter_mut() {
                unwrap_markers(x);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(unwrap_markers),
        _ => {}
    }
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn strings(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().filter_map(scalar).collect(),
        Some(x) => scalar(x).into_iter().collect(),
        None => vec![],
    }
}

/// `command:`/`entrypoint:` as a string (split like a shell) or a list.
fn argv(v: Option<&Value>) -> Result<Option<Vec<String>>> {
    Ok(match v {
        None | Some(Value::Null) => None,
        Some(Value::Array(a)) => Some(a.iter().filter_map(scalar).collect()),
        Some(x) => Some(build::shell_split(&scalar(x).unwrap_or_default())?),
    })
}

fn sanitize_project(s: &str) -> String {
    let s: String = s.to_ascii_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    let s = s.trim_start_matches(['-', '_']).to_string();
    if s.is_empty() { "default".into() } else { s }
}

fn parse_port_list(v: &Value) -> Result<Vec<(u16, u16)>> {
    let mut out = vec![];
    let items: Vec<Value> = match v {
        Value::Array(a) => a.clone(),
        other => vec![other.clone()],
    };
    for p in items {
        if let Value::Object(m) = &p {
            if m.get("protocol").and_then(|x| x.as_str()).map(|x| x != "tcp").unwrap_or(false) {
                eprintln!("agentbox: warning: only tcp ports are supported; skipping {p}");
                continue;
            }
            let target: u16 = m.get("target").and_then(scalar).ok_or_else(|| anyhow!("port without target: {p}"))?.parse()?;
            let published = m.get("published").and_then(scalar).map(|x| x.parse::<u16>()).transpose()?.unwrap_or(target);
            out.push((published, target));
            continue;
        }
        let s = scalar(&p).ok_or_else(|| anyhow!("bad port {p}"))?;
        let (spec, proto) = s.split_once('/').unwrap_or((s.as_str(), "tcp"));
        if proto != "tcp" {
            eprintln!("agentbox: warning: only tcp ports are supported; skipping {s}");
            continue;
        }
        // [ip:][host[-range]:]container[-range]; ip may be [::1]
        let spec = match spec.rfind(']') {
            Some(i) => spec[i + 1..].trim_start_matches(':'),
            None => spec,
        };
        let parts: Vec<&str> = spec.split(':').collect();
        let (host, cont) = match parts.as_slice() {
            [c] => (*c, *c),
            [h, c] => (*h, *c),
            [_ip, h, c] => (if h.is_empty() { *c } else { *h }, *c),
            _ => bail!("bad port {s:?}"),
        };
        let range = |r: &str| -> Result<(u16, u16)> {
            match r.split_once('-') {
                Some((a, b)) => Ok((a.parse()?, b.parse()?)),
                None => {
                    let n: u16 = r.parse().with_context(|| format!("bad port {s:?}"))?;
                    Ok((n, n))
                }
            }
        };
        let (h0, h1) = range(host)?;
        let (c0, c1) = range(cont)?;
        if c1 - c0 != h1 - h0 && h0 != h1 {
            bail!("port ranges differ in length: {s}");
        }
        for k in 0..=(c1 - c0) {
            out.push((if h0 == h1 && c0 != c1 { h0 + k } else { h0 + k }, c0 + k));
        }
    }
    Ok(out)
}

fn health_from(v: &Value) -> Result<(Option<Health>, bool)> {
    if v.get("disable").and_then(|d| d.as_bool()).unwrap_or(false) {
        return Ok((None, true));
    }
    let test = match v.get("test") {
        Some(Value::String(s)) => vec!["CMD-SHELL".to_string(), s.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(scalar).collect(),
        _ => return Ok((None, false)),
    };
    if test.first().map(|t| t == "NONE").unwrap_or(false) {
        return Ok((None, true));
    }
    let dur = |k: &str, d: u64| -> Result<Duration> {
        Ok(Duration::from_nanos(match v.get(k).and_then(scalar) {
            Some(s) => parse_duration_ns(&s)?,
            None => d,
        }))
    };
    Ok((
        Some(Health {
            test,
            interval: dur("interval", 30_000_000_000)?,
            timeout: dur("timeout", 30_000_000_000)?,
            retries: v.get("retries").and_then(|r| r.as_u64()).unwrap_or(3) as u32,
            start_period: dur("start_period", 0)?,
        }),
        false,
    ))
}

/// The image's own HEALTHCHECK (Docker config format, durations in ns).
fn image_health(h: &Value) -> Option<Health> {
    let test: Vec<String> = h.get("Test")?.as_array()?.iter().filter_map(scalar).collect();
    if test.is_empty() || test[0] == "NONE" {
        return None;
    }
    let ns = |k: &str, d: u64| Duration::from_nanos(h.get(k).and_then(|x| x.as_u64()).filter(|x| *x > 0).unwrap_or(d));
    Some(Health {
        test,
        interval: ns("Interval", 30_000_000_000),
        timeout: ns("Timeout", 30_000_000_000),
        retries: h.get("Retries").and_then(|x| x.as_u64()).filter(|x| *x > 0).unwrap_or(3) as u32,
        start_period: ns("StartPeriod", 0),
    })
}

pub fn load(store: &Store, opts: &ComposeOpts) -> Result<Project> {
    let files = find_files(opts)?;
    let dir = match &opts.project_dir {
        Some(d) => fs::canonicalize(d)?,
        None => files[0].parent().unwrap_or(Path::new("/")).to_path_buf(),
    };

    // Variables: the shell environment wins over .env / --env-file.
    let mut vars: BTreeMap<String, String> = BTreeMap::new();
    let env_files: Vec<PathBuf> = if opts.env_files.is_empty() { vec![dir.join(".env")] } else { opts.env_files.clone() };
    for f in &env_files {
        match fs::read_to_string(f) {
            Ok(t) => vars.extend(parse_env_file(&t)),
            Err(e) if !opts.env_files.is_empty() => bail!("env file {}: {e}", f.display()),
            Err(_) => {}
        }
    }
    for (k, v) in std::env::vars() {
        vars.insert(k, v);
    }
    let lookup = |k: &str| vars.get(k).cloned();

    let mut doc = Value::Object(Map::new());
    for f in &files {
        let text = fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?;
        let mut v = yaml_to_json(&text, f)?;
        if let Some(svcs) = v.get_mut("services").and_then(|s| s.as_object_mut()) {
            for (_, s) in svcs.iter_mut() {
                for k in ["environment", "labels", "extra_hosts"] {
                    if let Some(x) = s.get(k).cloned() {
                        if k != "extra_hosts" || x.is_object() {
                            s[k] = normalize_kv(&x);
                        }
                    }
                }
                if let Some(Value::Object(m)) = s.get("extra_hosts").cloned() {
                    s["extra_hosts"] = Value::Array(m.iter().map(|(k, v)| json!(format!("{k}:{}", scalar(v).unwrap_or_default()))).collect());
                }
            }
        }
        interpolate_value(&mut v, &lookup).with_context(|| format!("interpolating {}", f.display()))?;
        merge(&mut doc, v, "");
    }
    unwrap_markers(&mut doc);

    let name = opts
        .project_name
        .clone()
        .or_else(|| std::env::var("COMPOSE_PROJECT_NAME").ok())
        .or_else(|| doc.get("name").and_then(scalar))
        .unwrap_or_else(|| dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "default".into()));
    let name = sanitize_project(&name);

    let mut active_profiles = opts.profiles.clone();
    if active_profiles.is_empty() {
        if let Ok(p) = std::env::var("COMPOSE_PROFILES") {
            active_profiles = p.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        }
    }

    let mut volumes = BTreeMap::new();
    if let Some(Value::Object(vs)) = doc.get("volumes") {
        for (k, v) in vs {
            let external = v.get("external").map(|e| e.as_bool().unwrap_or(true)).unwrap_or(false);
            let vname = v.get("name").and_then(scalar).unwrap_or_else(|| if external { k.clone() } else { format!("{name}_{k}") });
            volumes.insert(k.clone(), (vname, external));
        }
    }

    let svcs = doc.get("services").and_then(|s| s.as_object()).ok_or_else(|| anyhow!("the compose file has no services"))?;
    let all_names: Vec<String> = svcs.keys().cloned().collect();
    let mut services = vec![];
    for (sname, s) in svcs {
        services.push(
            service(store, &name, &dir, sname, s, &mut volumes, &all_names, svcs).with_context(|| format!("service {sname}"))?,
        );
    }

    // Dependency order.
    let mut ordered: Vec<Service> = vec![];
    let mut state: BTreeMap<String, u8> = BTreeMap::new();
    fn visit(n: &str, by: &BTreeMap<String, Service>, st: &mut BTreeMap<String, u8>, out: &mut Vec<Service>) -> Result<()> {
        match st.get(n) {
            Some(2) => return Ok(()),
            Some(1) => bail!("dependency cycle involving service {n}"),
            _ => {}
        }
        st.insert(n.to_string(), 1);
        let s = by.get(n).ok_or_else(|| anyhow!("unknown service {n}"))?;
        for (d, _) in &s.depends_on {
            if by.contains_key(d) {
                visit(d, by, st, out)?;
            }
        }
        st.insert(n.to_string(), 2);
        out.push(s.clone());
        Ok(())
    }
    let by: BTreeMap<String, Service> = services.iter().map(|s| (s.name.clone(), s.clone())).collect();
    for s in &services {
        visit(&s.name, &by, &mut state, &mut ordered)?;
    }
    Ok(Project { name, dir, files, services: ordered, volumes, active_profiles })
}

#[allow(clippy::too_many_arguments)]
fn service(
    store: &Store,
    project: &str,
    dir: &Path,
    sname: &str,
    s: &Value,
    volumes: &mut BTreeMap<String, (String, bool)>,
    all: &[String],
    _svcs: &Map<String, Value>,
) -> Result<Service> {
    let rel = |p: &str| -> PathBuf {
        if let Some(r) = p.strip_prefix("~/") {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(r)
        } else if p.starts_with('/') {
            PathBuf::from(p)
        } else {
            dir.join(p)
        }
    };
    let build = match s.get("build") {
        None | Some(Value::Null) => None,
        Some(Value::String(c)) => Some(BuildSpec { context: rel(c), dockerfile: None, args: vec![], target: None }),
        Some(b) => {
            let ctx = rel(&b.get("context").and_then(scalar).unwrap_or_else(|| ".".into()));
            let args = match b.get("args") {
                Some(a) => normalize_kv(a)
                    .as_object()
                    .map(|m| m.iter().map(|(k, v)| match scalar(v) { Some(v) => format!("{k}={v}"), None => k.clone() }).collect())
                    .unwrap_or_default(),
                None => vec![],
            };
            Some(BuildSpec {
                dockerfile: b.get("dockerfile").and_then(scalar).map(|d| if d.starts_with('/') { PathBuf::from(d) } else { ctx.join(d) }),
                context: ctx,
                args,
                target: b.get("target").and_then(scalar),
            })
        }
    };
    let explicit_image = s.get("image").and_then(scalar).is_some();
    let image = match s.get("image").and_then(scalar) {
        Some(i) => i,
        None if build.is_some() => format!("{project}-{sname}"),
        None => bail!("needs an image or a build section"),
    };

    let mut env: Vec<String> = vec![];
    for f in s.get("env_file").map(|v| match v {
        Value::Array(a) => a.clone(),
        other => vec![other.clone()],
    }).unwrap_or_default() {
        let (path, required) = match &f {
            Value::Object(m) => (m.get("path").and_then(scalar).unwrap_or_default(), m.get("required").and_then(|r| r.as_bool()).unwrap_or(true)),
            other => (scalar(other).unwrap_or_default(), true),
        };
        match fs::read_to_string(rel(&path)) {
            Ok(t) => env.extend(parse_env_file(&t).into_iter().map(|(k, v)| format!("{k}={v}"))),
            Err(e) if required => bail!("env_file {path}: {e}"),
            Err(_) => {}
        }
    }
    if let Some(Value::Object(m)) = s.get("environment") {
        for (k, v) in m {
            env.retain(|e| e.split('=').next() != Some(k.as_str()));
            match scalar(v) {
                Some(v) => env.push(format!("{k}={v}")),
                None => {
                    if let Ok(v) = std::env::var(k) {
                        env.push(format!("{k}={v}"));
                    }
                }
            }
        }
    }

    let published = match s.get("ports") {
        Some(p) => parse_port_list(p)?,
        None => vec![],
    };
    let expose: Vec<u16> = strings(s.get("expose")).iter().filter_map(|e| e.split('/').next()?.parse().ok()).collect();

    let mut vols = vec![];
    for v in s.get("volumes").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
        let (src, target, ro, ty) = match &v {
            Value::Object(m) => (
                m.get("source").and_then(scalar).unwrap_or_default(),
                m.get("target").and_then(scalar).unwrap_or_default(),
                m.get("read_only").and_then(|r| r.as_bool()).unwrap_or(false),
                m.get("type").and_then(scalar).unwrap_or_else(|| "volume".into()),
            ),
            other => {
                let st = scalar(other).unwrap_or_default();
                let parts: Vec<&str> = st.splitn(3, ':').collect();
                match parts.as_slice() {
                    [t] => (String::new(), t.to_string(), false, "volume".to_string()),
                    [a, b] => (a.to_string(), b.to_string(), false, String::new()),
                    [a, b, m] => (a.to_string(), b.to_string(), m.split(',').any(|x| x == "ro"), String::new()),
                    _ => bail!("bad volume {st}"),
                }
            }
        };
        if ty == "tmpfs" || ty == "npipe" {
            eprintln!("agentbox: warning: service {sname}: {ty} mounts are not supported; {target} stays in the container filesystem");
            continue;
        }
        if src.is_empty() {
            continue; // anonymous volume: the container's own filesystem persists anyway
        }
        let is_path = ty == "bind" || (ty.is_empty() && (src.starts_with('.') || src.starts_with('/') || src.starts_with('~')));
        let host = if is_path {
            let p = rel(&src);
            if !p.exists() {
                fs::create_dir_all(&p).with_context(|| format!("creating bind mount source {}", p.display()))?;
            }
            p.to_string_lossy().to_string()
        } else {
            let vname = match volumes.get(&src) {
                Some((n, _)) => n.clone(),
                None => {
                    eprintln!("agentbox: warning: service {sname} uses volume {src}, which is not declared under top-level volumes");
                    let n = format!("{project}_{src}");
                    volumes.insert(src.clone(), (n.clone(), false));
                    n
                }
            };
            let d = store.volume_dir(&vname)?;
            d.to_string_lossy().to_string()
        };
        vols.push(format!("{host}:{target}{}", if ro { ":ro" } else { "" }));
    }

    let mut depends_on = vec![];
    match s.get("depends_on") {
        Some(Value::Array(a)) => {
            for d in a.iter().filter_map(scalar) {
                depends_on.push((d, "service_started".to_string()));
            }
        }
        Some(Value::Object(m)) => {
            for (d, c) in m {
                let cond = c.get("condition").and_then(scalar).unwrap_or_else(|| "service_started".into());
                depends_on.push((d.clone(), cond));
            }
        }
        _ => {}
    }
    for (d, _) in &depends_on {
        if !all.contains(d) {
            bail!("depends on undefined service {d}");
        }
    }

    let (healthcheck, health_disabled) = match s.get("healthcheck") {
        Some(h) => health_from(h)?,
        None => (None, false),
    };

    let mem = s
        .get("mem_limit")
        .or_else(|| s.pointer("/deploy/resources/limits/memory"))
        .and_then(scalar)
        .map(|m| crate::parse_memory(&m))
        .transpose()?;
    let cpus = s
        .get("cpus")
        .or_else(|| s.pointer("/deploy/resources/limits/cpus"))
        .and_then(scalar)
        .map(|c| c.parse::<f32>().map(|f| f.ceil().clamp(1.0, 64.0) as u8))
        .transpose()?;
    let default_cpus = std::thread::available_parallelism().map(|n| n.get().min(8) as u8).unwrap_or(4);
    if s.get("scale").and_then(|x| x.as_u64()).unwrap_or(1) > 1 || s.pointer("/deploy/replicas").and_then(|x| x.as_u64()).unwrap_or(1) > 1 {
        eprintln!("agentbox: warning: service {sname}: scaling is not supported; running one container");
    }
    let network = s.get("network_mode").and_then(scalar).map(|n| n != "none").unwrap_or(true);

    let opts = RunOpts {
        image: image.clone(),
        command: argv(s.get("command"))?.unwrap_or_default(),
        entrypoint: argv(s.get("entrypoint"))?,
        env,
        workdir: s.get("working_dir").and_then(scalar),
        volumes: vols,
        ports: vec![],
        cpus: Some(cpus.unwrap_or(default_cpus)),
        memory_mib: Some(mem.unwrap_or(4096)),
        name: None,
        network: Some(network),
        rm: false,
        extra_hosts: strings(s.get("extra_hosts")),
        forwards: vec![],
        labels: BTreeMap::new(),
    };
    let stop_timeout = match s.get("stop_grace_period").and_then(scalar) {
        Some(d) => Duration::from_nanos(parse_duration_ns(&d)?),
        None => Duration::from_secs(10),
    };
    Ok(Service {
        name: sname.to_string(),
        image,
        build,
        pull_policy: s.get("pull_policy").and_then(scalar).unwrap_or_else(|| "missing".into()),
        opts,
        depends_on,
        healthcheck,
        health_disabled,
        container_name: s.get("container_name").and_then(scalar).unwrap_or_else(|| format!("{project}-{sname}-1")),
        published,
        expose,
        stop_timeout,
        profiles: strings(s.get("profiles")),
        explicit_image,
    })
}

// ---------------------------------------------------------------------------
// Helpers

impl Project {
    pub fn service(&self, n: &str) -> Result<&Service> {
        self.services.iter().find(|s| s.name == n).ok_or_else(|| anyhow!("no such service: {n}"))
    }

    fn enabled(&self, s: &Service) -> bool {
        s.profiles.is_empty() || s.profiles.iter().any(|p| self.active_profiles.contains(p) || p == "*")
            || self.active_profiles.iter().any(|p| p == "*")
    }

    /// The named services (or all enabled ones) plus their dependencies, in start order.
    pub fn selection(&self, names: &[String], with_deps: bool) -> Result<Vec<&Service>> {
        let mut want: BTreeSet<String> = BTreeSet::new();
        if names.is_empty() {
            for s in &self.services {
                if self.enabled(s) {
                    want.insert(s.name.clone());
                }
            }
        } else {
            for n in names {
                self.service(n)?;
                want.insert(n.clone());
            }
        }
        if with_deps {
            loop {
                let mut added = false;
                for s in &self.services {
                    if want.contains(&s.name) {
                        for (d, _) in &s.depends_on {
                            if want.insert(d.clone()) {
                                added = true;
                            }
                        }
                    }
                }
                if !added {
                    break;
                }
            }
        }
        Ok(self.services.iter().filter(|s| want.contains(&s.name)).collect())
    }

    /// The loopback address a service's name resolves to inside the other containers.
    fn alias(&self, n: &str) -> String {
        let mut names: Vec<&str> = self.services.iter().map(|s| s.name.as_str()).collect();
        names.sort();
        let i = names.iter().position(|x| *x == n).unwrap_or(0);
        format!("127.0.{}.{}", 1 + i / 250, 1 + i % 250)
    }

    /// Container options for service `s`, given the project's port plan. Also returns
    /// warnings about service ports this container can't reach.
    fn run_opts(&self, store: &Store, s: &Service, net: &Net) -> Result<(RunOpts, Vec<String>)> {
        let mut warnings = vec![];
        let mut o = s.opts.clone();
        o.image = registry::Reference::parse(&s.image)?.canonical();
        o.name = Some(s.container_name.clone());
        let own = net.ports.get(&s.name).cloned().unwrap_or_default();
        o.ports = own.iter().map(|(k, h)| format!("{h}:{k}")).collect();
        let moved: Vec<String> = own.iter().filter(|(k, h)| k != h).map(|(k, h)| format!("{k}->{h}")).collect();
        if !moved.is_empty() {
            warnings.push(format!(
                "inside {}, its own port(s) answer on the host number ({}), not on localhost:<container port>; healthchecks are adjusted",
                s.name,
                moved.join(", ")
            ));
        }

        let can_forward = crate::vm::GUEST_HELPER.is_some()
            && crate::vm::find_shell(&store.image_dir(&o.image).join("rootfs")).is_some()
            && o.network != Some(false);
        let mut hosts = vec![];
        let mut forwards = vec![];
        for t in &self.services {
            let names: Vec<&str> = if t.container_name != format!("{}-{}-1", self.name, t.name) {
                vec![t.name.as_str(), t.container_name.as_str()]
            } else {
                vec![t.name.as_str()]
            };
            if t.name == s.name {
                for n in names {
                    hosts.push(format!("{n}:127.0.0.1"));
                }
                continue;
            }
            let tp = net.ports.get(&t.name).cloned().unwrap_or_default();
            // A service with no known ports (no ports/expose/EXPOSE) has no port map, so
            // libkrun exposes whatever it listens on at the same number: reach it there.
            if can_forward && !tp.is_empty() {
                let a = self.alias(&t.name);
                for n in names {
                    hosts.push(format!("{n}:{a}"));
                }
                for (k, h) in &tp {
                    if own.contains_key(k) {
                        // Our own process listens on 0.0.0.0:k; a forwarder bound first on
                        // the alias address would make that fail.
                        warnings.push(format!("service {} can't reach {}:{k} (it listens on port {k} itself)", s.name, t.name));
                        continue;
                    }
                    forwards.push(format!("{a}:{k}=127.0.0.1:{h}"));
                }
            } else {
                for n in names {
                    hosts.push(format!("{n}:127.0.0.1"));
                }
                if tp.iter().any(|(k, h)| k != h) {
                    warnings.push(format!(
                        "service {} has no shell for the name forwarders; it reaches {} only at the host ports {}",
                        s.name,
                        t.name,
                        tp.iter().map(|(k, h)| format!("{k}->{h}")).collect::<Vec<_>>().join(", ")
                    ));
                }
            }
        }
        hosts.extend(o.extra_hosts.iter().cloned());
        o.extra_hosts = hosts;
        o.forwards = forwards;
        o.labels.insert(L_PROJECT.into(), self.name.clone());
        o.labels.insert(L_SERVICE.into(), s.name.clone());
        o.labels.insert(L_DIR.into(), self.dir.to_string_lossy().into_owned());
        let published: Vec<String> = s.published.iter().map(|(h, c)| format!("0.0.0.0:{h}->{c}/tcp")).collect();
        if !published.is_empty() {
            o.labels.insert(L_PUBLISHED.into(), published.join(","));
        }
        let digest = store.image(&o.image)?.map(|i| i.digest).unwrap_or_default();
        let mut h = Sha256::new();
        h.update(serde_json::to_vec(&o)?);
        h.update(digest.as_bytes());
        o.labels.insert(L_HASH.into(), format!("{:x}", h.finalize())[..16].to_string());
        Ok((o, warnings))
    }

    pub fn containers(&self, store: &Store) -> Result<Vec<Container>> {
        project_containers(store, &self.name)
    }
}

pub fn project_containers(store: &Store, project: &str) -> Result<Vec<Container>> {
    Ok(container::list(store)?.into_iter().filter(|c| c.config.labels.get(L_PROJECT).map(|p| p == project).unwrap_or(false)).collect())
}

fn service_container(store: &Store, p: &Project, s: &Service) -> Option<Container> {
    container::find(store, &s.container_name).ok().filter(|c| c.config.labels.get(L_PROJECT) == Some(&p.name))
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
extern "C" fn on_sigint(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

fn tty() -> bool {
    unsafe { libc::isatty(1) == 1 }
}

/// Writes each complete line with a `service  | ` prefix.
struct Prefixed {
    prefix: String,
    out: Arc<Mutex<Box<dyn Write + Send>>>,
    buf: Vec<u8>,
}

impl Write for Prefixed {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(b);
        while let Some(i) = self.buf.iter().position(|c| *c == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            let mut o = self.out.lock().unwrap();
            o.write_all(self.prefix.as_bytes())?;
            o.write_all(&line)?;
            o.flush()?;
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if !self.buf.is_empty() {
            let mut o = self.out.lock().unwrap();
            o.write_all(self.prefix.as_bytes())?;
            o.write_all(&self.buf)?;
            o.write_all(b"\n")?;
            self.buf.clear();
        }
        Ok(())
    }
}

fn prefixes(names: &[String]) -> Vec<String> {
    let w = names.iter().map(|n| n.len()).max().unwrap_or(0);
    const COLORS: &[u8] = &[36, 33, 32, 35, 34, 96, 93, 92, 95, 94];
    names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            if tty() {
                format!("\x1b[{}m{n:<w$} |\x1b[0m ", COLORS[i % COLORS.len()])
            } else {
                format!("{n:<w$} | ")
            }
        })
        .collect()
}

/// Follow (or print) the logs of several containers, interleaved with prefixes.
pub fn multiplex_logs(store: &Store, cs: &[Container], follow: bool, tail: Option<usize>, stop_when: Option<&dyn Fn() -> bool>) -> Result<()> {
    let out: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(Box::new(std::io::stdout())));
    let names: Vec<String> = cs.iter().map(|c| c.config.labels.get(L_SERVICE).cloned().unwrap_or_else(|| c.name.clone())).collect();
    let pre = prefixes(&names);
    let mut threads = vec![];
    for (c, p) in cs.iter().zip(pre) {
        let (store, c, out) = (store.clone(), c.clone(), out.clone());
        threads.push(std::thread::spawn(move || {
            let mut w = Prefixed { prefix: p, out, buf: vec![] };
            let _ = container::logs(&store, &c, follow, tail, &mut w);
            let _ = w.flush();
        }));
    }
    if let Some(stop) = stop_when {
        while !threads.iter().all(|t| t.is_finished()) {
            if stop() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    for t in threads {
        let _ = t.join();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Commands

pub struct UpOpts {
    pub detach: bool,
    pub build: bool,
    pub no_build: bool,
    pub force_recreate: bool,
    pub no_recreate: bool,
    pub no_deps: bool,
    pub remove_orphans: bool,
    pub wait: bool,
    pub services: Vec<String>,
    pub pull: Option<String>,
}

fn say(out: &mut dyn Write, what: &str, name: &str, status: &str) {
    let _ = writeln!(out, " {what} {name:<28} {status}");
}

/// Build (if needed or asked) the images of the given services.
pub fn build_images(store: &Store, p: &Project, svcs: &[&Service], force: bool, no_cache: bool, pull: bool, out: &mut dyn Write) -> Result<()> {
    for s in svcs {
        let Some(b) = &s.build else { continue };
        let canonical = registry::Reference::parse(&s.image)?.canonical();
        if !force && store.image(&canonical)?.is_some() {
            continue;
        }
        writeln!(out, "Building {} ({})", s.name, b.context.strip_prefix(&p.dir).map(|r| format!("./{}", r.display())).unwrap_or_else(|_| b.context.display().to_string()))?;
        build::build(
            store,
            &BuildOpts {
                context: b.context.clone(),
                dockerfile: b.dockerfile.clone(),
                tags: vec![s.image.clone()],
                build_args: b.args.clone(),
                target: b.target.clone(),
                no_cache,
                pull,
                cpus: std::thread::available_parallelism().map(|n| n.get().min(8) as u8).unwrap_or(4),
                memory_mib: 4096,
            },
            out,
        )
        .with_context(|| format!("building service {}", s.name))?;
    }
    Ok(())
}

fn ensure_images(store: &Store, p: &Project, svcs: &[&Service], o: &UpOpts, out: &mut dyn Write) -> Result<()> {
    if !o.no_build {
        build_images(store, p, svcs, o.build, false, false, out)?;
    }
    for s in svcs {
        if s.build.is_some() && (!s.explicit_image || store.image(&registry::Reference::parse(&s.image)?.canonical())?.is_some()) {
            continue;
        }
        let canonical = registry::Reference::parse(&s.image)?.canonical();
        let policy = o.pull.clone().unwrap_or_else(|| s.pull_policy.clone());
        let have = store.image(&canonical)?.is_some();
        if policy == "never" && !have {
            bail!("image {} for service {} is not present and pull_policy is never", s.image, s.name);
        }
        if policy == "always" || !have {
            writeln!(out, " Pulling {} ({})", s.name, s.image)?;
            registry::pull(store, &s.image, true).with_context(|| format!("pulling {} for service {}", s.image, s.name))?;
        }
    }
    Ok(())
}

/// The ports a service's container listens on, as far as we know: published container
/// ports, `expose`, and the image's EXPOSE list (tcp).
fn container_ports(store: &Store, s: &Service) -> Vec<u16> {
    let mut v: Vec<u16> = s.published.iter().map(|(_, c)| *c).chain(s.expose.iter().copied()).collect();
    if let Some(m) = registry::Reference::parse(&s.image).ok().and_then(|r| store.image(&r.canonical()).ok().flatten()) {
        v.extend(m.exposed_ports.iter().filter(|p| !p.ends_with("/udp")).filter_map(|p| p.split('/').next()?.parse::<u16>().ok()));
    }
    v.sort();
    v.dedup();
    v
}

fn port_free(p: u16) -> bool {
    std::net::TcpListener::bind(("0.0.0.0", p)).is_ok() && std::net::TcpListener::bind(("127.0.0.1", p)).is_ok()
}

/// Map every service's container ports to host ports, keeping the mapping of existing
/// containers stable so unchanged services aren't recreated.
pub fn plan_net(store: &Store, p: &Project) -> Result<Net> {
    let mut used: BTreeSet<u16> = BTreeSet::new();
    let mut ports: BTreeMap<String, BTreeMap<u16, u16>> = BTreeMap::new();
    let mut svcs: Vec<&Service> = p.services.iter().collect();
    svcs.sort_by(|a, b| a.name.cmp(&b.name));
    // Published ports are fixed; reserve them first.
    for s in &svcs {
        for (h, _) in &s.published {
            used.insert(*h);
        }
    }
    for s in svcs {
        let existing = service_container(store, p, s);
        let running = existing.as_ref().map(|c| c.state(store).running()).unwrap_or(false);
        let old: BTreeMap<u16, u16> = existing
            .as_ref()
            .map(|c| c.config.ports.iter().filter_map(|m| {
                let (h, k) = m.split_once(':')?;
                Some((k.parse().ok()?, h.parse().ok()?))
            }).collect())
            .unwrap_or_default();
        let mut m = BTreeMap::new();
        for k in container_ports(store, s) {
            let h = if let Some((h, _)) = s.published.iter().find(|(_, c)| *c == k) {
                *h
            } else if let Some(h) = old.get(&k).filter(|h| !used.contains(h) && (running || port_free(**h))) {
                *h
            } else if !used.contains(&k) && port_free(k) {
                k
            } else {
                let mut hs = Sha256::new();
                hs.update(format!("{}/{}/{k}", p.name, s.name));
                let seed = u16::from_be_bytes([hs.clone().finalize()[0], hs.finalize()[1]]) as u32;
                let mut cand = 20000 + seed % 40000;
                let mut tries = 0;
                loop {
                    let c = cand as u16;
                    if !used.contains(&c) && port_free(c) {
                        break c;
                    }
                    cand = if cand >= 59999 { 20000 } else { cand + 1 };
                    tries += 1;
                    if tries > 40000 {
                        bail!("no free host port for {}:{k}", s.name);
                    }
                }
            };
            used.insert(h);
            m.insert(k, h);
        }
        ports.insert(s.name.clone(), m);
    }
    Ok(Net { ports })
}

/// Published ports must be free (unlike internal ones, they can't move).
fn check_published(starting: &[&Service]) -> Result<()> {
    for s in starting {
        for (h, _) in &s.published {
            if !port_free(*h) {
                bail!("port {h} (published by service {}) is already in use on this machine", s.name);
            }
        }
    }
    Ok(())
}

/// Replace `from` (e.g. "localhost:5432") when it isn't followed by another digit.
fn replace_port(s: &str, from: &str, to: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find(from) {
        let after = &rest[i + from.len()..];
        out.push_str(&rest[..i]);
        if after.starts_with(|c: char| c.is_ascii_digit()) {
            out.push_str(from);
        } else {
            out.push_str(to);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

fn health_ok(store: &Store, c: &Container, h: &Health) -> bool {
    let command = match h.test.first().map(String::as_str) {
        Some("CMD-SHELL") => vec!["/bin/sh".to_string(), "-c".to_string(), h.test[1..].join(" ")],
        Some("CMD") => h.test[1..].to_vec(),
        _ => h.test.clone(),
    };
    // Inside the VM a mapped port answers only on its host number (libkrun), so point
    // `localhost:<container port>` in the check at the host port.
    let command: Vec<String> = command
        .into_iter()
        .map(|mut a| {
            for m in &c.config.ports {
                if let Some((hp, k)) = m.split_once(':') {
                    if hp != k {
                        for host in ["localhost", "127.0.0.1", "0.0.0.0", "[::1]"] {
                            a = replace_port(&a, &format!("{host}:{k}"), &format!("{host}:{hp}"));
                        }
                    }
                }
            }
            a
        })
        .collect();
    let o = ExecOpts { command, env: vec![], workdir: None, timeout: Some(h.timeout) };
    matches!(container::exec_captured(store, c, &o), Ok(r) if r.exit_code == Some(0))
}

fn service_health(store: &Store, s: &Service) -> Option<Health> {
    if s.health_disabled {
        return None;
    }
    s.healthcheck.clone().or_else(|| {
        let r = registry::Reference::parse(&s.image).ok()?;
        store.image(&r.canonical()).ok().flatten()?.healthcheck.as_ref().and_then(image_health)
    })
}

/// Block until `c` passes its healthcheck (polling faster than `interval` at first).
fn wait_healthy(store: &Store, s: &Service, c: &Container, out: &mut dyn Write) -> Result<()> {
    let Some(h) = service_health(store, s) else {
        writeln!(out, " warning: service {} has no healthcheck; treating it as healthy once started", s.name)?;
        return Ok(());
    };
    // Docker counts a failure per `interval` after `start_period` and gives up after
    // `retries` of them. We probe more often than that (so a service that comes up fast
    // isn't held back) but count at most one failure per interval, and allow a few
    // seconds of grace for the VM to boot.
    let start = Instant::now();
    let grace = h.start_period + Duration::from_secs(5);
    let mut failures = 0;
    let mut last_counted: Option<Instant> = None;
    loop {
        if INTERRUPTED.load(Ordering::SeqCst) {
            bail!("interrupted");
        }
        if !c.state(store).running() {
            bail!("container {} exited while waiting for it to become healthy (exit code {}); see `agentbox logs {}`",
                c.name, c.state(store).exit_code.map(|x| x.to_string()).unwrap_or("?".into()), c.name);
        }
        if health_ok(store, c, &h) {
            return Ok(());
        }
        if start.elapsed() > grace && last_counted.map(|t| t.elapsed() >= h.interval).unwrap_or(true) {
            failures += 1;
            last_counted = Some(Instant::now());
            if failures >= h.retries.max(1) {
                bail!("container {} is unhealthy (healthcheck {:?} failed {} times)", c.name, h.test, failures);
            }
        }
        let pause = h.interval.min(Duration::from_secs(1)).max(Duration::from_millis(200));
        let deadline = Instant::now() + pause;
        while Instant::now() < deadline {
            if INTERRUPTED.load(Ordering::SeqCst) {
                bail!("interrupted");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn wait_condition(store: &Store, p: &Project, dep: &Service, cond: &str, out: &mut dyn Write) -> Result<()> {
    let c = service_container(store, p, dep).ok_or_else(|| anyhow!("dependency {} has no container", dep.name))?;
    match cond {
        "service_healthy" => {
            wait_healthy(store, dep, &c, out)?;
            say(out, "Container", &c.name, "Healthy");
        }
        "service_completed_successfully" => {
            let code = container::wait(store, &c)?;
            if code != Some(0) {
                bail!("service {} didn't complete successfully: exit {}", dep.name, code.map(|x| x.to_string()).unwrap_or("?".into()));
            }
            say(out, "Container", &c.name, "Exited (0)");
        }
        _ => {}
    }
    Ok(())
}

/// Create (or recreate) and start the containers of `svcs`, honouring depends_on.
fn converge(store: &Store, p: &Project, svcs: &[&Service], o: &UpOpts, out: &mut dyn Write) -> Result<Vec<Container>> {
    // Which containers need (re)creating?
    let net = plan_net(store, p)?;
    let mut plan: Vec<(&Service, RunOpts, Option<Container>, bool)> = vec![]; // (svc, opts, existing, recreate)
    let mut warned = BTreeSet::new();
    for s in svcs {
        let (opts, warnings) = p.run_opts(store, s, &net)?;
        for w in warnings {
            if warned.insert(w.clone()) {
                writeln!(out, " warning: {w}")?;
            }
        }
        let existing = container::find(store, &s.container_name).ok();
        if let Some(c) = &existing {
            if c.config.labels.get(L_PROJECT) != Some(&p.name) {
                bail!("container name {} is already used by a container outside project {}", s.container_name, p.name);
            }
        }
        let recreate = match &existing {
            None => false,
            Some(_) if o.no_recreate => false,
            Some(c) => o.force_recreate || c.config.labels.get(L_HASH) != opts.labels.get(L_HASH),
        };
        plan.push((s, opts, existing, recreate));
    }
    // Port checks for everything that isn't already running unchanged.
    let starting: Vec<&Service> = plan
        .iter()
        .filter(|(_, _, ex, re)| *re || ex.as_ref().map(|c| !c.state(store).running()).unwrap_or(true))
        .map(|(s, ..)| *s)
        .collect();
    // Containers we're about to recreate release their ports first.
    for (s, _, ex, re) in &plan {
        if *re {
            if let Some(c) = ex {
                say(out, "Container", &c.name, "Recreating");
                container::stop(store, c, s.stop_timeout)?;
                container::remove(store, c, true)?;
            }
        }
    }
    check_published(&starting)?;

    let mut result = vec![];
    for (s, opts, existing, recreate) in plan {
        for (dep, cond) in &s.depends_on {
            if let Ok(d) = p.service(dep) {
                if svcs.iter().any(|x| x.name == d.name) {
                    wait_condition(store, p, d, cond, out).with_context(|| format!("dependency of {}", s.name))?;
                }
            }
        }
        let c = match existing {
            Some(c) if !recreate => c,
            _ => {
                let c = container::create(store, &opts)?;
                say(out, "Container", &c.name, "Created");
                c
            }
        };
        if c.state(store).running() {
            say(out, "Container", &c.name, "Running");
        } else {
            container::start_detached(store, &c).with_context(|| format!("starting service {}", s.name))?;
            say(out, "Container", &c.name, "Started");
        }
        result.push(c);
    }
    Ok(result)
}

fn remove_orphans(store: &Store, p: &Project, out: &mut dyn Write) -> Result<()> {
    for c in p.containers(store)? {
        let svc = c.config.labels.get(L_SERVICE).cloned().unwrap_or_default();
        if c.config.labels.contains_key(L_ONEOFF) || p.services.iter().any(|s| s.name == svc) {
            continue;
        }
        container::stop(store, &c, Duration::from_secs(10))?;
        container::remove(store, &c, true)?;
        say(out, "Container", &c.name, "Removed (orphan)");
    }
    Ok(())
}

pub fn up(store: &Store, p: &Project, o: &UpOpts, out: &mut dyn Write) -> Result<i32> {
    let mut out = out;
    let svcs = p.selection(&o.services, !o.no_deps)?;
    if svcs.is_empty() {
        bail!("no services to start (all are behind profiles?)");
    }
    ensure_images(store, p, &svcs, o, &mut out)?;
    for (n, (v, ext)) in &p.volumes {
        let d = store.volume_dir(v)?;
        if *ext && !d.exists() {
            bail!("external volume {n} ({v}) does not exist; create it with `agentbox volume create {v}`");
        }
        fs::create_dir_all(d)?;
    }
    if o.remove_orphans {
        remove_orphans(store, p, &mut out)?;
    } else {
        let orphans: Vec<String> = p
            .containers(store)?
            .into_iter()
            .filter(|c| !c.config.labels.contains_key(L_ONEOFF))
            .filter(|c| !p.services.iter().any(|s| Some(&s.name) == c.config.labels.get(L_SERVICE)))
            .map(|c| c.name)
            .collect();
        if !orphans.is_empty() {
            writeln!(out, " warning: orphan containers ({}) for this project; run with --remove-orphans to clean them up", orphans.join(", "))?;
        }
    }
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_sigint as *const () as libc::sighandler_t);
    }
    let started = converge(store, p, &svcs, o, &mut out)?;
    if o.detach {
        if o.wait {
            for s in &svcs {
                if let Some(c) = service_container(store, p, s) {
                    let st = c.state(store);
                    if !st.running() {
                        // A one-shot service that finished cleanly is fine; anything else failed.
                        if st.exit_code != Some(0) {
                            bail!(
                                "container {} exited with code {}; see `agentbox compose logs {}`",
                                c.name,
                                st.exit_code.map(|x| x.to_string()).unwrap_or("?".into()),
                                s.name
                            );
                        }
                    } else if service_health(store, s).is_some() {
                        wait_healthy(store, s, &c, &mut out)?;
                        say(&mut out, "Container", &c.name, "Healthy");
                    }
                }
            }
        }
        return Ok(0);
    }
    writeln!(out, "Attaching to {} (Ctrl-C stops them)", started.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", "))?;
    let stop = || INTERRUPTED.load(Ordering::SeqCst);
    multiplex_logs(store, &started, true, None, Some(&stop))?;
    if INTERRUPTED.load(Ordering::SeqCst) {
        writeln!(out, "Gracefully stopping... (press Ctrl-C again to force)")?;
        INTERRUPTED.store(false, Ordering::SeqCst);
        for (s, c) in svcs.iter().zip(started.iter()).rev() {
            if INTERRUPTED.load(Ordering::SeqCst) {
                let _ = container::signal(store, c, libc::SIGKILL);
            } else {
                container::stop(store, c, s.stop_timeout)?;
            }
            say(&mut out, "Container", &c.name, "Stopped");
        }
    }
    Ok(0)
}

pub fn down(store: &Store, p: &Project, volumes: bool, rmi: Option<&str>, orphans: bool, out: &mut dyn Write) -> Result<()> {
    let mut out = out;
    let order: Vec<&str> = p.services.iter().map(|s| s.name.as_str()).collect();
    let mut cs = p.containers(store)?;
    cs.retain(|c| {
        orphans || c.config.labels.contains_key(L_ONEOFF) || order.iter().any(|n| Some(&n.to_string()) == c.config.labels.get(L_SERVICE))
    });
    // Reverse dependency order: dependents first.
    cs.sort_by_key(|c| std::cmp::Reverse(order.iter().position(|n| Some(&n.to_string()) == c.config.labels.get(L_SERVICE)).unwrap_or(usize::MAX)));
    for c in &cs {
        let t = c
            .config
            .labels
            .get(L_SERVICE)
            .and_then(|n| p.service(n).ok())
            .map(|s| s.stop_timeout)
            .unwrap_or(Duration::from_secs(10));
        if c.state(store).running() {
            say(&mut out, "Container", &c.name, "Stopping");
            container::stop(store, c, t)?;
        }
        container::remove(store, c, true)?;
        say(&mut out, "Container", &c.name, "Removed");
    }
    if volumes {
        for (v, ext) in p.volumes.values() {
            if *ext {
                continue;
            }
            let d = store.volume_dir(v)?;
            if d.exists() {
                crate::store::remove_tree(&d)?;
                say(&mut out, "Volume", v, "Removed");
            }
        }
    }
    if let Some(mode) = rmi {
        for s in &p.services {
            if mode == "local" && s.explicit_image {
                continue;
            }
            let r = registry::Reference::parse(&s.image)?.canonical();
            if store.image(&r)?.is_some() && container::list(store)?.iter().all(|c| c.image != r) {
                store.remove_image(&r)?;
                say(&mut out, "Image", &r, "Removed");
            }
        }
    }
    Ok(())
}

pub fn ps_rows(store: &Store, p: &Project, all: bool, services: &[String]) -> Result<Vec<(Container, String)>> {
    let mut rows = vec![];
    let order: Vec<&str> = p.services.iter().map(|s| s.name.as_str()).collect();
    let mut cs = p.containers(store)?;
    cs.sort_by_key(|c| order.iter().position(|n| Some(&n.to_string()) == c.config.labels.get(L_SERVICE)).unwrap_or(usize::MAX));
    for c in cs {
        let svc = c.config.labels.get(L_SERVICE).cloned().unwrap_or_default();
        if !services.is_empty() && !services.contains(&svc) {
            continue;
        }
        if !all && !c.state(store).running() {
            continue;
        }
        rows.push((c, svc));
    }
    Ok(rows)
}

/// Published ports of a compose container, Docker-style.
pub fn ports_text(c: &Container) -> String {
    match c.config.labels.get(L_PUBLISHED) {
        Some(p) => p.replace(',', ", "),
        None if c.config.labels.contains_key(L_PROJECT) => String::new(),
        None => c.config.ports.iter().filter_map(|p| p.split_once(':').map(|(h, g)| format!("0.0.0.0:{h}->{g}/tcp"))).collect::<Vec<_>>().join(", "),
    }
}

/// One-off `compose run`: dependencies are started like `up -d`, then the command runs
/// attached in a new container.
#[allow(clippy::too_many_arguments)]
pub fn run(store: &Store, p: &Project, service: &str, command: Vec<String>, env: Vec<String>, rm: bool, no_deps: bool, service_ports: bool, entrypoint: Option<Vec<String>>, workdir: Option<String>, detach: bool) -> Result<i32> {
    let mut out = std::io::stderr();
    let s = p.service(service)?;
    let deps: Vec<&Service> = if no_deps {
        vec![]
    } else {
        p.selection(&[service.to_string()], true)?.into_iter().filter(|x| x.name != s.name).collect()
    };
    let o = UpOpts { detach: true, build: false, no_build: false, force_recreate: false, no_recreate: false, no_deps: false, remove_orphans: false, wait: false, services: vec![], pull: None };
    ensure_images(store, p, &[s], &o, &mut out)?;
    if !deps.is_empty() {
        ensure_images(store, p, &deps, &o, &mut out)?;
        converge(store, p, &deps, &o, &mut out)?;
        for (d, cond) in &s.depends_on {
            if let Ok(ds) = p.service(d) {
                wait_condition(store, p, ds, cond, &mut out)?;
            }
        }
    }
    let net = plan_net(store, p)?;
    let (mut opts, _) = p.run_opts(store, s, &net)?;
    opts.name = Some(format!("{}-{}-run-{}", p.name, s.name, &uuidish()[..12]));
    if !command.is_empty() {
        opts.command = command;
    }
    if let Some(e) = entrypoint {
        opts.entrypoint = Some(e);
    }
    if workdir.is_some() {
        opts.workdir = workdir;
    }
    opts.env.extend(env);
    // A one-off container doesn't take the service's ports (the service may be running).
    opts.ports = if service_ports { s.published.iter().map(|(h, c)| format!("{h}:{c}")).collect() } else { vec![] };
    opts.labels.remove(L_PUBLISHED);
    opts.rm = rm;
    opts.labels.insert(L_ONEOFF.into(), "true".into());
    let c = container::create(store, &opts)?;
    if detach {
        container::start_detached(store, &c)?;
        println!("{}", c.name);
        return Ok(0);
    }
    container::start_attached(store, &c)
}

fn uuidish() -> String {
    let mut b = [0u8; 8];
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        let _ = std::io::Read::read_exact(&mut f, &mut b);
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Normalized configuration, for `compose config`.
pub fn config_json(store: &Store, p: &Project) -> Result<Value> {
    let net = plan_net(store, p)?;
    let mut svcs = Map::new();
    for s in &p.services {
        let (o, _) = p.run_opts(store, s, &net)?;
        svcs.insert(
            s.name.clone(),
            json!({
                "image": s.image,
                "build": s.build.as_ref().map(|b| json!({"context": b.context, "dockerfile": b.dockerfile, "args": b.args, "target": b.target})),
                "container_name": s.container_name,
                "command": o.command,
                "entrypoint": o.entrypoint,
                "environment": o.env,
                "volumes": o.volumes,
                "ports": s.published.iter().map(|(h, c)| format!("{h}:{c}")).collect::<Vec<_>>(),
                "expose": s.expose,
                "depends_on": s.depends_on.iter().map(|(d, c)| json!({"service": d, "condition": c})).collect::<Vec<_>>(),
                "healthcheck": s.healthcheck.as_ref().map(|h| json!({"test": h.test, "interval": format!("{:?}", h.interval), "retries": h.retries})),
                "cpus": o.cpus,
                "memory_mib": o.memory_mib,
                "profiles": s.profiles,
                "network": o.network,
                "host_ports": o.ports,
                "hosts": o.extra_hosts,
            }),
        );
    }
    Ok(json!({
        "name": p.name,
        "project_dir": p.dir,
        "files": p.files,
        "services": svcs,
        "volumes": p.volumes.iter().map(|(k, (n, ext))| (k.clone(), json!({"name": n, "external": ext}))).collect::<Map<_, _>>(),
    }))
}

/// Projects with containers in the store: (name, running, total, dir).
pub fn projects(store: &Store) -> Result<Vec<(String, usize, usize, String)>> {
    let mut m: BTreeMap<String, (usize, usize, String)> = BTreeMap::new();
    for c in container::list(store)? {
        if let Some(pn) = c.config.labels.get(L_PROJECT) {
            let e = m.entry(pn.clone()).or_insert((0, 0, c.config.labels.get(L_DIR).cloned().unwrap_or_default()));
            e.1 += 1;
            if c.state(store).running() {
                e.0 += 1;
            }
        }
    }
    Ok(m.into_iter().map(|(k, (r, t, d))| (k, r, t, d)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports() {
        let v: Value = serde_json::from_str(r#"["5434:5432", 8080, "127.0.0.1:9000:90", "3000-3001:4000-4001", {"target": 80, "published": "8081"}, "53:53/udp"]"#).unwrap();
        assert_eq!(parse_port_list(&v).unwrap(), vec![(5434, 5432), (8080, 8080), (9000, 90), (3000, 4000), (3001, 4001), (8081, 80)]);
    }

    #[test]
    fn rewrites_ports() {
        assert_eq!(replace_port("wget -qO- http://localhost:8080/health", "localhost:8080", "localhost:20080"), "wget -qO- http://localhost:20080/health");
        assert_eq!(replace_port("curl localhost:80801", "localhost:8080", "localhost:9"), "curl localhost:80801");
    }

    #[test]
    fn interpolation() {
        let lk = |k: &str| match k {
            "A" => Some("1".to_string()),
            "E" => Some(String::new()),
            _ => None,
        };
        assert_eq!(interpolate("${A}-${B:-x}-${E:-d}-${E-d}-$$A-$A", &lk).unwrap(), "1-x-d--$A-1");
        assert!(interpolate("${B:?need B}", &lk).is_err());
        assert_eq!(interpolate("${FIELD:-./.f}:/p:ro", &lk).unwrap(), "./.f:/p:ro");
    }

    #[test]
    fn env_files() {
        let e = parse_env_file("# c\nA=1\nexport B=\"two words\"\nC='x'\nD=v # note\n");
        assert_eq!(e, vec![("A".into(), "1".into()), ("B".into(), "two words".into()), ("C".into(), "x".into()), ("D".into(), "v".into())]);
    }

    #[test]
    fn merges_files() {
        let mut a: Value = serde_json::from_str(r#"{"services":{"x":{"image":"a","ports":["1:1"],"environment":{"K":"1"}}}}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"services":{"x":{"image":"b","ports":["2:2"],"environment":{"L":"2"}}}}"#).unwrap();
        merge(&mut a, b, "");
        assert_eq!(a["services"]["x"]["image"], "b");
        assert_eq!(a["services"]["x"]["ports"], json!(["1:1", "2:2"]));
        assert_eq!(a["services"]["x"]["environment"], json!({"K": "1", "L": "2"}));
    }
}
