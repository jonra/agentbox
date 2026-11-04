//! On-disk layout:
//!
//! ```text
//! ~/.agentbox/
//!   store.sparsebundle      (macOS only) case-sensitive APFS image holding `data/`
//!   data/                   mount point (or a plain dir on Linux / AGENTBOX_NO_VOLUME=1)
//!     blobs/sha256/<hex>    cached layer/config blobs
//!     images/<key>/         image.json + rootfs/
//!     containers/<id>/      see container.rs
//!     volumes/<name>/       named volumes (`-v name:/path`)
//! ```
//!
//! Linux root filesystems contain paths that differ only by case (terminfo, perl, ...),
//! which the default macOS volume can't hold, hence the case-sensitive sparse bundle.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone)]
pub struct Store {
    pub data: PathBuf,
}

/// What we keep from an image's config: enough to run it like Docker would.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ImageMeta {
    pub reference: String,
    pub digest: String,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub entrypoint: Vec<String>,
    #[serde(default)]
    pub cmd: Vec<String>,
    #[serde(default)]
    pub workdir: String,
    #[serde(default)]
    pub user: String,
    pub size_bytes: u64,
}

impl Store {
    pub fn open() -> Result<Store> {
        let home = match std::env::var_os("AGENTBOX_HOME") {
            Some(h) => PathBuf::from(h),
            None => PathBuf::from(std::env::var_os("HOME").context("HOME not set")?).join(".agentbox"),
        };
        fs::create_dir_all(&home)?;
        let data = home.join("data");
        let marker = data.join(".agentbox-store");
        if !marker.exists() {
            if cfg!(target_os = "macos") && std::env::var_os("AGENTBOX_NO_VOLUME").is_none() {
                mount_volume(&home, &data)?;
            } else {
                fs::create_dir_all(&data)?;
            }
            fs::write(&marker, b"agentbox store\n")?;
        }
        for d in ["blobs/sha256", "images", "containers", "volumes"] {
            fs::create_dir_all(data.join(d))?;
        }
        Ok(Store { data })
    }

    pub fn blob_path(&self, digest: &str) -> PathBuf {
        let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
        self.data.join("blobs/sha256").join(hex)
    }

    pub fn image_dir(&self, reference: &str) -> PathBuf {
        self.data.join("images").join(key(reference))
    }

    pub fn container_dir(&self, id: &str) -> PathBuf {
        self.data.join("containers").join(key(id))
    }

    pub fn volume_dir(&self, name: &str) -> Result<PathBuf> {
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c)) || name.starts_with('.') {
            bail!("invalid volume name {name:?}");
        }
        Ok(self.data.join("volumes").join(name))
    }

    pub fn volumes(&self) -> Result<Vec<String>> {
        let mut out: Vec<String> = fs::read_dir(self.data.join("volumes"))?
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| !n.starts_with('.'))
            .collect();
        out.sort();
        Ok(out)
    }

    pub fn image(&self, reference: &str) -> Result<Option<ImageMeta>> {
        let p = self.image_dir(reference).join("image.json");
        if !p.exists() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&fs::read(p)?)?))
    }

    pub fn images(&self) -> Result<Vec<ImageMeta>> {
        let mut out = vec![];
        for e in fs::read_dir(self.data.join("images"))? {
            let p = e?.path().join("image.json");
            if let Ok(b) = fs::read(&p) {
                out.push(serde_json::from_slice(&b)?);
            }
        }
        out.sort_by(|a: &ImageMeta, b| a.reference.cmp(&b.reference));
        Ok(out)
    }

    pub fn remove_image(&self, reference: &str) -> Result<()> {
        let d = self.image_dir(reference);
        if !d.exists() {
            bail!("no such image: {reference}");
        }
        remove_tree(&d)
    }
}

/// Filesystem-safe key for an image reference or container name.
pub fn key(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn mount_volume(home: &Path, data: &Path) -> Result<()> {
    let bundle = home.join("store.sparsebundle");
    if !bundle.exists() {
        eprintln!("agentbox: creating case-sensitive store at {}", bundle.display());
        run(Command::new("hdiutil").args(["create", "-quiet", "-size", "64g", "-type", "SPARSEBUNDLE"])
            .args(["-fs", "Case-sensitive APFS", "-volname", "agentbox"]).arg(&bundle))?;
    }
    fs::create_dir_all(data)?;
    run(Command::new("hdiutil").args(["attach", "-quiet", "-nobrowse", "-owners", "on", "-mountpoint"])
        .arg(data).arg(&bundle))
    .context("mounting the agentbox store volume")
}

fn run(cmd: &mut Command) -> Result<()> {
    let st = cmd.status().with_context(|| format!("running {cmd:?}"))?;
    if !st.success() {
        bail!("{cmd:?} failed: {st}");
    }
    Ok(())
}

/// Copy-on-write clone of a directory tree (APFS clonefile / Linux reflink).
pub fn clone_tree(src: &Path, dst: &Path) -> Result<()> {
    if let Some(p) = dst.parent() {
        fs::create_dir_all(p)?;
    }
    let mut cmd = Command::new("cp");
    if cfg!(target_os = "macos") {
        cmd.arg("-cRp");
    } else {
        cmd.args(["-a", "--reflink=auto"]);
    }
    run(cmd.arg(src).arg(dst))
}

/// rm -rf that first restores owner write permission, since image trees contain
/// read-only directories.
pub fn remove_tree(p: &Path) -> Result<()> {
    let _ = Command::new("chmod").args(["-R", "u+rwX"]).arg(p).status();
    fs::remove_dir_all(p).with_context(|| format!("removing {}", p.display()))
}
