//! `clipt update`: check GitHub for a newer release and, for installs that
//! clipt owns (install.sh into a user directory, or an unpacked release
//! zip or tarball), replace the binary.
//!
//! Network access goes through the system `curl`, so clipt carries no TLS
//! stack of its own; Windows 10 and 11 ship `curl.exe` and `tar.exe` too. Nothing here runs unless asked: the CLI command, the tray
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
    /// The binary inside clipt.app: replacing it alone would break the
    /// bundle's signature, so a new app comes from the release page.
    AppBundle,
    /// /usr/bin and friends: the distro package manager owns it. On
    /// Windows, a folder we can't write, such as Program Files.
    Package,
}

impl Install {
    pub fn detect() -> Self {
        let exe = current_exe();
        let s = exe.to_string_lossy();
        if flatpak() {
            Install::Flatpak
        } else if s.contains(".app/Contents/MacOS/") {
            Install::AppBundle
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
            Install::AppBundle => "download the new clipt.app from the release page",
            Install::Package if cfg!(windows) => "download the new zip from the release page",
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
    // Trying is the only test that works everywhere: Windows' ACLs and
    // macOS's sandbox both answer differently from the permission bits.
    let probe = dir.join(format!(".clipt-probe-{}", std::process::id()));
    let ok = std::fs::File::create(&probe).is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// A system tool, by its full path on Windows: Git for Windows and MSYS put
/// GNU versions on PATH, and GNU tar can't unpack a zip.
fn tool(name: &str) -> PathBuf {
    match std::env::var_os("SystemRoot") {
        Some(root) if cfg!(windows) => Path::new(&root).join("System32").join(format!("{name}.exe")),
        _ => name.into(),
    }
}

fn repo() -> &'static str {
    env!("CARGO_PKG_REPOSITORY").trim_start_matches("https://github.com/").trim_end_matches('/')
}

fn curl(args: &[&str]) -> Result<Vec<u8>> {
    let ua = concat!("clipt/", env!("CARGO_PKG_VERSION"));
    let out = Command::new(tool("curl"))
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
    // CI's runners share their address, and with it GitHub's limit on
    // anonymous requests; CI hands over its token. Only for this request,
    // and only under this name, so a GITHUB_TOKEN set for other tools
    // isn't sent.
    let auth = std::env::var("CLIPT_GITHUB_TOKEN").ok().map(|t| format!("Authorization: Bearer {t}"));
    let mut args = vec!["-H", "Accept: application/vnd.github+json"];
    if let Some(a) = &auth {
        args.extend(["-H", a]);
    }
    args.push(&url);
    let body = curl(&args).map_err(|e| {
        // GitHub answers 404 until the first release is published.
        if format!("{e}").contains("404") { anyhow::anyhow!("no releases have been published yet") } else { e }
    })?;
    serde_json::from_slice(&body).context("unexpected reply from GitHub")
}

/// This build's version. CI sets `CLIPT_UPDATE_AS=0.0.0` to have a fresh
/// build update itself from the latest release, testing the whole path.
fn current_version() -> String {
    std::env::var("CLIPT_UPDATE_AS").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").into())
}

/// `Some(release)` when it's newer than this build.
pub fn check() -> Result<Option<Release>> {
    let r = latest()?;
    Ok(newer(r.version(), &current_version()).then_some(r))
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

/// The release archive for this machine, without its extension: one per
/// architecture on Linux, one universal binary for both kinds of Mac, and
/// x86_64 for Windows (which runs it on ARM too, emulated).
fn platform() -> String {
    if cfg!(target_os = "macos") {
        "universal-macos".into()
    } else if cfg!(windows) {
        "x86_64-windows".into()
    } else {
        format!("{}-linux", std::env::consts::ARCH)
    }
}

const ARCHIVE: &str = if cfg!(windows) { ".zip" } else { ".tar.gz" };
const BIN: &str = if cfg!(windows) { "clipt.exe" } else { "clipt" };

/// Download `r`'s archive for this machine, verify its checksum and swap
/// the binary at `exe` for the new one. Returns the new binary's path.
pub fn install(r: &Release, exe: &Path) -> Result<PathBuf> {
    let name = format!("clipt-{}-{}{ARCHIVE}", r.tag_name, platform());
    let find = |n: &str| r.assets.iter().find(|a| a.name == n).map(|a| a.browser_download_url.clone());
    let url = find(&name).with_context(|| format!("release {} has no build for {}", r.tag_name, platform()))?;
    let sum_url = find(&format!("{name}.sha256")).context("release has no checksum file")?;

    let dir = exe.parent().context("binary has no parent directory")?;
    let work = tempdir(dir)?;
    let result = (|| -> Result<PathBuf> {
        let archive = work.join(&name);
        std::fs::write(&archive, curl(&[&url])?)?;
        let want = String::from_utf8(curl(&[&sum_url])?)?;
        let want = want.split_whitespace().next().context("empty checksum file")?.to_lowercase();
        let got = sha256(&archive)?;
        anyhow::ensure!(got == want, "checksum mismatch: expected {want}, got {got}");

        // bsdtar (macOS, and Windows' tar.exe) reads zips as well as tarballs.
        let st = Command::new(tool("tar")).arg("-xf").arg(&archive).arg("-C").arg(&work).status().context("running tar")?;
        anyhow::ensure!(st.success(), "couldn't unpack {name}");
        let new = work.join(name.trim_end_matches(ARCHIVE)).join(BIN);
        anyhow::ensure!(new.is_file(), "{name} has no {BIN}");
        let out = Command::new(&new).arg("--version").output().context("running the new binary")?;
        anyhow::ensure!(out.status.success(), "the new binary doesn't run on this system");
        replace(&new, exe, dir)?;
        Ok(exe.to_path_buf())
    })();
    let _ = std::fs::remove_dir_all(&work);
    result
}

/// Put `new` at `exe`. Same directory, so the rename is atomic: running
/// copies keep their old image and new launches get the new one.
#[cfg(unix)]
fn replace(new: &Path, exe: &Path, dir: &Path) -> Result<()> {
    let staged = dir.join(".clipt.new");
    std::fs::copy(new, &staged)?;
    std::fs::set_permissions(&staged, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    std::fs::rename(&staged, exe)?;
    Ok(())
}

/// Windows won't overwrite or delete a running exe, and this one is
/// running, but it will rename it. So: move it aside, put the new one in
/// its place, and let the next start delete the old one ([`cleanup`]).
#[cfg(windows)]
fn replace(new: &Path, exe: &Path, dir: &Path) -> Result<()> {
    let staged = dir.join("clipt.exe.new");
    std::fs::copy(new, &staged)?;
    cleanup();
    // A tray or editor from before an earlier update may still hold the
    // usual name; then pick one that's free.
    let old = (0..100)
        .map(|i| dir.join(if i == 0 { "clipt.exe.old".into() } else { format!("clipt.exe.old{i}") }))
        .find(|p| !p.exists())
        .context("too many old copies of clipt.exe")?;
    std::fs::rename(exe, &old).context("moving the running clipt.exe aside")?;
    if let Err(e) = std::fs::rename(&staged, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(e).context("putting the new clipt.exe in place");
    }
    Ok(())
}

/// Delete what an update moved aside, once nothing runs it. Called at
/// every start on Windows; a copy still running stays until next time.
#[cfg(windows)]
pub fn cleanup() {
    let exe = current_exe();
    let Some(dir) = exe.parent() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        if e.file_name().to_string_lossy().starts_with("clipt.exe.old") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn tempdir(near: &Path) -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| near.to_path_buf());
    let d = base.join(format!("clipt/update-{}", std::process::id()));
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

fn sha256(p: &Path) -> Result<String> {
    if cfg!(windows) {
        // certutil prints a heading line, then the hash.
        let out = Command::new(tool("certutil")).arg("-hashfile").arg(p).arg("SHA256").output().context("running certutil")?;
        anyhow::ensure!(out.status.success(), "certutil failed");
        let text = String::from_utf8_lossy(&out.stdout);
        let hash = text.lines().nth(1).unwrap_or_default().replace(' ', "");
        return Ok(hash.trim().to_lowercase());
    }
    // macOS has no sha256sum; its shasum prints the same format.
    let (cmd, args): (&str, &[&str]) = if cfg!(target_os = "macos") { ("shasum", &["-a", "256"]) } else { ("sha256sum", &[]) };
    let out = Command::new(cmd).args(args).arg(p).output().with_context(|| format!("running {cmd}"))?;
    anyhow::ensure!(out.status.success(), "{cmd} failed");
    Ok(String::from_utf8(out.stdout)?.split_whitespace().next().unwrap_or_default().to_lowercase())
}

/// `clipt update [--check]`.
pub fn cli(check_only: bool) -> Result<()> {
    let current = current_version();
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
            let tray = if cfg!(target_os = "macos") { "the menu bar icon" } else { "`clipt tray`" };
            println!("updated to {}; restart {tray} if it's running", r.version());
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
