//! `clipt tray` on Windows: an icon in the notification area with the
//! Linux tray's actions (screenshots, recording, updates), and a global
//! hotkey (Print Screen by default). The icon gets a red dot while
//! recording, amber while paused.
//!
//! As on Linux and macOS, the tray never captures anything itself: each
//! action starts a fresh `clipt` with no console window. Screenshot
//! processes tell the tray what they saved or copied (`notify`), and the
//! tray shows it as a notification, since Windows only shows those for a
//! program with an icon in the notification area.

use super::{from_wide, wide};
use crate::config::Config;
use anyhow::{Context, Result, bail};
use std::cell::RefCell;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey, VK_F1, VK_SNAPSHOT,
};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_LARGE_ICON, NIIF_USER, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NIN_BALLOONUSERCLICK, NIN_SELECT, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW, ShellExecuteW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DestroyWindow, DispatchMessageW, FindWindowW, GetCursorPos, GetMessageW, GetSystemMetrics, HICON, ICONINFO, MF_CHECKED, MF_SEPARATOR,
    MF_STRING, MSG, PostMessageW, PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SM_CXSMICON, SMTO_ABORTIFHUNG, SW_SHOWNORMAL,
    SendMessageTimeoutW, SetForegroundWindow, SetTimer, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx, TranslateMessage, WM_APP,
    WM_CONTEXTMENU, WM_COPYDATA, WM_DESTROY, WM_HOTKEY, WM_NULL, WM_TIMER, WNDCLASSEXW,
};

/// Long enough for the menu to fade out of the shot.
const CLICK_DELAY: &str = "0.2";
const CLASS: &str = "CliptTray";
const CALLBACK: u32 = WM_APP + 1;
/// From the update thread: the new clipt.exe is in place; restart on it.
const RESTART: u32 = WM_APP + 2;
/// A notification's "path" meaning: clicking it installs the update.
const UPDATE: &str = "clipt:update";
/// `COPYDATASTRUCT.dwData` for a notification from a screenshot process.
const NOTIFY: usize = 0x636c_6970; // "clip"
/// Enter or space on the icon (NIN_SELECT | NINF_KEY).
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
/// HKCU's Run key: programs Windows starts at sign-in.
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

#[derive(Clone, Copy, Debug, PartialEq)]
enum Action {
    Region,
    RegionDelayed,
    Full,
    RecordRegion,
    RecordScreen,
    ToggleSound,
    ToggleGif,
    PauseRecording,
    ResumeRecording,
    StopRecording,
    OpenFolder,
    OpenVideos,
    ToggleCopy,
    ToggleNotify,
    OpenSettings,
    Startup,
    CheckUpdates,
    Quit,
}

/// (label, action); `None` is a separator. A menu item's id is its index + 1.
const MENU: &[Option<(&str, Action)>] = &[
    Some(("Capture region", Action::Region)),
    Some(("Capture region in 3 seconds", Action::RegionDelayed)),
    Some(("Copy full desktop", Action::Full)),
    None,
    Some(("Record region…", Action::RecordRegion)),
    Some(("Record screen", Action::RecordScreen)),
    Some(("Record sound", Action::ToggleSound)),
    Some(("Record as GIF", Action::ToggleGif)),
    Some(("Pause recording", Action::PauseRecording)),
    Some(("Resume recording", Action::ResumeRecording)),
    Some(("Stop recording", Action::StopRecording)),
    None,
    Some(("Open screenshots folder", Action::OpenFolder)),
    Some(("Open recordings folder", Action::OpenVideos)),
    None,
    Some(("Copy when saving", Action::ToggleCopy)),
    Some(("Notifications", Action::ToggleNotify)),
    Some(("Settings…", Action::OpenSettings)),
    None,
    Some(("Start with Windows", Action::Startup)),
    Some(("Check for updates…", Action::CheckUpdates)),
    Some(("Quit clipt", Action::Quit)),
];

/// Recording state, as the tray last saw it: also the index of its icon.
const IDLE: usize = 0;
const RECORDING: usize = 1;
const PAUSED: usize = 2;

fn rec_state() -> usize {
    match crate::record::status() {
        None => IDLE,
        Some(false) => RECORDING,
        Some(true) => PAUSED,
    }
}

/// Whether the menu shows `a` in recording state `rec`: what starts a
/// recording while there's none, what controls one while there is.
fn shown(a: Action, rec: usize) -> bool {
    match a {
        Action::RecordRegion | Action::RecordScreen | Action::ToggleSound | Action::ToggleGif => rec == IDLE,
        Action::PauseRecording => rec == RECORDING,
        Action::ResumeRecording => rec == PAUSED,
        Action::StopRecording => rec != IDLE,
        _ => true,
    }
}

struct State {
    hwnd: HWND,
    /// Idle, recording, paused.
    icons: [HICON; 3],
    rec: usize,
    /// Held while the tray runs; let go before restarting after an update.
    mutex: HANDLE,
    save_dir: String,
    video_dir: String,
    /// The "Record sound" tick, and whether `record_audio` names sources:
    /// if it does, a ticked box records those, otherwise the computer's
    /// sound.
    sound: bool,
    sound_configured: bool,
    /// The file the last notification was about, opened when it's clicked.
    last: Option<String>,
    /// Explorer's broadcast when the taskbar comes back after a restart.
    taskbar_created: u32,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

pub fn run(cfg: &Config) -> Result<()> {
    // One tray per session: a second would add a second icon, and its
    // hotkey would fail anyway.
    let name = wide(r"Local\clipt-tray");
    let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if mutex.is_null() || unsafe { GetLastError() } == 183 {
        // ERROR_ALREADY_EXISTS
        bail!("clipt tray is already running");
    }

    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    let class = wide(CLASS);
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance,
        lpszClassName: class.as_ptr(),
        ..unsafe { std::mem::zeroed() }
    };
    unsafe { RegisterClassExW(&wc) };
    // A hidden top-level window rather than a message-only one: those don't
    // get the TaskbarCreated broadcast.
    let title = wide("clipt");
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        )
    };
    anyhow::ensure!(!hwnd.is_null(), "creating the tray window failed");
    let taskbar_created = unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) };
    let icons = [make_icon(None)?, make_icon(Some((0xef, 0x44, 0x44)))?, make_icon(Some((0xf5, 0x9e, 0x0b)))?];
    STATE.set(Some(State {
        hwnd,
        icons,
        rec: rec_state(),
        mutex,
        save_dir: crate::config::expand(&cfg.save_dir).display().to_string(),
        video_dir: crate::config::expand(&cfg.video_dir).display().to_string(),
        sound: sound_configured(cfg),
        sound_configured: sound_configured(cfg),
        last: None,
        taskbar_created,
    }));
    add_icon()?;
    // Recordings run in their own processes: look at their state each second.
    unsafe { SetTimer(hwnd, 1, 1000, None) };
    if cfg.check_updates {
        // Once shortly after sign-in, then daily.
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(60));
            loop {
                updates(false);
                std::thread::sleep(std::time::Duration::from_secs(24 * 3600));
            }
        });
    }

    for (id, spec) in [(1, cfg.hotkey.as_str()), (2, cfg.hotkey_full.as_str())] {
        if spec.is_empty() {
            continue;
        }
        // A mistyped or taken shortcut costs only that shortcut, not the tray.
        let r = Hotkey::parse(spec).and_then(|k| {
            if unsafe { RegisterHotKey(hwnd, id, k.mods | MOD_NOREPEAT, k.vk) } == 0 {
                bail!("another program has it")
            }
            Ok(())
        });
        if let Err(e) = r {
            let print = spec.eq_ignore_ascii_case("print");
            let hint = if print {
                "Windows gives Print Screen to the Snipping Tool: turn off Settings > Accessibility > Keyboard > \
                 \"Use the Print screen key to open screen capture\", then restart clipt's tray. Or set another \
                 key: clipt config hotkey ctrl+shift+s"
            } else {
                "Set another with clipt config hotkey KEY"
            };
            eprintln!("clipt: hotkey {spec}: {e:#}. {hint}");
            balloon(&format!("{spec} isn't available as clipt's shortcut"), hint, None);
        }
    }

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// Hand a notification to the tray, if one is running: Windows shows them
/// only for a program with a notification-area icon. `path`, when given,
/// opens if the notification is clicked.
pub fn notify(title: &str, body: &str, path: Option<&std::path::Path>) {
    let hwnd = unsafe { FindWindowW(wide(CLASS).as_ptr(), std::ptr::null()) };
    if hwnd.is_null() {
        return;
    }
    let text: Vec<u16> = format!("{title}\n{body}\n{}", path.map(|p| p.display().to_string()).unwrap_or_default()).encode_utf16().collect();
    let cds = COPYDATASTRUCT { dwData: NOTIFY, cbData: (text.len() * 2) as u32, lpData: text.as_ptr() as *mut _ };
    let mut result = 0;
    unsafe { SendMessageTimeoutW(hwnd, WM_COPYDATA, 0, &cds as *const _ as LPARAM, SMTO_ABORTIFHUNG, 1000, &mut result) };
}

fn icon_data(flags: u32) -> NOTIFYICONDATAW {
    STATE.with_borrow(|s| {
        let s = s.as_ref().expect("tray state");
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = s.hwnd;
        nid.uID = 1;
        nid.uFlags = flags;
        nid.uCallbackMessage = CALLBACK;
        nid.hIcon = s.icons[s.rec];
        copy_into(
            &mut nid.szTip,
            match s.rec {
                RECORDING => "clipt — recording (click: stop)",
                PAUSED => "clipt — recording paused (click: stop)",
                _ => "clipt",
            },
        );
        nid
    })
}

fn add_icon() -> Result<()> {
    let mut nid = icon_data(NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP);
    if unsafe { Shell_NotifyIconW(NIM_ADD, &nid) } == 0 {
        bail!("adding the notification-area icon failed");
    }
    // Version 4: one callback per click, with the event in LOWORD(lParam).
    nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    unsafe { Shell_NotifyIconW(NIM_SETVERSION, &nid) };
    Ok(())
}

fn balloon(title: &str, body: &str, path: Option<String>) {
    let mut nid = icon_data(NIF_INFO);
    copy_into(&mut nid.szInfoTitle, title);
    copy_into(&mut nid.szInfo, body);
    nid.dwInfoFlags = NIIF_USER | NIIF_LARGE_ICON;
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
    STATE.with_borrow_mut(|s| {
        if let Some(s) = s.as_mut() {
            s.last = path;
        }
    });
}

/// Copy `s` into a fixed UTF-16 field, cut short to fit with its null.
fn copy_into(dst: &mut [u16], s: &str) {
    let src: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..src.len()].copy_from_slice(&src);
    dst[src.len()] = 0;
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let taskbar_created = STATE.with_borrow(|s| s.as_ref().map_or(0, |s| s.taskbar_created));
    match msg {
        CALLBACK => match (l & 0xffff) as u32 {
            // As on Linux: a click captures a region, or stops the recording.
            NIN_SELECT | NIN_KEYSELECT => {
                let idle = STATE.with_borrow(|s| s.as_ref().is_none_or(|s| s.rec == IDLE));
                run_action(if idle { Action::Region } else { Action::StopRecording })
            }
            WM_CONTEXTMENU => show_menu(h),
            NIN_BALLOONUSERCLICK => match STATE.with_borrow(|s| s.as_ref().and_then(|s| s.last.clone())) {
                Some(p) if p == UPDATE => {
                    std::thread::spawn(install_update);
                }
                Some(p) => open(&p),
                None => {}
            },
            _ => {}
        },
        WM_TIMER => poll_recording(),
        RESTART => restart(),
        WM_HOTKEY => run_action(if w == 2 { Action::Full } else { Action::Region }),
        WM_COPYDATA => {
            let cds = unsafe { &*(l as *const COPYDATASTRUCT) };
            if cds.dwData == NOTIFY && !cds.lpData.is_null() {
                let text =
                    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(cds.lpData as *const u16, cds.cbData as usize / 2) });
                let mut parts = text.splitn(3, '\n');
                let (title, body, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                if std::env::var_os("CLIPT_DEBUG").is_some() {
                    eprintln!("clipt: notification: {title} — {body}");
                }
                balloon(title, body, (!path.is_empty()).then(|| path.to_string()));
            }
            return 1;
        }
        WM_DESTROY => {
            let nid = icon_data(0);
            unsafe {
                Shell_NotifyIconW(NIM_DELETE, &nid);
                PostQuitMessage(0);
            }
        }
        m if m == taskbar_created && m != 0 => {
            let _ = add_icon();
        }
        _ => return unsafe { DefWindowProcW(h, msg, w, l) },
    }
    0
}

/// Show the recording's state on the icon when it changes.
fn poll_recording() {
    let now = rec_state();
    let changed = STATE.with_borrow_mut(|s| match s.as_mut() {
        Some(s) if s.rec != now => {
            s.rec = now;
            true
        }
        _ => false,
    });
    if changed {
        if std::env::var_os("CLIPT_DEBUG").is_some() {
            eprintln!("clipt: recording state: {}", ["idle", "recording", "paused"][now]);
        }
        let nid = icon_data(NIF_ICON | NIF_TIP | NIF_SHOWTIP);
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
    }
}

fn show_menu(h: HWND) {
    let cfg = Config::load().unwrap_or_default();
    poll_recording();
    let rec = STATE.with_borrow(|s| s.as_ref().map_or(IDLE, |s| s.rec));
    let menu = unsafe { CreatePopupMenu() };
    let mut last_separator = true;
    for (i, entry) in MENU.iter().enumerate() {
        match entry {
            // Never two in a row, where hidden items left a section empty.
            None if last_separator => {}
            None => unsafe {
                AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
                last_separator = true;
            },
            Some((_, a)) if !shown(*a, rec) => {}
            Some((label, a)) => {
                last_separator = false;
                let on = match a {
                    Action::ToggleCopy => cfg.copy_on_save,
                    Action::ToggleNotify => cfg.notify,
                    Action::ToggleGif => cfg.record_format == "gif",
                    Action::ToggleSound => STATE.with_borrow(|s| s.as_ref().is_some_and(|s| s.sound)),
                    Action::Startup => startup_enabled(),
                    _ => false,
                };
                let flags = MF_STRING | if on { MF_CHECKED } else { 0 };
                unsafe { AppendMenuW(menu, flags, i + 1, wide(label).as_ptr()) };
            }
        }
    }
    let mut p = POINT { x: 0, y: 0 };
    unsafe {
        GetCursorPos(&mut p);
        // Without this the menu stays open when clicking elsewhere.
        SetForegroundWindow(h);
        let id = TrackPopupMenuEx(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, p.x, p.y, h, std::ptr::null());
        PostMessageW(h, WM_NULL, 0, 0);
        DestroyMenu(menu);
        if let Some(Some((_, a))) = (id as usize).checked_sub(1).and_then(|i| MENU.get(i)) {
            run_action(*a);
        }
    }
}

fn run_action(a: Action) {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return eprintln!("clipt: {e}"),
    };
    let sound: &[&str] = match STATE.with_borrow(|s| s.as_ref().map_or((false, false), |s| (s.sound, s.sound_configured))) {
        (false, _) => &["--no-audio"],
        (true, true) => &[],
        (true, false) => &["--audio"],
    };
    let args: &[&str] = match a {
        Action::Region => &["gui", "-d", CLICK_DELAY],
        Action::RegionDelayed => &["gui", "-d", "3"],
        Action::Full => &["full", "-c", "-d", CLICK_DELAY],
        Action::RecordRegion => &[&["record", "-d", CLICK_DELAY], sound].concat(),
        Action::RecordScreen => &[&["record", "screen", "-d", CLICK_DELAY], sound].concat(),
        Action::ToggleSound => {
            STATE.with_borrow_mut(|s| {
                if let Some(s) = s.as_mut() {
                    s.sound = !s.sound;
                }
            });
            return;
        }
        Action::PauseRecording => &["record", "--pause"],
        Action::ResumeRecording => &["record", "--resume"],
        Action::StopRecording => &["record", "--stop"],
        Action::OpenFolder | Action::OpenVideos => {
            let dir = STATE
                .with_borrow(|s| s.as_ref().map(|s| if a == Action::OpenFolder { s.save_dir.clone() } else { s.video_dir.clone() }))
                .unwrap_or_default();
            let _ = std::fs::create_dir_all(&dir);
            return open(&dir);
        }
        Action::ToggleGif => {
            let gif = Config::load().unwrap_or_default().record_format == "gif";
            if let Err(e) = Config::set("record_format", if gif { "mp4" } else { "gif" }) {
                eprintln!("clipt: {e:#}");
            }
            return;
        }
        Action::CheckUpdates => {
            std::thread::spawn(|| updates(true));
            return;
        }
        Action::OpenSettings => {
            let p = Config::path();
            if !p.exists() {
                let _ = p.parent().map(std::fs::create_dir_all);
                let _ = std::fs::write(&p, "# clipt's settings: `clipt config` lists them.\n");
            }
            // Notepad: .toml often has no program to open it.
            let _ = Command::new("notepad.exe").arg(&p).spawn();
            return;
        }
        Action::ToggleCopy | Action::ToggleNotify => {
            let cfg = Config::load().unwrap_or_default();
            let (key, on) = if a == Action::ToggleCopy { ("copy_on_save", cfg.copy_on_save) } else { ("notify", cfg.notify) };
            if let Err(e) = Config::set(key, if on { "false" } else { "true" }) {
                eprintln!("clipt: {e:#}");
            }
            return;
        }
        Action::Startup => {
            if let Err(e) = set_startup(!startup_enabled()) {
                eprintln!("clipt: start with Windows: {e:#}");
            }
            return;
        }
        Action::Quit => {
            let h = STATE.with_borrow(|s| s.as_ref().map(|s| s.hwnd));
            if let Some(h) = h {
                unsafe { DestroyWindow(h) };
            }
            return;
        }
    };
    // The tray has just had a click or a hotkey, so Windows lets it hand the
    // foreground on: without that the overlay would open behind other windows.
    unsafe { AllowSetForegroundWindow(u32::MAX) };
    let r = Command::new(exe)
        .args(args)
        .creation_flags(DETACHED_PROCESS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match r {
        Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
        Err(e) => eprintln!("clipt: starting {a:?}: {e}"),
    }
}

// ---- updates ----------------------------------------------------------------

/// Check for a release and say so. `manual` also reports "up to date" and
/// errors; the daily check only speaks up about a new version. Runs on its
/// own thread, so it reaches the icon as other processes do (`notify`).
fn updates(manual: bool) {
    use crate::update::{self, Install};
    let r = match update::check() {
        Ok(r) => r,
        Err(e) => {
            if manual {
                notify("Couldn't check for updates", &format!("{e:#}"), None);
            }
            return;
        }
    };
    let Some(r) = r else {
        if manual {
            notify("clipt is up to date", &format!("You have the latest version, {}.", env!("CARGO_PKG_VERSION")), None);
        }
        return;
    };
    let title = format!("clipt {} is available", r.version());
    let have = env!("CARGO_PKG_VERSION");
    match Install::detect() {
        Install::SelfManaged(_) => notify(&title, &format!("You have {have}. Click here to update."), Some(UPDATE.as_ref())),
        other => notify(&title, &format!("You have {have}. To update, {}.", other.how_to_update()), Some(r.html_url.as_ref())),
    }
}

/// Clicked "update": install the release over this clipt.exe, then have the
/// tray restart on it.
fn install_update() {
    use crate::update::{self, Install};
    let r = match update::check() {
        Ok(Some(r)) => r,
        Ok(None) => return notify("clipt is up to date", "", None),
        Err(e) => return notify("Couldn't update", &format!("{e:#}"), None),
    };
    let Install::SelfManaged(exe) = Install::detect() else { return };
    match update::install(&r, &exe) {
        Ok(_) => {
            notify(&format!("Updated to clipt {}", r.version()), "The tray icon restarts on the new version.", None);
            let hwnd = unsafe { FindWindowW(wide(CLASS).as_ptr(), std::ptr::null()) };
            unsafe { PostMessageW(hwnd, RESTART, 0, 0) };
        }
        Err(e) => notify("Update failed", &format!("{e:#}"), None),
    }
}

/// Start the tray again on the clipt.exe now in place, and quit this one.
fn restart() {
    let (mutex, hwnd) = STATE.with_borrow(|s| s.as_ref().map_or((std::ptr::null_mut(), std::ptr::null_mut()), |s| (s.mutex, s.hwnd)));
    // The new tray takes the mutex, and the icon's place.
    unsafe { CloseHandle(mutex) };
    let r = Command::new(crate::update::current_exe())
        .arg("tray")
        .creation_flags(DETACHED_PROCESS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Err(e) = r {
        eprintln!("clipt tray: restarting failed: {e}");
    }
    unsafe { DestroyWindow(hwnd) };
}

fn sound_configured(cfg: &Config) -> bool {
    crate::record::audio::Source::parse(&cfg.record_audio).is_ok_and(|s| !s.is_empty())
}

/// No console at all, so none flashes up.
pub const DETACHED_PROCESS: u32 = 0x0000_0008;

fn open(target: &str) {
    let (op, t) = (wide("open"), wide(target));
    unsafe { ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), t.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) };
}

fn startup_command() -> Option<String> {
    std::env::current_exe().ok().map(|e| format!("\"{}\" tray", e.display()))
}

fn startup_enabled() -> bool {
    let (key, name) = (wide(RUN_KEY), wide("clipt"));
    let mut buf = [0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut len)
    };
    // Only if it starts this clipt.exe: a copy elsewhere is another install.
    r == 0 && Some(from_wide(&buf)) == startup_command()
}

fn set_startup(on: bool) -> Result<()> {
    let (key, name) = (wide(RUN_KEY), wide("clipt"));
    let r = if on {
        let cmd = wide(&startup_command().context("no path to clipt.exe")?);
        unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), REG_SZ, cmd.as_ptr().cast(), (cmd.len() * 2) as u32) }
    } else {
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) }
    };
    anyhow::ensure!(r == 0, "the registry said {r}");
    Ok(())
}

/// The tray icon at the notification area's size, from the same drawing as
/// the other platforms', with a dot of `badge`'s colour for recording.
fn make_icon(badge: Option<(u8, u8, u8)>) -> Result<HICON> {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.clamp(16, 64) as u32;
    let argb = crate::icon::argb32(size, badge);
    let dib = super::capture::Dib::new(size as i32, size as i32)?;
    let bits = unsafe { std::slice::from_raw_parts_mut(dib.bits, (size * size * 4) as usize) };
    // Icons take straight (not premultiplied) alpha, BGRA.
    for (d, &[a, r, g, b]) in bits.as_chunks_mut::<4>().0.iter_mut().zip(argb.as_chunks::<4>().0) {
        *d = [b, g, r, a];
    }
    let zeros = vec![0u8; (size * size / 8) as usize];
    let mask = unsafe { CreateBitmap(size as i32, size as i32, 1, 1, zeros.as_ptr().cast()) };
    let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: dib.bitmap() };
    // CreateIconIndirect copies both bitmaps.
    let icon = unsafe { CreateIconIndirect(&info) };
    unsafe { DeleteObject(mask) };
    anyhow::ensure!(!icon.is_null(), "creating the tray icon failed");
    Ok(icon)
}

/// A shortcut from the config: "print", "ctrl+shift+s", "win+alt+f9".
#[derive(Debug, PartialEq)]
struct Hotkey {
    vk: u32,
    mods: u32,
}

impl Hotkey {
    fn parse(spec: &str) -> Result<Self> {
        let mut mods = 0;
        let mut key = None;
        for part in spec.split('+').map(|p| p.trim().to_ascii_lowercase()) {
            match part.as_str() {
                "ctrl" | "control" => mods |= MOD_CONTROL,
                "shift" => mods |= MOD_SHIFT,
                "alt" => mods |= MOD_ALT,
                "win" | "super" | "meta" => mods |= MOD_WIN,
                "" => bail!("an empty key"),
                other if key.is_none() => key = Some(other.to_string()),
                other => bail!("two keys ('{}' and '{other}'); one key and modifiers", key.unwrap_or_default()),
            }
        }
        let key = key.context("no key, only modifiers")?;
        let vk = match key.as_str() {
            "print" | "printscreen" | "prtsc" | "prtscn" => VK_SNAPSHOT as u32,
            k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphanumeric() => k.as_bytes()[0].to_ascii_uppercase() as u32,
            k if k.starts_with('f') && k[1..].parse::<u32>().is_ok_and(|n| (1..=24).contains(&n)) => {
                VK_F1 as u32 + k[1..].parse::<u32>().unwrap_or(1) - 1
            }
            k => bail!("unknown key '{k}' (a-z, 0-9, f1-f24 or print)"),
        };
        let bare = vk == VK_SNAPSHOT as u32 || (VK_F1 as u32..VK_F1 as u32 + 24).contains(&vk);
        if mods & (MOD_CONTROL | MOD_ALT | MOD_WIN) == 0 && !bare {
            bail!("needs ctrl, alt or win, or it would take that key from every program");
        }
        Ok(Hotkey { vk, mods })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_follows_the_recording() {
        let labels = |rec| MENU.iter().flatten().filter(|(_, a)| shown(*a, rec)).map(|(l, _)| *l).collect::<Vec<_>>();
        let idle = labels(IDLE);
        assert!(idle.contains(&"Record region…") && idle.contains(&"Record as GIF") && !idle.contains(&"Stop recording"));
        let rec = labels(RECORDING);
        assert!(rec.contains(&"Pause recording") && rec.contains(&"Stop recording"));
        assert!(!rec.contains(&"Record screen") && !rec.contains(&"Resume recording"));
        let paused = labels(PAUSED);
        assert!(paused.contains(&"Resume recording") && !paused.contains(&"Pause recording"));
    }

    #[test]
    fn parses_hotkeys() {
        assert_eq!(Hotkey::parse("print").unwrap(), Hotkey { vk: 0x2c, mods: 0 });
        assert_eq!(Hotkey::parse("Ctrl + Shift + S").unwrap(), Hotkey { vk: b'S' as u32, mods: MOD_CONTROL | MOD_SHIFT });
        assert_eq!(Hotkey::parse("win+alt+f9").unwrap(), Hotkey { vk: 0x78, mods: MOD_WIN | MOD_ALT });
        assert_eq!(Hotkey::parse("shift+print").unwrap().mods, MOD_SHIFT);
        assert!(Hotkey::parse("shift+s").is_err());
        assert!(Hotkey::parse("ctrl+shift").is_err());
        assert!(Hotkey::parse("ctrl+a+b").is_err());
        assert!(Hotkey::parse("ctrl+nope").is_err());
        assert!(Hotkey::parse("f25").is_err());
    }
}
