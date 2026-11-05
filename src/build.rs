//! `agentbox build`: classic Dockerfiles (multi-stage, ARG/ENV, COPY/ADD, RUN with
//! `--mount=type=cache|bind`, heredocs), without a daemon or BuildKit.
//!
//! Each stage works on a plain rootfs directory. `RUN` boots that directory in a microVM;
//! `COPY`/`ADD` write into it from the host. After every filesystem step the tree is
//! snapshotted (APFS clonefile) into `build-cache/<key>/`, where the key chains the parent
//! key, the instruction and (for COPY/ADD) the content of the copied files, so an
//! unchanged prefix of a Dockerfile is skipped on the next build, like Docker's layer cache.

use crate::container::{clean_output, exit_code_of};
use crate::registry;
use crate::store::{clone_tree, remove_tree, ImageMeta, Store};
use crate::vm::{self, GuestRun};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

pub struct BuildOpts {
    pub context: PathBuf,
    /// Defaults to `<context>/Dockerfile`.
    pub dockerfile: Option<PathBuf>,
    pub tags: Vec<String>,
    /// KEY=VALUE, or KEY to take it from the environment.
    pub build_args: Vec<String>,
    pub target: Option<String>,
    pub no_cache: bool,
    /// Pull base images even if present.
    pub pull: bool,
    pub cpus: u8,
    pub memory_mib: u32,
}

// ---------------------------------------------------------------------------
// Variable expansion ($VAR, ${VAR}, ${VAR:-x}, ${VAR-x}, ${VAR:+x}, ${VAR+x}, ${VAR:?x}),
// shared with compose interpolation.

pub type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Expand the `$` expression at `s[i]` (which is '$'). Returns the text and the next index.
pub fn parse_dollar(s: &[char], i: usize, lookup: Lookup) -> Result<(String, usize)> {
    let is_name = |c: char| c.is_ascii_alphanumeric() || c == '_';
    if i + 1 >= s.len() {
        return Ok(("$".into(), i + 1));
    }
    if s[i + 1] == '{' {
        let mut depth = 1;
        let mut j = i + 2;
        while j < s.len() {
            match s[j] {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        if j >= s.len() {
            bail!("unterminated ${{ in {:?}", s.iter().collect::<String>());
        }
        let inner: Vec<char> = s[i + 2..j].to_vec();
        let n = inner.iter().take_while(|c| is_name(**c)).count();
        let name: String = inner[..n].iter().collect();
        let rest: String = inner[n..].iter().collect();
        let val = lookup(&name);
        let word = |w: &str| -> Result<String> { expand_plain(w, lookup) };
        let out = if rest.is_empty() {
            val.unwrap_or_default()
        } else if let Some(w) = rest.strip_prefix(":-") {
            match val.filter(|v| !v.is_empty()) {
                Some(v) => v,
                None => word(w)?,
            }
        } else if let Some(w) = rest.strip_prefix(":+") {
            if val.map(|v| !v.is_empty()).unwrap_or(false) { word(w)? } else { String::new() }
        } else if let Some(w) = rest.strip_prefix(":?") {
            match val.filter(|v| !v.is_empty()) {
                Some(v) => v,
                None => bail!("required variable {name} is missing a value: {}", word(w)?),
            }
        } else if let Some(w) = rest.strip_prefix('-') {
            match val {
                Some(v) => v,
                None => word(w)?,
            }
        } else if let Some(w) = rest.strip_prefix('+') {
            if val.is_some() { word(w)? } else { String::new() }
        } else if let Some(w) = rest.strip_prefix('?') {
            match val {
                Some(v) => v,
                None => bail!("required variable {name} is missing a value: {}", word(w)?),
            }
        } else {
            bail!("unsupported substitution ${{{}}}", inner.iter().collect::<String>());
        };
        return Ok((out, j + 1));
    }
    let n = s[i + 1..].iter().take_while(|c| is_name(**c)).count();
    if n == 0 {
        return Ok(("$".into(), i + 1));
    }
    let name: String = s[i + 1..i + 1 + n].iter().collect();
    Ok((lookup(&name).unwrap_or_default(), i + 1 + n))
}

/// Expand every `$` expression in `s` (no quote handling).
pub fn expand_plain(s: &str, lookup: Lookup) -> Result<String> {
    let c: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < c.len() {
        if c[i] == '$' {
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

/// Split a command line like a POSIX shell would (quotes, backslashes; no expansion).
pub fn shell_split(s: &str) -> Result<Vec<String>> {
    lex(s, '\\', None)
}

/// Dockerfile word lexer: whitespace-separated words with quotes, escapes and (optionally)
/// variable expansion. Quotes are removed; expanded values are not re-split.
fn lex(s: &str, esc: char, lookup: Option<Lookup>) -> Result<Vec<String>> {
    let c: Vec<char> = s.chars().collect();
    let mut words = vec![];
    let mut cur = String::new();
    let mut in_word = false;
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch == esc && i + 1 < c.len() {
            cur.push(c[i + 1]);
            in_word = true;
            i += 2;
        } else if ch == '\'' {
            in_word = true;
            i += 1;
            while i < c.len() && c[i] != '\'' {
                cur.push(c[i]);
                i += 1;
            }
            i += 1;
        } else if ch == '"' {
            in_word = true;
            i += 1;
            while i < c.len() && c[i] != '"' {
                if c[i] == esc && i + 1 < c.len() && ['"', '$', esc].contains(&c[i + 1]) {
                    cur.push(c[i + 1]);
                    i += 2;
                } else if c[i] == '$' && lookup.is_some() {
                    let (v, n) = parse_dollar(&c, i, lookup.unwrap())?;
                    cur.push_str(&v);
                    i = n;
                } else {
                    cur.push(c[i]);
                    i += 1;
                }
            }
            i += 1;
        } else if ch == '$' && lookup.is_some() {
            let (v, n) = parse_dollar(&c, i, lookup.unwrap())?;
            cur.push_str(&v);
            in_word = true;
            i = n;
        } else if ch.is_whitespace() {
            if in_word {
                words.push(std::mem::take(&mut cur));
                in_word = false;
            }
            i += 1;
        } else {
            cur.push(ch);
            in_word = true;
            i += 1;
        }
    }
    if in_word {
        words.push(cur);
    }
    Ok(words)
}

// ---------------------------------------------------------------------------
// Dockerfile parsing

#[derive(Debug, Clone)]
pub struct Instr {
    pub cmd: String,
    pub args: String,
    pub line: usize,
    /// Heredoc bodies (`RUN <<EOF`, `COPY <<EOF /dest`), in order.
    pub heredocs: Vec<(String, String)>,
}

pub fn parse_dockerfile(text: &str) -> Result<(Vec<Instr>, char)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut esc = '\\';
    // Parser directives: leading `# key=value` comments.
    for l in &lines {
        let t = l.trim();
        let Some(d) = t.strip_prefix('#') else { break };
        let Some((k, v)) = d.split_once('=') else { break };
        match k.trim().to_ascii_lowercase().as_str() {
            "escape" => esc = v.trim().chars().next().unwrap_or('\\'),
            "syntax" | "check" => {}
            _ => break,
        }
    }
    let mut out = vec![];
    let mut i = 0;
    while i < lines.len() {
        let start = i;
        let t = lines[i].trim();
        if t.is_empty() || t.starts_with('#') {
            i += 1;
            continue;
        }
        let mut logical = String::new();
        loop {
            let l = lines[i];
            let tl = l.trim();
            i += 1;
            if (tl.starts_with('#') || tl.is_empty()) && !logical.is_empty() {
                // comment or blank line inside a continuation
            } else {
                let trimmed = l.trim_end();
                if let Some(body) = trimmed.strip_suffix(esc) {
                    logical.push_str(body);
                    if i < lines.len() {
                        continue;
                    }
                } else {
                    logical.push_str(trimmed);
                    break;
                }
            }
            if i >= lines.len() {
                break;
            }
        }
        let logical = logical.trim().to_string();
        let (cmd, args) = match logical.split_once(char::is_whitespace) {
            Some((c, a)) => (c.to_ascii_uppercase(), a.trim().to_string()),
            None => (logical.to_ascii_uppercase(), String::new()),
        };
        // Heredocs: <<EOF, <<-EOF, <<"EOF", <<'EOF'
        let mut heredocs = vec![];
        if matches!(cmd.as_str(), "RUN" | "COPY" | "ADD") {
            let mut rest = args.as_str();
            while let Some(p) = rest.find("<<") {
                let after = &rest[p + 2..];
                let strip_tabs = after.starts_with('-');
                let after = after.trim_start_matches('-');
                let word: String = after
                    .trim_start_matches(['"', '\''])
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if word.is_empty() {
                    break;
                }
                let mut body = String::new();
                while i < lines.len() && lines[i].trim_end() != word && !(strip_tabs && lines[i].trim() == word) {
                    let l = if strip_tabs { lines[i].trim_start_matches('\t') } else { lines[i] };
                    body.push_str(l);
                    body.push('\n');
                    i += 1;
                }
                if i >= lines.len() {
                    bail!("line {}: heredoc {word} is not terminated", start + 1);
                }
                i += 1;
                heredocs.push((word.clone(), body));
                rest = &after[word.len().min(after.len())..];
            }
        }
        out.push(Instr { cmd, args, line: start + 1, heredocs });
    }
    Ok((out, esc))
}

/// Leading `--flag=value` options of an instruction.
fn take_flags(args: &str) -> (Vec<(String, String)>, String) {
    let mut flags = vec![];
    let mut rest = args.trim_start();
    while let Some(f) = rest.strip_prefix("--") {
        let end = f.find(char::is_whitespace).unwrap_or(f.len());
        let flag = &f[..end];
        let (k, v) = flag.split_once('=').unwrap_or((flag, "true"));
        flags.push((k.to_string(), v.to_string()));
        rest = f[end..].trim_start();
    }
    (flags, rest.to_string())
}

fn json_array(s: &str) -> Option<Vec<String>> {
    let t = s.trim();
    if !t.starts_with('[') {
        return None;
    }
    serde_json::from_str::<Vec<String>>(t).ok()
}

/// "30s", "1m30s", "500ms", "2h" -> nanoseconds.
pub fn parse_duration_ns(s: &str) -> Result<u64> {
    let s = s.trim();
    if let Ok(n) = s.parse::<f64>() {
        return Ok((n * 1e9) as u64); // bare number: seconds
    }
    let mut total = 0f64;
    let mut num = String::new();
    let c: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < c.len() {
        if c[i].is_ascii_digit() || c[i] == '.' {
            num.push(c[i]);
            i += 1;
            continue;
        }
        let mut unit = String::new();
        while i < c.len() && c[i].is_ascii_alphabetic() {
            unit.push(c[i]);
            i += 1;
        }
        let n: f64 = num.parse().with_context(|| format!("bad duration {s:?}"))?;
        num.clear();
        total += n * match unit.as_str() {
            "ns" => 1.0,
            "us" | "µs" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60e9,
            "h" => 3600e9,
            _ => bail!("bad duration unit in {s:?}"),
        };
    }
    if !num.is_empty() {
        bail!("bad duration {s:?}");
    }
    Ok(total as u64)
}

// ---------------------------------------------------------------------------
// .dockerignore

pub struct Ignore {
    patterns: Vec<(bool, Vec<String>)>, // (negated, components)
}

impl Ignore {
    pub fn load(context: &Path, dockerfile: &Path) -> Ignore {
        // <Dockerfile>.dockerignore takes precedence, like BuildKit.
        let specific = PathBuf::from(format!("{}.dockerignore", dockerfile.display()));
        let text = fs::read_to_string(&specific).or_else(|_| fs::read_to_string(context.join(".dockerignore"))).unwrap_or_default();
        Ignore::parse(&text)
    }

    pub fn parse(text: &str) -> Ignore {
        let mut patterns = vec![];
        for l in text.lines() {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            let (neg, p) = match l.strip_prefix('!') {
                Some(p) => (true, p.trim()),
                None => (false, l),
            };
            let comps: Vec<String> = p
                .split('/')
                .filter(|c| !c.is_empty() && *c != ".")
                .map(String::from)
                .collect();
            if !comps.is_empty() {
                patterns.push((neg, comps));
            }
        }
        Ignore { patterns }
    }

    pub fn has_negations(&self) -> bool {
        self.patterns.iter().any(|(n, _)| *n)
    }

    /// Is `rel` (context-relative, '/'-separated) excluded?
    pub fn excluded(&self, rel: &str) -> bool {
        let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
        let mut ex = false;
        for (neg, pat) in &self.patterns {
            // A pattern matching a parent directory excludes everything under it.
            let hit = (1..=comps.len()).any(|n| match_comps(pat, &comps[..n]));
            if hit {
                ex = !neg;
            }
        }
        ex
    }
}

fn match_comps(pat: &[String], path: &[&str]) -> bool {
    match pat.first() {
        None => path.is_empty(),
        Some(p) if p == "**" => (0..=path.len()).any(|k| match_comps(&pat[1..], &path[k..])),
        Some(p) => !path.is_empty() && glob_match(p, path[0]) && match_comps(&pat[1..], &path[1..]),
    }
}

/// Shell-style match of one path component: `*`, `?`, `[a-z]`, `[!x]`/`[^x]`, `\` escapes.
pub fn glob_match(pat: &str, s: &str) -> bool {
    fn rec(p: &[char], s: &[char]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some('*') => (0..=s.len()).any(|k| rec(&p[1..], &s[k..])),
            Some('?') => !s.is_empty() && rec(&p[1..], &s[1..]),
            Some('[') => {
                let Some(close) = p.iter().skip(1).position(|c| *c == ']').map(|x| x + 1) else {
                    return !s.is_empty() && s[0] == '[' && rec(&p[1..], &s[1..]);
                };
                if s.is_empty() {
                    return false;
                }
                let mut set = &p[1..close];
                let negate = matches!(set.first(), Some('!') | Some('^'));
                if negate {
                    set = &set[1..];
                }
                let mut hit = false;
                let mut k = 0;
                while k < set.len() {
                    if k + 2 < set.len() && set[k + 1] == '-' {
                        if set[k] <= s[0] && s[0] <= set[k + 2] {
                            hit = true;
                        }
                        k += 3;
                    } else {
                        if set[k] == s[0] {
                            hit = true;
                        }
                        k += 1;
                    }
                }
                hit != negate && rec(&p[close + 1..], &s[1..])
            }
            Some('\\') if p.len() > 1 => !s.is_empty() && s[0] == p[1] && rec(&p[2..], &s[1..]),
            Some(c) => !s.is_empty() && s[0] == *c && rec(&p[1..], &s[1..]),
        }
    }
    rec(&pat.chars().collect::<Vec<_>>(), &s.chars().collect::<Vec<_>>())
}

// ---------------------------------------------------------------------------
// Paths inside a rootfs

/// Resolve guest path `p` inside `root` on the host, following symlinks as the guest
/// would (absolute targets are relative to `root`), never escaping it.
pub fn resolve_in_root(root: &Path, p: &str) -> Result<PathBuf> {
    let mut stack: Vec<String> = vec![];
    let mut todo: Vec<String> = p.split('/').rev().filter(|c| !c.is_empty()).map(String::from).collect();
    let mut hops = 0;
    while let Some(c) = todo.pop() {
        match c.as_str() {
            "." => continue,
            ".." => {
                stack.pop();
                continue;
            }
            _ => {}
        }
        let cand: PathBuf = root.join(stack.iter().collect::<PathBuf>()).join(&c);
        match fs::symlink_metadata(&cand) {
            Ok(m) if m.file_type().is_symlink() => {
                hops += 1;
                if hops > 40 {
                    bail!("too many symlinks resolving {p}");
                }
                let target = fs::read_link(&cand)?;
                let t = target.to_string_lossy().to_string();
                if t.starts_with('/') {
                    stack.clear();
                }
                for part in t.split('/').rev().filter(|x| !x.is_empty()) {
                    todo.push(part.to_string());
                }
            }
            _ => stack.push(c),
        }
    }
    Ok(root.join(stack.iter().collect::<PathBuf>()))
}

fn remove_any(p: &Path) -> Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => remove_tree(p),
        Ok(_) => Ok(fs::remove_file(p)?),
        Err(_) => Ok(()),
    }
}

/// Copy `src` (file, dir or symlink) to `dst`; directories merge into existing ones.
fn copy_entry(src: &Path, dst: &Path, chmod: Option<u32>, ignore: &dyn Fn(&Path) -> bool) -> Result<()> {
    let m = fs::symlink_metadata(src).with_context(|| format!("reading {}", src.display()))?;
    if m.file_type().is_symlink() {
        remove_any(dst)?;
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
    } else if m.is_dir() {
        if fs::symlink_metadata(dst).map(|d| !d.is_dir()).unwrap_or(false) {
            remove_any(dst)?;
        }
        if !dst.exists() {
            fs::create_dir_all(dst)?;
            fs::set_permissions(dst, fs::Permissions::from_mode(chmod.unwrap_or(m.mode() & 0o7777) | 0o700))?;
        }
        let mut entries: Vec<_> = fs::read_dir(src)?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let sp = e.path();
            if ignore(&sp) {
                continue;
            }
            copy_entry(&sp, &dst.join(e.file_name()), chmod, ignore)?;
        }
    } else {
        if fs::symlink_metadata(dst).is_ok() {
            remove_any(dst)?;
        }
        fs::copy(src, dst).with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
        let mode = chmod.unwrap_or(m.mode() & 0o7777) | 0o600;
        fs::set_permissions(dst, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// Feed the content of the files `copy_entry` would copy into `h`.
fn hash_entry(src: &Path, rel: &str, h: &mut Sha256, ignore: &dyn Fn(&Path) -> bool) -> Result<()> {
    let m = fs::symlink_metadata(src).with_context(|| format!("reading {}", src.display()))?;
    h.update(rel.as_bytes());
    h.update([0]);
    if m.file_type().is_symlink() {
        h.update(b"L");
        h.update(fs::read_link(src)?.to_string_lossy().as_bytes());
    } else if m.is_dir() {
        h.update(format!("D{:o}", m.mode() & 0o7777));
        let mut entries: Vec<_> = fs::read_dir(src)?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let sp = e.path();
            if ignore(&sp) {
                continue;
            }
            hash_entry(&sp, &format!("{rel}/{}", e.file_name().to_string_lossy()), h, ignore)?;
        }
    } else {
        h.update(format!("F{:o}:{}", m.mode() & 0o7777, m.len()));
        let mut f = fs::File::open(src)?;
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
    }
    h.update([0]);
    Ok(())
}

fn sha_hex(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
        h.update([0xff]);
    }
    format!("{:x}", h.finalize())
}

// ---------------------------------------------------------------------------
// Build state

#[derive(Clone, Default)]
struct Config {
    env: Vec<String>,
    entrypoint: Vec<String>,
    cmd: Vec<String>,
    workdir: String,
    user: String,
    shell: Vec<String>,
    exposed: Vec<String>,
    healthcheck: Option<Value>,
}

impl Config {
    fn set_env(&mut self, k: &str, v: &str) {
        self.env.retain(|e| e.split('=').next() != Some(k));
        self.env.push(format!("{k}={v}"));
    }
    fn get_env(&self, k: &str) -> Option<String> {
        self.env.iter().find_map(|e| e.split_once('=').filter(|(a, _)| *a == k).map(|(_, v)| v.to_string()))
    }
}

/// Where a stage's current filesystem lives.
#[derive(Clone)]
enum Fs {
    /// Empty (FROM scratch, nothing written yet).
    Empty,
    /// A read-only tree we haven't copied yet: an image rootfs or a cache snapshot.
    Shared(PathBuf),
    /// The stage's private working directory.
    Work,
}

struct Stage {
    name: Option<String>,
    config: Config,
    args: BTreeMap<String, Option<String>>,
    key: String,
    fs: Fs,
    work: PathBuf,
}

impl Stage {
    fn root(&self) -> Option<PathBuf> {
        match &self.fs {
            Fs::Empty => None,
            Fs::Shared(p) => Some(p.clone()),
            Fs::Work => Some(self.work.clone()),
        }
    }

    /// Make the working directory hold the current filesystem so it can be modified.
    fn materialize(&mut self) -> Result<()> {
        match &self.fs {
            Fs::Work => {}
            Fs::Empty => {
                remove_any(&self.work)?;
                fs::create_dir_all(&self.work)?;
            }
            Fs::Shared(p) => {
                remove_any(&self.work)?;
                clone_tree(p, &self.work)?;
            }
        }
        self.fs = Fs::Work;
        Ok(())
    }

    fn lookup(&self, build_args: &BTreeMap<String, String>) -> impl Fn(&str) -> Option<String> + '_ {
        let ba = build_args.clone();
        move |k: &str| {
            self.config.get_env(k).or_else(|| match self.args.get(k) {
                Some(def) => ba.get(k).cloned().or_else(|| def.clone()),
                None => None,
            })
        }
    }

    /// ENV plus the ARGs declared in this stage: what a RUN step sees.
    fn run_env(&self, build_args: &BTreeMap<String, String>) -> Vec<String> {
        let mut env = vec![];
        for (k, def) in &self.args {
            if let Some(v) = build_args.get(k).cloned().or_else(|| def.clone()) {
                env.push(format!("{k}={v}"));
            }
        }
        for e in &self.config.env {
            let k = e.split('=').next().unwrap_or_default();
            env.retain(|x: &String| x.split('=').next() != Some(k));
            env.push(e.clone());
        }
        if !env.iter().any(|e| e.starts_with("PATH=")) {
            env.push("PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into());
        }
        if !env.iter().any(|e| e.starts_with("HOME=")) {
            env.push("HOME=/root".into());
        }
        env
    }
}

struct Builder<'a> {
    store: &'a Store,
    opts: &'a BuildOpts,
    out: &'a mut dyn Write,
    esc: char,
    context: PathBuf,
    ignore: Ignore,
    build_args: BTreeMap<String, String>,
    global_args: BTreeMap<String, Option<String>>,
    stages: Vec<Stage>,
    dir: PathBuf,
    cache_dir: PathBuf,
    used_args: std::collections::BTreeSet<String>,
}

fn image_rootfs(store: &Store, reference: &str, pull: bool, out: &mut dyn Write) -> Result<(ImageMeta, PathBuf)> {
    let canonical = registry::Reference::parse(reference)?.canonical();
    let meta = match store.image(&canonical)? {
        Some(m) if !pull => m,
        existing => {
            writeln!(out, " pulling {canonical}")?;
            match registry::pull(store, reference, true) {
                Ok(m) => m,
                Err(e) => match existing {
                    Some(m) => {
                        writeln!(out, " (pull failed, using the local image: {e:#})")?;
                        m
                    }
                    None => return Err(e),
                },
            }
        }
    };
    let root = store.image_dir(&meta.reference).join("rootfs");
    Ok((meta, root))
}

impl<'a> Builder<'a> {
    fn ctx_ignored(&self, p: &Path) -> bool {
        match p.strip_prefix(&self.context) {
            Ok(rel) => {
                let rel = rel.to_string_lossy();
                // With `!exceptions`, descend into excluded dirs; their files are checked one by one.
                !rel.is_empty() && self.ignore.excluded(&rel) && !(self.ignore.has_negations() && p.is_dir())
            }
            Err(_) => false,
        }
    }

    fn snapshot(&mut self, idx: usize) -> Result<()> {
        if self.opts.no_cache {
            return Ok(());
        }
        let st = &self.stages[idx];
        let dst = self.cache_dir.join(&st.key);
        if dst.exists() {
            return Ok(());
        }
        let tmp = self.cache_dir.join(format!("{}.tmp-{}", st.key, std::process::id()));
        remove_any(&tmp)?;
        clone_tree(&st.work, &tmp.join("rootfs"))?;
        fs::rename(&tmp, &dst)?;
        Ok(())
    }

    /// Advance the stage's cache key; true if a snapshot for it exists (step can be skipped).
    fn step_key(&mut self, idx: usize, desc: &str) -> bool {
        let st = &mut self.stages[idx];
        st.key = sha_hex(&[st.key.as_bytes(), desc.as_bytes()]);
        if self.opts.no_cache {
            return false;
        }
        let snap = self.cache_dir.join(&st.key).join("rootfs");
        if snap.is_dir() && !matches!(st.fs, Fs::Work) {
            st.fs = Fs::Shared(snap);
            return true;
        }
        false
    }

    fn run(&mut self, instrs: Vec<Instr>) -> Result<usize> {
        // Global ARGs (before the first FROM) are only visible to FROM lines.
        let mut target_idx = None;
        let mut step = 0;
        let total = instrs.len();
        for ins in instrs {
            step += 1;
            let t0 = Instant::now();
            let label = format!("{} {}", ins.cmd, ins.args);
            let short: String = label.chars().take(110).collect();
            writeln!(self.out, "#{step}/{total} {short}{}", if label.chars().count() > 110 { "…" } else { "" })?;
            let cached = self.instruction(&ins).with_context(|| format!("Dockerfile line {}: {}", ins.line, short))?;
            if cached {
                writeln!(self.out, "  CACHED")?;
            } else if t0.elapsed().as_millis() > 500 {
                writeln!(self.out, "  done in {:.1}s", t0.elapsed().as_secs_f64())?;
            }
            if let Some(t) = &self.opts.target {
                if let Some(i) = self.stages.iter().position(|s| s.name.as_deref() == Some(t.as_str())) {
                    target_idx = Some(i);
                }
            }
        }
        if self.stages.is_empty() {
            bail!("the Dockerfile has no FROM instruction");
        }
        match &self.opts.target {
            Some(t) => target_idx.ok_or_else(|| anyhow!("target stage {t:?} not found")),
            None => Ok(self.stages.len() - 1),
        }
    }

    /// Execute one instruction; returns true if it was served from the cache.
    fn instruction(&mut self, ins: &Instr) -> Result<bool> {
        let esc = self.esc;
        if ins.cmd == "FROM" {
            return self.from(ins);
        }
        if ins.cmd == "ARG" && self.stages.is_empty() {
            for w in lex(&ins.args, esc, None)? {
                let (k, v) = match w.split_once('=') {
                    Some((k, v)) => (k.to_string(), Some(v.to_string())),
                    None => (w.clone(), None),
                };
                self.used_args.insert(k.clone());
                self.global_args.insert(k, v);
            }
            return Ok(false);
        }
        let idx = self.stages.len().checked_sub(1).ok_or_else(|| anyhow!("{} before FROM", ins.cmd))?;
        let ba = self.build_args.clone();
        match ins.cmd.as_str() {
            "ARG" => {
                let words = {
                    let st = &self.stages[idx];
                    let lk = st.lookup(&ba);
                    lex(&ins.args, esc, Some(&lk))?
                };
                for w in words {
                    let (k, v) = match w.split_once('=') {
                        Some((k, v)) => (k.to_string(), Some(v.to_string())),
                        None => {
                            let def = self.global_args.get(&w).cloned().flatten();
                            (w.clone(), def)
                        }
                    };
                    self.used_args.insert(k.clone());
                    let val = ba.get(&k).cloned().or(v.clone());
                    let st = &mut self.stages[idx];
                    st.key = sha_hex(&[st.key.as_bytes(), format!("ARG {k}={val:?}").as_bytes()]);
                    st.args.insert(k, v);
                }
                Ok(false)
            }
            "ENV" => {
                let st = &self.stages[idx];
                let lk = st.lookup(&ba);
                let words = lex(&ins.args, esc, Some(&lk))?;
                let pairs: Vec<(String, String)> = if words.first().map(|w| !w.contains('=')).unwrap_or(false) {
                    // legacy `ENV KEY value with spaces`
                    let k = ins.args.split_whitespace().next().unwrap_or_default().to_string();
                    let rest = ins.args.trim_start()[k.len()..].trim();
                    vec![(k, lex(rest, esc, Some(&lk))?.join(" "))]
                } else {
                    words
                        .iter()
                        .map(|w| w.split_once('=').map(|(a, b)| (a.to_string(), b.to_string())))
                        .collect::<Option<_>>()
                        .ok_or_else(|| anyhow!("ENV needs KEY=VALUE pairs"))?
                };
                drop(lk);
                let st = &mut self.stages[idx];
                for (k, v) in pairs {
                    st.config.set_env(&k, &v);
                }
                Ok(false)
            }
            "LABEL" | "MAINTAINER" | "STOPSIGNAL" | "VOLUME" | "ONBUILD" => Ok(false),
            "WORKDIR" => {
                let st = &self.stages[idx];
                let lk = st.lookup(&ba);
                let w = lex(&ins.args, esc, Some(&lk))?.join(" ");
                drop(lk);
                let st = &mut self.stages[idx];
                let w = if w.starts_with('/') {
                    w
                } else {
                    format!("{}/{}", st.config.workdir.trim_end_matches('/'), w)
                };
                st.config.workdir = w.clone();
                // Docker creates the directory.
                let exists = st.root().map(|r| resolve_in_root(&r, &w).map(|p| p.is_dir()).unwrap_or(false)).unwrap_or(false);
                if !exists {
                    if self.step_key(idx, &format!("WORKDIR {w}")) {
                        return Ok(true);
                    }
                    let st = &mut self.stages[idx];
                    st.materialize()?;
                    fs::create_dir_all(resolve_in_root(&st.work, &w)?)?;
                    self.snapshot(idx)?;
                }
                Ok(false)
            }
            "USER" => {
                let st = &self.stages[idx];
                let lk = st.lookup(&ba);
                let u = lex(&ins.args, esc, Some(&lk))?.join(" ");
                drop(lk);
                self.stages[idx].config.user = u;
                Ok(false)
            }
            "EXPOSE" => {
                let st = &self.stages[idx];
                let lk = st.lookup(&ba);
                let ports = lex(&ins.args, esc, Some(&lk))?;
                drop(lk);
                let st = &mut self.stages[idx];
                for p in ports {
                    let p = if p.contains('/') { p } else { format!("{p}/tcp") };
                    if !st.config.exposed.contains(&p) {
                        st.config.exposed.push(p);
                    }
                }
                Ok(false)
            }
            "SHELL" => {
                let sh = json_array(&ins.args).ok_or_else(|| anyhow!("SHELL needs a JSON array"))?;
                self.stages[idx].config.shell = sh;
                Ok(false)
            }
            "CMD" | "ENTRYPOINT" => {
                let st = &mut self.stages[idx];
                let v = match json_array(&ins.args) {
                    Some(a) => a,
                    None if ins.args.trim().is_empty() => vec![],
                    None => {
                        let mut a = st.config.shell.clone();
                        a.push(ins.args.clone());
                        a
                    }
                };
                if ins.cmd == "CMD" {
                    st.config.cmd = v;
                } else {
                    st.config.entrypoint = v;
                    st.config.cmd.clear(); // Docker resets CMD when ENTRYPOINT is set
                }
                Ok(false)
            }
            "HEALTHCHECK" => {
                let (flags, rest) = take_flags(&ins.args);
                let st = &mut self.stages[idx];
                let rest = rest.trim();
                if rest.eq_ignore_ascii_case("NONE") {
                    st.config.healthcheck = Some(json!({"Test": ["NONE"]}));
                    return Ok(false);
                }
                let body = rest.strip_prefix("CMD").or_else(|| rest.strip_prefix("cmd")).ok_or_else(|| anyhow!("HEALTHCHECK needs CMD"))?.trim();
                let test: Vec<String> = match json_array(body) {
                    Some(a) => std::iter::once("CMD".to_string()).chain(a).collect(),
                    None => vec!["CMD-SHELL".into(), body.to_string()],
                };
                let mut h = json!({"Test": test});
                for (k, v) in flags {
                    match k.as_str() {
                        "interval" => h["Interval"] = json!(parse_duration_ns(&v)?),
                        "timeout" => h["Timeout"] = json!(parse_duration_ns(&v)?),
                        "start-period" => h["StartPeriod"] = json!(parse_duration_ns(&v)?),
                        "start-interval" => h["StartInterval"] = json!(parse_duration_ns(&v)?),
                        "retries" => h["Retries"] = json!(v.parse::<u64>()?),
                        _ => {}
                    }
                }
                st.config.healthcheck = Some(h);
                Ok(false)
            }
            "RUN" => self.run_step(idx, ins),
            "COPY" | "ADD" => self.copy_step(idx, ins),
            other => bail!("unknown instruction {other}"),
        }
    }

    fn from(&mut self, ins: &Instr) -> Result<bool> {
        let (_flags, rest) = take_flags(&ins.args);
        let ga = self.global_args.clone();
        let ba = self.build_args.clone();
        let lk = move |k: &str| -> Option<String> {
            match ga.get(k) {
                Some(def) => ba.get(k).cloned().or_else(|| def.clone()),
                None => None,
            }
        };
        let words = lex(&rest, self.esc, Some(&lk))?;
        let (image, name) = match words.as_slice() {
            [img] => (img.clone(), None),
            [img, as_, n] if as_.eq_ignore_ascii_case("as") => (img.clone(), Some(n.to_ascii_lowercase())),
            _ => bail!("FROM needs `image [AS name]`"),
        };
        let idx = self.stages.len();
        let work = self.dir.join(format!("stage{idx}"));
        let mut stage = Stage { name, config: Config::default(), args: BTreeMap::new(), key: String::new(), fs: Fs::Empty, work };
        stage.config.shell = vec!["/bin/sh".into(), "-c".into()];
        let lower = image.to_ascii_lowercase();
        if lower == "scratch" {
            stage.key = sha_hex(&[b"scratch"]);
        } else if let Some(prev) = self.stages.iter().find(|s| s.name.as_deref() == Some(lower.as_str())) {
            stage.config = prev.config.clone();
            stage.key = prev.key.clone();
            stage.fs = match prev.root() {
                Some(r) => Fs::Shared(r),
                None => Fs::Empty,
            };
        } else {
            let (meta, root) = image_rootfs(self.store, &image, self.opts.pull, self.out)?;
            stage.config.env = meta.env.clone();
            stage.config.entrypoint = meta.entrypoint.clone();
            stage.config.cmd = meta.cmd.clone();
            stage.config.workdir = meta.workdir.clone();
            stage.config.user = meta.user.clone();
            stage.config.exposed = meta.exposed_ports.clone();
            stage.config.healthcheck = meta.healthcheck.clone();
            stage.key = sha_hex(&[b"image", meta.digest.as_bytes()]);
            stage.fs = Fs::Shared(root);
        }
        self.stages.push(stage);
        Ok(false)
    }

    fn run_step(&mut self, idx: usize, ins: &Instr) -> Result<bool> {
        let (flags, rest) = take_flags(&ins.args);
        let ba = self.build_args.clone();
        let mut network = true;
        let mut volumes = vec![];
        for (k, v) in &flags {
            match k.as_str() {
                "network" => network = v != "none",
                "mount" => volumes.push(self.mount(idx, v)?),
                "security" => {}
                _ => bail!("unsupported RUN flag --{k}"),
            }
        }
        let st = &self.stages[idx];
        let mut argv = match json_array(&rest) {
            Some(a) => a,
            None => {
                let mut a = st.config.shell.clone();
                a.push(rest.clone());
                a
            }
        };
        // `RUN <<EOF` (the whole heredoc is the script) or `RUN cmd <<EOF` (stdin): both
        // become a script file, run with the shell.
        let mut script = None;
        if let Some((_, body)) = ins.heredocs.first() {
            let first = rest.trim_start();
            if first.starts_with("<<") {
                script = Some(body.clone());
            } else {
                let cmdline = rest.split("<<").next().unwrap_or_default().trim().to_string();
                script = Some(format!("{cmdline} <<'__AGENTBOX_EOF'\n{body}__AGENTBOX_EOF\n"));
            }
        }
        let env = st.run_env(&ba);
        let workdir = if st.config.workdir.is_empty() { "/".to_string() } else { st.config.workdir.clone() };
        let desc = format!("RUN {flags:?} {rest} {:?} env={env:?} wd={workdir} sh={:?}", ins.heredocs, st.config.shell);
        if self.step_key(idx, &desc) {
            return Ok(true);
        }
        let st = &mut self.stages[idx];
        st.materialize()?;
        if let Some(body) = script {
            let p = st.work.join(".agentbox/run.sh");
            fs::create_dir_all(p.parent().unwrap())?;
            let body = if body.starts_with("#!") { body } else { format!("#!/bin/sh\n{body}") };
            fs::write(&p, body)?;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755))?;
            let interp = body_interpreter(&p);
            argv = match interp {
                Some(i) => vec![i, "/.agentbox/run.sh".into()],
                None => {
                    let mut a: Vec<String> = st.config.shell.iter().take(1).cloned().collect();
                    a.push("/.agentbox/run.sh".into());
                    a
                }
            };
        }
        let spec = vm::prepare(&GuestRun {
            rootfs: &st.work,
            what: "the build stage",
            argv,
            env,
            workdir,
            volumes,
            ports: Some(vec![]),
            extra_hosts: vec![],
            forwards: vec![],
            cpus: self.opts.cpus,
            memory_mib: self.opts.memory_mib,
            network,
            execd: false,
        })?;
        let mut child = vm::command(&spec)?.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        let mut stderr = child.stderr.take();
        let err_thread = std::thread::spawn(move || {
            let mut b = vec![];
            if let Some(e) = stderr.as_mut() {
                let _ = e.read_to_end(&mut b);
            }
            b
        });
        if let Some(mut so) = child.stdout.take() {
            let mut buf = [0u8; 8192];
            loop {
                let n = so.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                self.out.write_all(clean_output(&buf[..n]).as_bytes())?;
                self.out.flush()?;
            }
        }
        let status = child.wait()?;
        let err = err_thread.join().unwrap_or_default();
        if !err.is_empty() {
            self.out.write_all(&err)?;
        }
        let _ = remove_any(&st.work.join(".agentbox"));
        let code = exit_code_of(status);
        if code != 0 {
            bail!("the command returned a non-zero code: {code}");
        }
        self.snapshot(idx)?;
        Ok(false)
    }

    /// `--mount=type=cache,target=/root/.m2` or `type=bind,source=x,target=/y[,from=stage]`.
    fn mount(&mut self, idx: usize, spec: &str) -> Result<String> {
        let mut kv = BTreeMap::new();
        for part in spec.split(',') {
            let (k, v) = part.split_once('=').unwrap_or((part, "true"));
            kv.insert(k.trim().to_string(), v.trim().to_string());
        }
        let ty = kv.get("type").map(String::as_str).unwrap_or("bind");
        let target = kv.get("target").or(kv.get("dst")).or(kv.get("destination")).ok_or_else(|| anyhow!("--mount needs target="))?;
        let target = if target.starts_with('/') {
            target.clone()
        } else {
            format!("{}/{}", self.stages[idx].config.workdir.trim_end_matches('/'), target)
        };
        match ty {
            "cache" => {
                let id = kv.get("id").cloned().unwrap_or_else(|| target.clone());
                let dir = self.store.data.join("build-cache/mounts").join(crate::store::key(&id));
                fs::create_dir_all(&dir)?;
                Ok(format!("{}:{target}", dir.display()))
            }
            "bind" => {
                let src = kv.get("source").or(kv.get("src")).cloned().unwrap_or_default();
                let root = match kv.get("from") {
                    Some(f) => self.from_root(f)?,
                    None => self.context.clone(),
                };
                let p = if kv.contains_key("from") { resolve_in_root(&root, &src)? } else { safe_join(&root, &src) };
                let p = fs::canonicalize(&p).with_context(|| format!("bind mount source {src}"))?;
                let rw = kv.contains_key("rw") || kv.contains_key("readwrite");
                Ok(format!("{}:{target}{}", p.display(), if rw { "" } else { ":ro" }))
            }
            other => bail!("RUN --mount=type={other} is not supported (cache and bind are)"),
        }
    }

    fn from_root(&mut self, from: &str) -> Result<PathBuf> {
        let lower = from.to_ascii_lowercase();
        if let Some(s) = self.stages.iter().find(|s| s.name.as_deref() == Some(lower.as_str())) {
            return s.root().ok_or_else(|| anyhow!("stage {from} is empty"));
        }
        if let Ok(n) = from.parse::<usize>() {
            if let Some(s) = self.stages.get(n) {
                return s.root().ok_or_else(|| anyhow!("stage {from} is empty"));
            }
        }
        Ok(image_rootfs(self.store, from, false, self.out)?.1)
    }

    fn copy_step(&mut self, idx: usize, ins: &Instr) -> Result<bool> {
        let is_add = ins.cmd == "ADD";
        let (flags, rest) = take_flags(&ins.args);
        let mut from = None;
        let mut chmod = None;
        for (k, v) in &flags {
            match k.as_str() {
                "from" => from = Some(v.clone()),
                "chmod" => chmod = Some(u32::from_str_radix(v, 8).with_context(|| format!("bad --chmod {v}"))?),
                "chown" | "link" | "checksum" | "keep-git-dir" | "parents" => {}
                _ => bail!("unsupported {} flag --{k}", ins.cmd),
            }
        }
        let ba = self.build_args.clone();
        let words = match json_array(&rest) {
            Some(a) => {
                let st = &self.stages[idx];
                let lk = st.lookup(&ba);
                a.iter().map(|w| expand_plain(w, &lk)).collect::<Result<Vec<_>>>()?
            }
            None => {
                let st = &self.stages[idx];
                let lk = st.lookup(&ba);
                lex(&rest, self.esc, Some(&lk))?
            }
        };
        if words.len() < 2 {
            bail!("{} needs at least a source and a destination", ins.cmd);
        }
        let (srcs, dest) = words.split_at(words.len() - 1);
        let dest = &dest[0];
        let workdir = self.stages[idx].config.workdir.clone();
        let dest_abs = if dest.starts_with('/') { dest.clone() } else { format!("{}/{}", workdir.trim_end_matches('/'), dest) };
        let dest_is_dir = dest.ends_with('/') || dest == "." || srcs.len() > 1;

        // Resolve sources: (host path, name for dest, kind)
        enum Src {
            Path(PathBuf),
            Url(String),
            Inline(String, String), // heredoc name, body
        }
        let src_root = match &from {
            Some(f) => Some(self.from_root(f)?),
            None => None,
        };
        let mut resolved = vec![];
        for s in srcs {
            if let Some(h) = s.strip_prefix("<<") {
                let name = h.trim_start_matches('-').trim_matches(['"', '\'']).to_string();
                let body = ins.heredocs.iter().find(|(n, _)| *n == name).map(|(_, b)| b.clone()).unwrap_or_default();
                resolved.push(Src::Inline(name, body));
                continue;
            }
            if is_add && from.is_none() && (s.starts_with("http://") || s.starts_with("https://")) {
                resolved.push(Src::Url(s.clone()));
                continue;
            }
            if is_add && from.is_none() && s.starts_with("git@") {
                bail!("ADD from git is not supported");
            }
            let base = src_root.clone().unwrap_or_else(|| self.context.clone());
            let matches = self.glob_sources(&base, s, src_root.is_some())?;
            if matches.is_empty() {
                bail!("{}: {s:?} not found{}", ins.cmd, if src_root.is_none() { " in the build context (or excluded by .dockerignore)" } else { "" });
            }
            for m in matches {
                resolved.push(Src::Path(m));
            }
        }
        let dest_is_dir = dest_is_dir || resolved.len() > 1;

        // Cache key from the content.
        let mut h = Sha256::new();
        h.update(format!("{} {flags:?} -> {dest_abs} dir={dest_is_dir}", ins.cmd));
        for s in &resolved {
            match s {
                Src::Path(p) => {
                    let me = &*self;
                    let ign = |q: &Path| src_root.is_none() && me.ctx_ignored(q);
                    hash_entry(p, &p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), &mut h, &ign)?;
                }
                Src::Url(u) => h.update(format!("url {u}")),
                Src::Inline(n, b) => h.update(format!("inline {n} {b}")),
            }
        }
        let desc = format!("{:x}", h.finalize());
        if self.step_key(idx, &desc) {
            return Ok(true);
        }
        self.stages[idx].materialize()?;
        let work = self.stages[idx].work.clone();
        let dst_base = resolve_in_root(&work, &dest_abs)?;
        if dest_is_dir || dst_base.is_dir() {
            fs::create_dir_all(&dst_base)?;
        } else if let Some(p) = dst_base.parent() {
            fs::create_dir_all(p)?;
        }
        let dst_is_dir_now = dst_base.is_dir();
        for s in resolved {
            match s {
                Src::Path(p) => {
                    let me = &*self;
                    let ign = |q: &Path| src_root.is_none() && me.ctx_ignored(q);
                    let m = fs::metadata(&p).with_context(|| format!("reading {}", p.display()))?;
                    if m.is_dir() {
                        // A directory's contents go into dest.
                        fs::create_dir_all(&dst_base)?;
                        for e in fs::read_dir(&p)? {
                            let e = e?;
                            if ign(&e.path()) {
                                continue;
                            }
                            copy_entry(&e.path(), &dst_base.join(e.file_name()), chmod, &ign)?;
                        }
                    } else if is_add && src_root.is_none() && is_archive(&p) {
                        fs::create_dir_all(&dst_base)?;
                        extract_archive(&p, &dst_base)?;
                    } else {
                        let target = if dst_is_dir_now { dst_base.join(p.file_name().unwrap_or_default()) } else { dst_base.clone() };
                        // Follow a top-level symlink source, like Docker.
                        if fs::symlink_metadata(&target).is_ok() {
                            remove_any(&target)?;
                        }
                        fs::copy(&p, &target).with_context(|| format!("copying {}", p.display()))?;
                        fs::set_permissions(&target, fs::Permissions::from_mode(chmod.unwrap_or(m.mode() & 0o7777) | 0o600))?;
                    }
                }
                Src::Url(u) => {
                    let name = u.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("download");
                    let name = name.split(['?', '#']).next().unwrap_or(name);
                    let target = if dst_is_dir_now { dst_base.join(name) } else { dst_base.clone() };
                    writeln!(self.out, " downloading {u}")?;
                    let resp = ureq::get(&u).set("User-Agent", "agentbox").call().with_context(|| format!("downloading {u}"))?;
                    let mut f = fs::File::create(&target)?;
                    std::io::copy(&mut resp.into_reader(), &mut f)?;
                    fs::set_permissions(&target, fs::Permissions::from_mode(chmod.unwrap_or(0o600)))?;
                }
                Src::Inline(name, body) => {
                    let target = if dst_is_dir_now { dst_base.join(&name) } else { dst_base.clone() };
                    fs::write(&target, body)?;
                    fs::set_permissions(&target, fs::Permissions::from_mode(chmod.unwrap_or(0o644)))?;
                }
            }
        }
        self.snapshot(idx)?;
        Ok(false)
    }

    /// Expand a COPY source (with wildcards) under `base`.
    fn glob_sources(&self, base: &Path, pattern: &str, in_root: bool) -> Result<Vec<PathBuf>> {
        let comps: Vec<&str> = pattern.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
        if comps.iter().any(|c| *c == "..") && !in_root {
            bail!("COPY sources can't leave the build context ({pattern})");
        }
        let mut cur: Vec<PathBuf> = vec![base.to_path_buf()];
        for c in &comps {
            let mut next = vec![];
            for d in &cur {
                if c.contains(['*', '?', '[']) {
                    if let Ok(rd) = fs::read_dir(d) {
                        let mut names: Vec<_> = rd.filter_map(|e| e.ok()).collect();
                        names.sort_by_key(|e| e.file_name());
                        for e in names {
                            if glob_match(c, &e.file_name().to_string_lossy()) {
                                next.push(e.path());
                            }
                        }
                    }
                } else {
                    let p = if in_root {
                        let rel = d.strip_prefix(base).unwrap_or(Path::new("")).join(c);
                        resolve_in_root(base, &rel.to_string_lossy())?
                    } else {
                        d.join(c)
                    };
                    if fs::symlink_metadata(&p).is_ok() {
                        next.push(p);
                    }
                }
            }
            cur = next;
        }
        if !in_root {
            cur.retain(|p| !self.ctx_ignored(p) || p == base);
        }
        Ok(cur)
    }
}

fn safe_join(base: &Path, rel: &str) -> PathBuf {
    let mut p = base.to_path_buf();
    for c in Path::new(rel).components() {
        if let Component::Normal(n) = c {
            p.push(n);
        }
    }
    p
}

fn body_interpreter(p: &Path) -> Option<String> {
    let s = fs::read_to_string(p).ok()?;
    let first = s.lines().next()?;
    let i = first.strip_prefix("#!")?.trim();
    if i.is_empty() { None } else { Some(i.split_whitespace().next()?.to_string()) }
}

fn is_archive(p: &Path) -> bool {
    let mut b = [0u8; 262];
    let n = fs::File::open(p).and_then(|mut f| f.read(&mut b)).unwrap_or(0);
    (n >= 2 && b[0] == 0x1f && b[1] == 0x8b) || (n >= 262 && &b[257..262] == b"ustar")
}

fn extract_archive(p: &Path, dst: &Path) -> Result<()> {
    let f = fs::File::open(p)?;
    let mut magic = [0u8; 2];
    fs::File::open(p)?.read_exact(&mut magic)?;
    let r: Box<dyn Read> = if magic == [0x1f, 0x8b] { Box::new(flate2::read::GzDecoder::new(f)) } else { Box::new(f) };
    let mut a = tar::Archive::new(r);
    a.set_preserve_permissions(true);
    a.set_overwrite(true);
    for e in a.entries()? {
        let mut e = e?;
        let path = e.path()?.into_owned();
        if path.components().any(|c| matches!(c, Component::ParentDir)) {
            continue;
        }
        e.unpack_in(dst)?;
    }
    Ok(())
}

/// Build an image from a Dockerfile. Progress and RUN output go to `out`.
pub fn build(store: &Store, opts: &BuildOpts, out: &mut dyn Write) -> Result<ImageMeta> {
    let context = fs::canonicalize(&opts.context).with_context(|| format!("build context {}", opts.context.display()))?;
    let dockerfile = match &opts.dockerfile {
        Some(d) if d.is_absolute() => d.clone(),
        Some(d) => {
            // Relative to the current directory if it exists there, else to the context (compose).
            if d.exists() { fs::canonicalize(d)? } else { context.join(d) }
        }
        None => context.join("Dockerfile"),
    };
    let text = fs::read_to_string(&dockerfile).with_context(|| format!("reading {}", dockerfile.display()))?;
    let (instrs, esc) = parse_dockerfile(&text)?;

    let mut build_args = BTreeMap::new();
    for a in &opts.build_args {
        match a.split_once('=') {
            Some((k, v)) => build_args.insert(k.to_string(), v.to_string()),
            None => match std::env::var(a) {
                Ok(v) => build_args.insert(a.clone(), v),
                Err(_) => None,
            },
        };
    }
    let id = format!("{}-{}", std::process::id(), crate::container::now());
    let dir = store.data.join("builds").join(&id);
    fs::create_dir_all(&dir)?;
    let cache_dir = store.data.join("build-cache");
    fs::create_dir_all(&cache_dir)?;
    let t0 = Instant::now();
    let mut b = Builder {
        store,
        opts,
        out,
        esc,
        ignore: Ignore::load(&context, &dockerfile),
        context: context.clone(),
        build_args: build_args.clone(),
        global_args: BTreeMap::new(),
        stages: vec![],
        dir: dir.clone(),
        cache_dir,
        used_args: Default::default(),
    };
    let result = (|| -> Result<ImageMeta> {
        let target = b.run(instrs)?;
        for k in build_args.keys() {
            if !b.used_args.contains(k) {
                writeln!(b.out, "warning: build arg {k} was not consumed by any ARG")?;
            }
        }
        let st = &b.stages[target];
        let tags: Vec<String> = if opts.tags.is_empty() {
            let base = context.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_else(|| "image".into());
            let base: String = base.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '-' }).collect();
            vec![format!("{base}:latest")]
        } else {
            opts.tags.clone()
        };
        let canon: Vec<String> = tags.iter().map(|t| registry::Reference::parse(t).map(|r| r.canonical())).collect::<Result<_>>()?;
        let digest = format!("sha256:{}", sha_hex(&[st.key.as_bytes(), format!("{:?}", st.config.env).as_bytes(), format!("{:?}{:?}", st.config.entrypoint, st.config.cmd).as_bytes()]));
        let staging = store.image_dir(&canon[0]).with_extension("staging");
        remove_any(&staging)?;
        match st.root() {
            Some(r) => clone_tree(&r, &staging.join("rootfs"))?,
            None => fs::create_dir_all(staging.join("rootfs"))?,
        }
        let _ = remove_any(&staging.join("rootfs/.agentbox"));
        let size = dir_size(&staging.join("rootfs"));
        let mut meta = ImageMeta {
            reference: canon[0].clone(),
            digest,
            env: st.config.env.clone(),
            entrypoint: st.config.entrypoint.clone(),
            cmd: st.config.cmd.clone(),
            workdir: st.config.workdir.clone(),
            user: st.config.user.clone(),
            size_bytes: size,
            exposed_ports: st.config.exposed.clone(),
            healthcheck: st.config.healthcheck.clone(),
        };
        fs::write(staging.join("image.json"), serde_json::to_vec_pretty(&meta)?)?;
        let final_dir = store.image_dir(&canon[0]);
        remove_any(&final_dir)?;
        fs::rename(&staging, &final_dir)?;
        for c in &canon[1..] {
            let d = store.image_dir(c);
            remove_any(&d)?;
            clone_tree(&final_dir, &d)?;
            meta.reference = c.clone();
            fs::write(d.join("image.json"), serde_json::to_vec_pretty(&meta)?)?;
        }
        meta.reference = canon[0].clone();
        for c in &canon {
            writeln!(b.out, "Successfully tagged {c}")?;
        }
        writeln!(b.out, "Built in {:.1}s", t0.elapsed().as_secs_f64())?;
        Ok(meta)
    })();
    let _ = remove_tree(&dir);
    result
}

fn dir_size(p: &Path) -> u64 {
    std::process::Command::new("du")
        .arg("-sk")
        .arg(p)
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().next().and_then(|n| n.parse::<u64>().ok()))
        .map(|k| k * 1024)
        .unwrap_or(0)
}

/// Remove the build cache (snapshots and RUN --mount=type=cache directories).
pub fn prune_cache(store: &Store, mounts_too: bool) -> Result<u64> {
    let dir = store.data.join("build-cache");
    let mut n = 0;
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.flatten() {
            if e.file_name() == "mounts" && !mounts_too {
                continue;
            }
            remove_any(&e.path())?;
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_continuations_and_heredocs() {
        let df = "# syntax=docker/dockerfile:1\nFROM alpine AS b\n# c\nRUN echo a \\\n  # inner comment\n  && echo b\nRUN <<EOF\necho hi\nEOF\nCOPY a b /dst/\n";
        let (i, esc) = parse_dockerfile(df).unwrap();
        assert_eq!(esc, '\\');
        assert_eq!(i.len(), 4);
        assert_eq!(i[1].args, "echo a   && echo b");
        assert_eq!(i[2].heredocs, vec![("EOF".to_string(), "echo hi\n".to_string())]);
        assert_eq!(i[3].cmd, "COPY");
    }

    #[test]
    fn lexes_words_and_vars() {
        let lk = |k: &str| if k == "A" { Some("x y".to_string()) } else { None };
        assert_eq!(lex(r#"K="$A" L='$A' M=${A:-d} N=${B:-d} O=${A:+set}"#, '\\', Some(&lk)).unwrap(), vec!["K=x y", "L=$A", "M=x y", "N=d", "O=set"]);
        assert_eq!(lex(r"a\ b c", '\\', None).unwrap(), vec!["a b", "c"]);
    }

    #[test]
    fn dockerignore() {
        let ig = Ignore::parse("target/\n*.iml\n**/__pycache__/\n.env*\ndocs\n!docs/keep.md\n");
        assert!(ig.excluded("target"));
        assert!(ig.excluded("target/x.jar"));
        assert!(ig.excluded("a.iml"));
        assert!(!ig.excluded("src/a.iml"));
        assert!(ig.excluded("pkg/sub/__pycache__/m.pyc"));
        assert!(ig.excluded(".env.local"));
        assert!(ig.excluded("docs/a.md"));
        assert!(!ig.excluded("docs/keep.md"));
        assert!(!ig.excluded("src/main.rs"));
    }

    #[test]
    fn globs_and_durations() {
        assert!(glob_match("*.jar", "app.jar"));
        assert!(glob_match("a?c", "abc"));
        assert!(glob_match("[a-c]x", "bx"));
        assert!(!glob_match("[!a-c]x", "bx"));
        assert_eq!(parse_duration_ns("1m30s").unwrap(), 90_000_000_000);
        assert_eq!(parse_duration_ns("500ms").unwrap(), 500_000_000);
    }

    #[test]
    fn resolves_symlinks_inside_root() {
        let d = std::env::temp_dir().join(format!("abx-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("usr/bin")).unwrap();
        std::os::unix::fs::symlink("/usr/bin", d.join("bin")).unwrap();
        std::os::unix::fs::symlink("../../../../etc", d.join("usr/bin/esc")).unwrap();
        assert_eq!(resolve_in_root(&d, "/bin/sh").unwrap(), d.join("usr/bin/sh"));
        assert_eq!(resolve_in_root(&d, "/bin/esc/passwd").unwrap(), d.join("etc/passwd"));
        let _ = fs::remove_dir_all(&d);
    }
}
