//! `clipt update`: check GitHub for a newer release and, for installs that
//! clipt owns (install.sh into a user directory), replace the binary.
//!
//! Network access goes through the system `curl`, so clipt carries no TLS
//! stack of its own. Nothing here runs unless asked: the CLI command, the tray
//! menu item, or `check_updates = true` in the config.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

const API: &str = "https://api.github.com/repos";

#[derive(Deserialize, Debug, Clone)]
pub struct Release {
    pub tag_name: String,
    pub html_url: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}

impl Release {
    pub fn version(&self) -> &str {
        self.tag_name.trim_start_matches('v')
    }
}

/// How this copy of clipt was installed, which decides who updates it.
#[derive(Debug, PartialEq, Eq)]
pub enum Install {
    /// install.sh or a copied binary in a directory we can write: we update.
    SelfManaged(PathBuf),
    Cargo,
    Nix,
    /// Flatpak: Flathub or the user's software centre updates it.
    Flatpak,
    /// /usr/bin and friends: the distro package manager owns it.
    Package,
}

impl Install {
    pub fn detect() -> Self {
        let exe = current_exe();
        let s = exe.to_string_lossy();
        if flatpak() {
            Install::Flatpak
        } else if s.starts_with("/nix/store/") {
            Install::Nix
        } else if s.contains("/.cargo/bin/") {
            Install::Cargo
        } else if s.starts_with("/usr/") || s.starts_with("/bin/") || s.starts_with("/opt/") {
            Install::Package
        } else if exe.parent().is_some_and(writable) {
            Install::SelfManaged(exe)
        } else {
            Install::Package
        }
    }

    /// What to tell someone whose clipt we can't replace ourselves.
    pub fn how_to_update(&self) -> &'static str {
        match self {
            Install::SelfManaged(_) => "run `clipt update`",
            Install::Cargo => "run `cargo install clipt`",
            Install::Nix => "update your flake input",
            Install::Flatpak => "run `flatpak update` or use your software centre",
            Install::Package => "update it with your package manager",
        }
    }
}

/// Running inside a Flatpak sandbox. Flatpak puts this file in every sandbox.
pub fn flatpak() -> bool {
    Path::new("/.flatpak-info").exists()
}

/// Our binary's path. After an upgrade replaces the file, Linux reports the
/// running image as "… (deleted)"; the new binary is at the original path.
pub fn current_exe() -> PathBuf {
    match std::env::current_exe() {
        Ok(p) => {
            let s = p.to_string_lossy();
            match s.strip_suffix(" (deleted)") {
                Some(orig) => orig.into(),
                None => p,
            }
        }
        Err(_) => "clipt".into(),
    }
}

fn writable(dir: &Path) -> bool {
    rustix::fs::access(dir, rustix::fs::Access::WRITE_OK).is_ok()
}

fn repo() -> &'static str {
    env!("CARGO_PKG_REPOSITORY").trim_start_matches("https://github.com/").trim_end_matches('/')
}

fn curl(args: &[&str]) -> Result<Vec<u8>> {
    let ua = concat!("clipt/", env!("CARGO_PKG_VERSION"));
    let out = Command::new("curl")
        .args(["-fsSL", "--proto", "=https", "--max-time", "60", "-A", ua])
        .args(args)
        .output()
        .context("running curl (is it installed?)")?;
    if !out.status.success() {
        bail!("download failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// The newest published release (drafts and prereleases excluded by GitHub).
pub fn latest() -> Result<Release> {
    let url = format!("{API}/{}/releases/latest", repo());
    let body = curl(&["-H", "Accept: application/vnd.github+json", &url]).map_err(|e| {
        // GitHub answers 404 until the first release is published.
        if format!("{e}").contains("404") { anyhow::anyhow!("no releases have been published yet") } else { e }
    })?;
    serde_json::from_slice(&body).context("unexpected reply from GitHub")
}

/// `Some(release)` when it's newer than this build.
pub fn check() -> Result<Option<Release>> {
    let r = latest()?;
    Ok(newer(r.version(), env!("CARGO_PKG_VERSION")).then_some(r))
}

/// Semantic-version comparison of "x.y.z[-pre]": a prerelease sorts before
/// its release.
pub fn newer(candidate: &str, current: &str) -> bool {
    fn parse(v: &str) -> Option<([u64; 3], Option<&str>)> {
        let (core, pre) = match v.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (v, None),
        };
        let mut it = core.split('.').map(|n| n.parse::<u64>().ok());
        let n = [it.next()??, it.next().flatten().unwrap_or(0), it.next().flatten().unwrap_or(0)];
        Some((n, pre))
    }
    let (Some((a, ap)), Some((b, bp))) = (parse(candidate), parse(current)) else { return false };
    match a.cmp(&b) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => match (ap, bp) {
            (None, Some(_)) => true,
            (Some(x), Some(y)) => x > y,
            _ => false,
        },
    }
}

fn arch() -> &'static str {
    std::env::consts::ARCH
}

/// Download `r`'s tarball for this machine, verify its checksum and swap
/// the binary at `exe` for the new one. Returns the new binary's path.
pub fn install(r: &Release, exe: &Path) -> Result<PathBuf> {
    let name = format!("clipt-{}-{}-linux.tar.gz", r.tag_name, arch());
    let find = |n: &str| r.assets.iter().find(|a| a.name == n).map(|a| a.browser_download_url.clone());
    let url = find(&name).with_context(|| format!("release {} has no build for {}", r.tag_name, arch()))?;
    let sum_url = find(&format!("{name}.sha256")).context("release has no checksum file")?;

    let dir = exe.parent().context("binary has no parent directory")?;
    let work = tempdir(dir)?;
    let result = (|| -> Result<PathBuf> {
        let tarball = work.join(&name);
        std::fs::write(&tarball, curl(&[&url])?)?;
        let want = String::from_utf8(curl(&[&sum_url])?)?;
        let want = want.split_whitespace().next().context("empty checksum file")?.to_lowercase();
        let got = sha256(&tarball)?;
        anyhow::ensure!(got == want, "checksum mismatch: expected {want}, got {got}");

        let st = Command::new("tar").arg("xzf").arg(&tarball).arg("-C").arg(&work).status().context("running tar")?;
        anyhow::ensure!(st.success(), "couldn't unpack {name}");
        let new = work.join(name.trim_end_matches(".tar.gz")).join("clipt");
        anyhow::ensure!(new.is_file(), "tarball has no clipt binary");
        let out = Command::new(&new).arg("--version").output().context("running the new binary")?;
        anyhow::ensure!(out.status.success(), "the new binary doesn't run on this system");

        // Same directory, so the rename is atomic: running copies keep their
        // old image and new launches get the new one.
        let staged = dir.join(".clipt.new");
        std::fs::copy(&new, &staged)?;
        std::fs::set_permissions(&staged, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
        std::fs::rename(&staged, exe)?;
        Ok(exe.to_path_buf())
    })();
    let _ = std::fs::remove_dir_all(&work);
    result
}

fn tempdir(near: &Path) -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| near.to_path_buf());
    let d = base.join(format!("clipt/update-{}", std::process::id()));
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

fn sha256(p: &Path) -> Result<String> {
    let out = Command::new("sha256sum").arg(p).output().context("running sha256sum")?;
    anyhow::ensure!(out.status.success(), "sha256sum failed");
    Ok(String::from_utf8(out.stdout)?.split_whitespace().next().unwrap_or_default().to_lowercase())
}

/// `clipt update [--check]`.
pub fn cli(check_only: bool) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    if flatpak() {
        bail!("this clipt is a Flatpak: {}", Install::Flatpak.how_to_update());
    }
    let Some(r) = check()? else {
        println!("clipt {current} is up to date");
        return Ok(());
    };
    println!("clipt {} is available (you have {current}): {}", r.version(), r.html_url);
    if check_only {
        return Ok(());
    }
    match Install::detect() {
        Install::SelfManaged(exe) => {
            println!("updating {} …", exe.display());
            install(&r, &exe)?;
            println!("updated to {}; restart `clipt tray` if it's running", r.version());
            Ok(())
        }
        other => bail!("this clipt is managed elsewhere: {}", other.how_to_update()),
    }
}

#[cfg(test)]
mod tests {
    use super::newer;

    #[test]
    fn version_order() {
        assert!(newer("0.2.0", "0.1.0"));
        assert!(newer("1.0.0", "0.9.9"));
        assert!(newer("0.1.1", "0.1.0"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.2.0"));
        assert!(newer("0.2.0", "0.2.0-rc1"));
        assert!(!newer("0.2.0-rc1", "0.2.0"));
        assert!(newer("0.2.0-rc2", "0.2.0-rc1"));
        assert!(!newer("garbage", "0.1.0"));
    }
}
