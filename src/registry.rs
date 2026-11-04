//! Minimal OCI / Docker Registry v2 client: anonymous bearer-token auth, multi-arch
//! index resolution, digest-verified blob downloads, and layer extraction with
//! whiteout handling.

use crate::store::{remove_tree, ImageMeta, Store};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
application/vnd.docker.distribution.manifest.list.v2+json, \
application/vnd.oci.image.manifest.v1+json, \
application/vnd.docker.distribution.manifest.v2+json";

#[derive(Debug, Clone, PartialEq)]
pub struct Reference {
    pub registry: String,
    pub repo: String,
    /// tag or `sha256:` digest
    pub tag: String,
}

impl Reference {
    pub fn parse(s: &str) -> Result<Reference> {
        let (rest, tag) = if let Some((r, d)) = s.split_once('@') {
            (r.to_string(), d.to_string())
        } else {
            // a ':' after the last '/' is a tag, otherwise it's a registry port
            match s.rfind(':') {
                Some(i) if !s[i..].contains('/') => (s[..i].to_string(), s[i + 1..].to_string()),
                _ => (s.to_string(), "latest".to_string()),
            }
        };
        let (registry, repo) = match rest.split_once('/') {
            Some((first, path)) if first.contains('.') || first.contains(':') || first == "localhost" => {
                (first.to_string(), path.to_string())
            }
            _ => ("docker.io".to_string(), rest.clone()),
        };
        let repo = if registry == "docker.io" && !repo.contains('/') { format!("library/{repo}") } else { repo };
        if repo.is_empty() || tag.is_empty() {
            bail!("invalid image reference: {s}");
        }
        Ok(Reference { registry, repo, tag })
    }

    /// Canonical short form used as the store key: `alpine:3.20`, `ghcr.io/x/y:tag`.
    pub fn canonical(&self) -> String {
        let repo = if self.registry == "docker.io" {
            self.repo.strip_prefix("library/").unwrap_or(&self.repo).to_string()
        } else {
            format!("{}/{}", self.registry, self.repo)
        };
        let sep = if self.tag.starts_with("sha256:") { '@' } else { ':' };
        format!("{repo}{sep}{}", self.tag)
    }

    fn api_host(&self) -> &str {
        if self.registry == "docker.io" { "registry-1.docker.io" } else { &self.registry }
    }
}

struct Client {
    agent: ureq::Agent,
    reference: Reference,
    token: Option<String>,
}

impl Client {
    fn url(&self, path: &str) -> String {
        let scheme = if self.reference.registry.starts_with("localhost") { "http" } else { "https" };
        format!("{scheme}://{}/v2/{}/{path}", self.reference.api_host(), self.reference.repo)
    }

    /// GET with one retry after fetching a bearer token on 401.
    fn get(&mut self, path: &str, accept: &str) -> Result<ureq::Response> {
        for attempt in 0..2 {
            let mut req = self.agent.get(&self.url(path)).set("Accept", accept);
            if let Some(t) = &self.token {
                req = req.set("Authorization", &format!("Bearer {t}"));
            }
            match req.call() {
                Ok(r) => return Ok(r),
                Err(ureq::Error::Status(401, r)) if attempt == 0 => {
                    let challenge = r.header("www-authenticate").unwrap_or_default().to_string();
                    self.token = Some(self.fetch_token(&challenge)?);
                }
                Err(ureq::Error::Status(code, r)) => {
                    let body = r.into_string().unwrap_or_default();
                    bail!("registry returned {code} for {path}: {}", body.chars().take(300).collect::<String>())
                }
                Err(e) => return Err(e.into()),
            }
        }
        unreachable!()
    }

    fn fetch_token(&self, challenge: &str) -> Result<String> {
        let params = parse_challenge(challenge);
        let realm = params.get("realm").ok_or_else(|| anyhow!("registry auth challenge has no realm: {challenge}"))?;
        let scope = params.get("scope").cloned().unwrap_or_else(|| format!("repository:{}:pull", self.reference.repo));
        let mut req = self.agent.get(realm).query("scope", &scope);
        if let Some(s) = params.get("service") {
            req = req.query("service", s);
        }
        let v: Value = req.call().context("fetching registry token")?.into_json()?;
        v.get("token")
            .or_else(|| v.get("access_token"))
            .and_then(|t| t.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow!("no token in registry auth response"))
    }

    fn manifest(&mut self, reference: &str) -> Result<(Value, String)> {
        let r = self.get(&format!("manifests/{reference}"), ACCEPT)?;
        let ct = r.content_type().to_string();
        let mut body = vec![];
        r.into_reader().take(16 << 20).read_to_end(&mut body)?;
        let digest = format!("sha256:{:x}", Sha256::digest(&body));
        let mut v: Value = serde_json::from_slice(&body)?;
        if v.get("mediaType").is_none() {
            v["mediaType"] = Value::String(ct);
        }
        Ok((v, digest))
    }

    /// Download a blob into the cache (if missing) and verify its digest.
    fn blob(&mut self, store: &Store, digest: &str, quiet: bool) -> Result<PathBuf> {
        let dst = store.blob_path(digest);
        if dst.exists() {
            return Ok(dst);
        }
        let want = digest.strip_prefix("sha256:").ok_or_else(|| anyhow!("unsupported digest {digest}"))?;
        let r = self.get(&format!("blobs/{digest}"), "*/*")?;
        let tmp = dst.with_extension("partial");
        let mut f = File::create(&tmp)?;
        let mut hasher = Sha256::new();
        let mut reader = r.into_reader();
        let mut buf = vec![0u8; 1 << 16];
        let mut total = 0u64;
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            f.write_all(&buf[..n])?;
            total += n as u64;
        }
        let got = format!("{:x}", hasher.finalize());
        if got != want {
            let _ = fs::remove_file(&tmp);
            bail!("digest mismatch for {digest}: got sha256:{got}");
        }
        fs::rename(&tmp, &dst)?;
        if !quiet {
            eprintln!("  {} {:.1} MB", &want[..12], total as f64 / 1e6);
        }
        Ok(dst)
    }
}

fn parse_challenge(h: &str) -> HashMap<String, String> {
    let rest = h.trim().strip_prefix("Bearer ").unwrap_or(h);
    let mut out = HashMap::new();
    for part in rest.split(',') {
        if let Some((k, v)) = part.split_once('=') {
            out.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
        }
    }
    out
}

fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}

/// Pull `image` into the store and return its metadata.
pub fn pull(store: &Store, image: &str, quiet: bool) -> Result<ImageMeta> {
    let reference = Reference::parse(image)?;
    let canonical = reference.canonical();
    let agent = ureq::AgentBuilder::new().user_agent("agentbox/0.1").build();
    let mut c = Client { agent, reference: reference.clone(), token: None };

    let (mut manifest, mut digest) = c.manifest(&reference.tag)?;
    let mt = manifest["mediaType"].as_str().unwrap_or_default().to_string();
    if mt.contains("index") || mt.contains("manifest.list") || manifest.get("manifests").is_some() {
        let arch = host_arch();
        let pick = manifest["manifests"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|m| m["platform"]["os"] == "linux" && m["platform"]["architecture"] == arch)
            .ok_or_else(|| anyhow!("{canonical} has no linux/{arch} image"))?;
        let d = pick["digest"].as_str().ok_or_else(|| anyhow!("index entry without digest"))?.to_string();
        (manifest, digest) = c.manifest(&d)?;
    }

    if let Some(existing) = store.image(&canonical)? {
        if existing.digest == digest {
            if !quiet {
                eprintln!("{canonical} is up to date ({})", &digest[..19]);
            }
            return Ok(existing);
        }
    }
    if !quiet {
        eprintln!("pulling {canonical} ({})", &digest[..19]);
    }

    let config_digest = manifest["config"]["digest"].as_str().ok_or_else(|| anyhow!("manifest has no config"))?;
    let config: Value = serde_json::from_slice(&fs::read(c.blob(store, config_digest, true)?)?)?;
    let mut layers = vec![];
    for l in manifest["layers"].as_array().ok_or_else(|| anyhow!("manifest has no layers"))? {
        let d = l["digest"].as_str().unwrap_or_default();
        layers.push((c.blob(store, d, quiet)?, l["mediaType"].as_str().unwrap_or_default().to_string()));
    }
    install(store, &canonical, digest, &config, &layers)
}

/// Import images from a `docker save` archive (legacy or OCI layout; both carry a
/// top-level manifest.json). `-` reads the archive from stdin. `tag` renames a
/// single-image archive.
pub fn load(store: &Store, archive: &str, tag: Option<&str>) -> Result<Vec<ImageMeta>> {
    let tmp = store.data.join(format!(".load-{}", std::process::id()));
    if tmp.exists() {
        remove_tree(&tmp)?;
    }
    fs::create_dir_all(&tmp)?;
    let result = (|| {
        let input: Box<dyn Read> =
            if archive == "-" { Box::new(io::stdin().lock()) } else { Box::new(BufReader::new(File::open(archive)?)) };
        let mut ar = tar::Archive::new(input);
        for entry in ar.entries()? {
            entry?.unpack_in(&tmp)?; // unpack_in refuses paths escaping tmp
        }
        let manifest: Value = serde_json::from_slice(&fs::read(tmp.join("manifest.json")).context("archive has no manifest.json; is it a `docker save` tarball?")?)?;
        let entries = manifest.as_array().ok_or_else(|| anyhow!("manifest.json is not a list"))?;
        if tag.is_some() && entries.len() != 1 {
            bail!("--tag needs an archive with exactly one image ({} found)", entries.len());
        }
        let mut out = vec![];
        for e in entries {
            let name = match tag {
                Some(t) => t.to_string(),
                None => e["RepoTags"][0].as_str().ok_or_else(|| anyhow!("image without a tag in archive; pass --tag"))?.to_string(),
            };
            let canonical = Reference::parse(&name)?.canonical();
            let config_path = tmp.join(e["Config"].as_str().ok_or_else(|| anyhow!("manifest entry without Config"))?);
            let config_bytes = fs::read(&config_path)?;
            let digest = format!("sha256:{:x}", Sha256::digest(&config_bytes));
            let config: Value = serde_json::from_slice(&config_bytes)?;
            let layers: Vec<(PathBuf, String)> = e["Layers"]
                .as_array()
                .ok_or_else(|| anyhow!("manifest entry without Layers"))?
                .iter()
                .map(|l| (tmp.join(l.as_str().unwrap_or_default()), String::new()))
                .collect();
            eprintln!("loading {canonical} ({} layers)", layers.len());
            out.push(install(store, &canonical, digest, &config, &layers)?);
        }
        Ok(out)
    })();
    let _ = remove_tree(&tmp);
    result
}

/// Build an image's rootfs from its layer files and record its metadata. Extracts into a
/// staging dir and swaps it in, so a failure never leaves a half-built image behind.
fn install(store: &Store, canonical: &str, digest: String, config: &Value, layers: &[(PathBuf, String)]) -> Result<ImageMeta> {
    let image_dir = store.image_dir(canonical);
    let staging = image_dir.with_extension("staging");
    if staging.exists() {
        remove_tree(&staging)?;
    }
    let rootfs = staging.join("rootfs");
    fs::create_dir_all(&rootfs)?;
    let mut size = 0;
    for (blob, media) in layers {
        size += fs::metadata(blob)?.len();
        extract_layer(blob, media, &rootfs).with_context(|| format!("extracting layer {}", blob.display()))?;
    }

    let cfg = &config["config"];
    let strings = |v: &Value| -> Vec<String> {
        v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default()
    };
    let meta = ImageMeta {
        reference: canonical.to_string(),
        digest,
        env: strings(&cfg["Env"]),
        entrypoint: strings(&cfg["Entrypoint"]),
        cmd: strings(&cfg["Cmd"]),
        workdir: cfg["WorkingDir"].as_str().unwrap_or_default().to_string(),
        user: cfg["User"].as_str().unwrap_or_default().to_string(),
        size_bytes: size,
    };
    fs::write(staging.join("image.json"), serde_json::to_vec_pretty(&meta)?)?;
    if image_dir.exists() {
        remove_tree(&image_dir)?;
    }
    fs::rename(&staging, &image_dir)?;
    Ok(meta)
}

/// Apply one layer tarball on top of `rootfs`, honouring OCI whiteouts.
fn extract_layer(blob: &Path, media_type: &str, rootfs: &Path) -> Result<()> {
    let mut f = BufReader::new(File::open(blob)?);
    let mut magic = [0u8; 4];
    let n = f.read(&mut magic)?;
    let head = io::Cursor::new(magic[..n].to_vec()).chain(f);
    let reader: Box<dyn Read> = if magic[..2] == [0x1f, 0x8b] {
        Box::new(flate2::read::GzDecoder::new(head))
    } else if magic == [0x28, 0xb5, 0x2f, 0xfd] {
        bail!("zstd-compressed layers ({media_type}) are not supported yet");
    } else {
        Box::new(head)
    };

    let mut ar = tar::Archive::new(reader);
    ar.set_preserve_permissions(true);
    ar.set_preserve_mtime(true);
    ar.set_unpack_xattrs(false);
    ar.set_overwrite(true);

    for entry in ar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        // Refuse anything that tries to climb out of the rootfs.
        if path.components().any(|c| matches!(c, Component::ParentDir | Component::Prefix(_))) {
            continue;
        }
        let rel: PathBuf = path.components().filter(|c| matches!(c, Component::Normal(_))).collect();
        let name = rel.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        let parent = rel.parent().map(|p| rootfs.join(p)).unwrap_or_else(|| rootfs.to_path_buf());

        if name == ".wh..wh..opq" {
            // Opaque dir: drop everything lower layers put here.
            if let Ok(rd) = fs::read_dir(&parent) {
                for e in rd.flatten() {
                    remove_any(&e.path())?;
                }
            }
            continue;
        }
        if let Some(target) = name.strip_prefix(".wh.") {
            remove_any(&parent.join(target))?;
            continue;
        }

        let dst = rootfs.join(&rel);
        let is_dir = entry.header().entry_type().is_dir();
        if let Ok(md) = fs::symlink_metadata(&dst) {
            // Never write through an existing symlink, and replace type changes outright.
            if !is_dir || !md.is_dir() {
                remove_any(&dst)?;
            }
        }
        if !entry.unpack_in(rootfs)? {
            continue;
        }
        // The host user (not root) owns these files; keep them writable/traversable for it so
        // later layers, the guest, and cleanup can all modify the tree.
        if let Ok(md) = fs::symlink_metadata(&dst) {
            if !md.file_type().is_symlink() {
                let mode = md.permissions().mode();
                let want = if md.is_dir() { mode | 0o700 } else { mode | 0o600 };
                if want != mode {
                    fs::set_permissions(&dst, fs::Permissions::from_mode(want))?;
                }
            }
        }
    }
    Ok(())
}

fn remove_any(p: &Path) -> Result<()> {
    match fs::symlink_metadata(p) {
        Ok(md) if md.is_dir() => remove_tree(p),
        Ok(_) => Ok(fs::remove_file(p)?),
        Err(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::Reference;

    fn r(s: &str) -> (String, String, String) {
        let x = Reference::parse(s).unwrap();
        (x.registry, x.repo, x.tag)
    }

    #[test]
    fn parses_references() {
        assert_eq!(r("alpine"), ("docker.io".into(), "library/alpine".into(), "latest".into()));
        assert_eq!(r("python:3.12-slim"), ("docker.io".into(), "library/python".into(), "3.12-slim".into()));
        assert_eq!(r("user/app:v1"), ("docker.io".into(), "user/app".into(), "v1".into()));
        assert_eq!(r("ghcr.io/org/app"), ("ghcr.io".into(), "org/app".into(), "latest".into()));
        assert_eq!(r("localhost:5000/app:dev"), ("localhost:5000".into(), "app".into(), "dev".into()));
        assert_eq!(r("alpine@sha256:abc"), ("docker.io".into(), "library/alpine".into(), "sha256:abc".into()));
        assert_eq!(Reference::parse("library/alpine:3").unwrap().canonical(), "alpine:3");
        assert_eq!(Reference::parse("ghcr.io/o/a").unwrap().canonical(), "ghcr.io/o/a:latest");
    }
}
