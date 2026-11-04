//! Full-page browser screenshots. clipt's Chrome/Edge extension
//! (extensions/chromium/) captures the whole page through the DevTools
//! protocol and sends the PNG to clipt, which the browser starts as its
//! native messaging host: `clipt chrome-extension://<id>/`. One message in
//! (the PNG and what to do with it), one reply out.
//!
//! `clipt browser-extension` registers clipt with the browsers it finds and
//! writes the extension where "Load unpacked" can pick it up.

use crate::config::Config;
use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The native messaging host's name, as the extension asks for it.
pub const HOST: &str = "io.github.jasondyck.clipt";

/// The unpacked extension's ID, fixed by the public key in its manifest.
pub const EXTENSION_ID: &str = "mmlcllkbmdmifjanehakjhafeombhhgb";

/// The extension, built in, so `clipt browser-extension` can write it out
/// whatever way clipt was installed.
const EXTENSION_FILES: &[(&str, &[u8])] = &[
    ("manifest.json", include_bytes!("../extensions/chromium/manifest.json")),
    ("background.js", include_bytes!("../extensions/chromium/background.js")),
    ("popup.html", include_bytes!("../extensions/chromium/popup.html")),
    ("popup.js", include_bytes!("../extensions/chromium/popup.js")),
    ("icons/icon-16.png", include_bytes!("../extensions/chromium/icons/icon-16.png")),
    ("icons/icon-32.png", include_bytes!("../extensions/chromium/icons/icon-32.png")),
    ("icons/icon-48.png", include_bytes!("../extensions/chromium/icons/icon-48.png")),
    ("icons/icon-128.png", include_bytes!("../extensions/chromium/icons/icon-128.png")),
];

/// Whether a browser started this process as the native messaging host: it
/// passes the calling extension's origin first (Windows adds
/// `--parent-window=…` after it).
pub fn started_by_browser() -> bool {
    std::env::args().nth(1).is_some_and(|a| a.starts_with("chrome-extension://"))
}

#[derive(Debug, PartialEq)]
enum Action {
    Copy,
    Save,
}

#[derive(serde::Deserialize)]
struct Request {
    action: String,
    /// The page as a base64 PNG.
    png: String,
}

/// Answer one request from the extension. `copy` puts the PNG on the
/// clipboard and calls its second argument once it's there, which sends
/// the reply: on Wayland and X11, clipt then lingers to serve the paste,
/// and the browser waits for nothing but the reply.
pub fn serve(
    cfg: &Config,
    copy: impl FnOnce(Vec<u8>, &mut dyn FnMut()) -> Result<()>,
    notify: impl Fn(&str, &str, Option<&Path>),
) -> Result<()> {
    let mut out = std::io::stdout().lock();
    let mut replied = false;
    let r = (|| -> Result<()> {
        let (action, png) = read_request(&mut std::io::stdin().lock())?;
        let saved = if action == Action::Save { Some(save(cfg, &png)?) } else { None };
        let mut done = || {
            if !replied {
                let path = saved.as_ref().map(|p| p.display().to_string());
                let _ = write_message(&mut out, &serde_json::json!({ "ok": true, "path": path }));
                replied = true;
                match &saved {
                    Some(p) => notify("Page saved", &p.display().to_string(), Some(p)),
                    None => notify("Page copied", "The whole page is on the clipboard", None),
                }
            }
        };
        if action == Action::Copy || cfg.copy_on_save {
            copy(png, &mut done)?;
        }
        done();
        Ok(())
    })();
    if let Err(e) = &r
        && !replied
    {
        let _ = write_message(&mut out, &serde_json::json!({ "ok": false, "error": format!("{e:#}") }));
    }
    r
}

fn read_request(r: &mut impl Read) -> Result<(Action, Vec<u8>)> {
    let msg = read_message(r)?;
    let req: Request = serde_json::from_slice(&msg).context("reading the extension's message")?;
    let action = match req.action.as_str() {
        "copy" => Action::Copy,
        "save" => Action::Save,
        a => bail!("unknown action '{a}'"),
    };
    let png = base64_decode(&req.png).context("decoding the page's PNG")?;
    anyhow::ensure!(png.starts_with(b"\x89PNG"), "the extension didn't send a PNG");
    Ok((action, png))
}

fn save(cfg: &Config, png: &[u8]) -> Result<PathBuf> {
    let p = cfg.next_save_path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&p, png).with_context(|| format!("writing {}", p.display()))?;
    Ok(p)
}

/// Native messaging framing: a 32-bit length in native byte order, then
/// that much UTF-8 JSON.
fn read_message(r: &mut impl Read) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).context("no message from the browser")?;
    let len = u32::from_ne_bytes(len) as usize;
    // Chrome sends at most 64 MiB.
    anyhow::ensure!(len <= 64 << 20, "message too large ({len} bytes)");
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).context("the browser's message ended early")?;
    Ok(buf)
}

fn write_message(w: &mut impl Write, v: &serde_json::Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(v)?;
    w.write_all(&(body.len() as u32).to_ne_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// Standard base64, padding optional.
fn base64_decode(s: &str) -> Result<Vec<u8>> {
    let val = |c: u8| -> Result<u32> {
        Ok(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => bail!("not base64: {:?}", c as char),
        } as u32)
    };
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(s.len() / 4 * 3 + 2);
    for chunk in s.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        let bytes = n.to_be_bytes();
        match chunk.len() {
            4 => out.extend_from_slice(&bytes[1..4]),
            3 => out.extend_from_slice(&bytes[1..3]),
            2 => out.push(bytes[1]),
            _ => bail!("not base64: bad length"),
        }
    }
    Ok(out)
}

// ---- installing ---------------------------------------------------------------

/// `clipt browser-extension`: write the extension out, and register clipt
/// as its native messaging host with every Chromium browser installed.
pub fn install() -> Result<()> {
    if std::env::var_os("FLATPAK_ID").is_some() {
        bail!("the Flatpak can't be started by a browser outside its sandbox; install clipt another way to use the browser extension");
    }
    let exe = std::env::current_exe().context("finding clipt's own path")?;
    let dir = Config::path().parent().map(Path::to_path_buf).context("no config folder")?;
    let ext_dir = dir.join("browser-extension");
    for (name, data) in EXTENSION_FILES {
        let p = ext_dir.join(name);
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, data).with_context(|| format!("writing {}", p.display()))?;
    }
    let manifest = host_manifest(&exe);
    let registered = register(&dir, &manifest)?;
    if registered.is_empty() {
        println!("No Chrome, Edge, Chromium or Brave found to register with; run this again after installing one.");
    } else {
        println!("Registered clipt with: {}", registered.join(", "));
    }
    flatpak_note();
    println!(
        "\nTo add the extension: open chrome://extensions (edge://extensions in Edge), turn on Developer mode, \
         click \"Load unpacked\" and choose:\n  {}\n\nThen click clipt's icon in the toolbar, or press Alt+Shift+P, to copy a whole page.",
        ext_dir.display()
    );
    Ok(())
}

fn host_manifest(exe: &Path) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "name": HOST,
        "description": "clipt: full-page screenshots from the browser",
        "path": exe,
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{EXTENSION_ID}/")],
    }))
    .unwrap()
}

/// Browsers find hosts through a JSON file in their own config folder.
#[cfg(not(windows))]
fn register(_dir: &Path, manifest: &str) -> Result<Vec<String>> {
    #[cfg(target_os = "macos")]
    let base = crate::config::home().join("Library/Application Support");
    #[cfg(target_os = "macos")]
    let browsers =
        [("Chrome", "Google/Chrome"), ("Chromium", "Chromium"), ("Edge", "Microsoft Edge"), ("Brave", "BraveSoftware/Brave-Browser")];
    #[cfg(not(target_os = "macos"))]
    let base = crate::config::config_home();
    #[cfg(not(target_os = "macos"))]
    let browsers =
        [("Chrome", "google-chrome"), ("Chromium", "chromium"), ("Edge", "microsoft-edge"), ("Brave", "BraveSoftware/Brave-Browser")];
    let mut done = Vec::new();
    for (name, rel) in browsers {
        let profile = base.join(rel);
        if !profile.is_dir() {
            continue;
        }
        let hosts = profile.join("NativeMessagingHosts");
        std::fs::create_dir_all(&hosts)?;
        let p = hosts.join(format!("{HOST}.json"));
        std::fs::write(&p, manifest).with_context(|| format!("writing {}", p.display()))?;
        done.push(name.to_string());
    }
    Ok(done)
}

/// Windows browsers find hosts through a registry key naming the JSON file.
#[cfg(windows)]
fn register(dir: &Path, manifest: &str) -> Result<Vec<String>> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegSetKeyValueW};
    let file = dir.join(format!("{HOST}.json"));
    std::fs::create_dir_all(dir)?;
    std::fs::write(&file, manifest).with_context(|| format!("writing {}", file.display()))?;
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let value = wide(&file.to_string_lossy());
    let mut done = Vec::new();
    for (name, key) in
        [("Chrome", r"Software\Google\Chrome"), ("Edge", r"Software\Microsoft\Edge"), ("Brave", r"Software\BraveSoftware\Brave-Browser")]
    {
        let key = wide(&format!(r"{key}\NativeMessagingHosts\{HOST}"));
        // An empty value name sets the key's default value, which is what
        // the browsers read.
        let r = unsafe {
            RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), std::ptr::null(), REG_SZ, value.as_ptr().cast(), (value.len() * 2) as u32)
        };
        anyhow::ensure!(r == 0, "registering with {name}: the registry said {r}");
        done.push(name.to_string());
    }
    Ok(done)
}

/// Chrome from Flathub runs in a sandbox that can't start clipt.
fn flatpak_note() {
    #[cfg(all(unix, not(target_os = "macos")))]
    if crate::config::home().join(".var/app/com.google.Chrome").is_dir()
        || crate::config::home().join(".var/app/org.chromium.Chromium").is_dir()
    {
        println!(
            "Note: Chrome or Chromium from Flathub can't start programs outside its sandbox, so the extension can't reach clipt there; \
             use a Chrome, Edge or Chromium from your distribution or the browser's own package."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGVsbG8").unwrap(), b"hello");
        assert_eq!(base64_decode("aGk=").unwrap(), b"hi");
        assert_eq!(base64_decode("+/8=").unwrap(), [0xfb, 0xff]);
        assert_eq!(base64_decode("").unwrap(), b"");
        assert!(base64_decode("a").is_err());
        assert!(base64_decode("a$==").is_err());
    }

    fn framed(json: &str) -> Vec<u8> {
        let mut v = (json.len() as u32).to_ne_bytes().to_vec();
        v.extend_from_slice(json.as_bytes());
        v
    }

    #[test]
    fn reads_a_request() {
        // "\x89PNG" in base64.
        let msg = framed(r#"{"action":"save","png":"iVBORw==","title":"t","url":"u"}"#);
        let (action, png) = read_request(&mut &msg[..]).unwrap();
        assert_eq!((action, png), (Action::Save, b"\x89PNG".to_vec()));
        assert!(read_request(&mut &framed(r#"{"action":"print","png":"iVBORw=="}"#)[..]).is_err());
        assert!(read_request(&mut &framed(r#"{"action":"copy","png":"aGk="}"#)[..]).is_err(), "not a PNG");
        assert!(read_request(&mut &msg[..10]).is_err(), "cut short");
    }

    #[test]
    fn replies_are_framed() {
        let mut out = Vec::new();
        write_message(&mut out, &serde_json::json!({ "ok": true })).unwrap();
        assert_eq!(&out[4..], br#"{"ok":true}"#);
        assert_eq!(u32::from_ne_bytes(out[..4].try_into().unwrap()) as usize, out.len() - 4);
    }

    /// The ID the host manifest allows must be the one Chrome derives from
    /// the manifest's key: the first 16 bytes of its SHA-256, as a-p.
    #[test]
    fn extension_id_matches_its_key() {
        let manifest: serde_json::Value = serde_json::from_slice(EXTENSION_FILES[0].1).unwrap();
        let key = base64_decode(manifest["key"].as_str().unwrap()).unwrap();
        let digest = sha256(&key);
        let id: String = digest[..16].iter().flat_map(|b| [b >> 4, b & 15]).map(|n| (b'a' + n) as char).collect();
        assert_eq!(id, EXTENSION_ID);
    }

    /// Enough SHA-256 for the test above.
    fn sha256(data: &[u8]) -> [u8; 32] {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01,
            0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc,
            0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147,
            0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
            0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08,
            0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
            0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
        ];
        let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
        let mut msg = data.to_vec();
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
        for block in msg.chunks(64) {
            let mut w = [0u32; 64];
            for i in 0..16 {
                w[i] = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
            }
            let mut v = h;
            for i in 0..64 {
                let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
                let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
                let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
                let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
                let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
                let t2 = s0.wrapping_add(maj);
                v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
            }
            for (a, b) in h.iter_mut().zip(v) {
                *a = a.wrapping_add(b);
            }
        }
        let mut out = [0u8; 32];
        for (o, x) in out.chunks_mut(4).zip(h) {
            o.copy_from_slice(&x.to_be_bytes());
        }
        out
    }
}
