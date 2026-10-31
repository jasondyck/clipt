//! `clipt tray`: a StatusNotifierItem (the tray protocol KDE, waybar, the
//! GNOME AppIndicator extension and most panels speak) with a dbusmenu.
//!
//! The tray process never captures anything itself; each action spawns a
//! fresh `clipt` so the overlay starts from a clean process, exactly as if
//! it had been launched from a hotkey.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;
use zbus::object_server::SignalEmitter;
use zbus::{Connection, OwnedObjectPath, OwnedVariant, Structure, Variant, interface};

const ITEM_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
/// Held for the lifetime of the tray so a second `clipt tray` exits.
const LOCK_NAME: &str = "org.clipt.Tray";

/// A Flatpak sandbox may only own bus names under its app ID.
fn lock_name() -> String {
    match std::env::var("FLATPAK_ID") {
        Ok(id) if crate::update::flatpak() => format!("{id}.Tray"),
        _ => LOCK_NAME.into(),
    }
}

/// SNI pixmaps: (width, height, ARGB32 bytes).
type Pixmaps = Vec<(i32, i32, Vec<u8>)>;

/// Long enough for a panel tooltip or closing menu to fade out of the shot.
const CLICK_DELAY: &str = "0.25";

#[derive(Clone, Copy)]
enum Action {
    Region,
    RegionDelayed,
    Full,
    OpenFolder,
    CheckUpdates,
    Quit,
}

/// (id, label, action); id 0 is the root, `None` is a separator.
const MENU: &[(i32, Option<(&str, Action)>)] = &[
    (1, Some(("Capture region", Action::Region))),
    (2, Some(("Capture region in 3 seconds", Action::RegionDelayed))),
    (3, Some(("Copy full desktop", Action::Full))),
    (4, None),
    (5, Some(("Open screenshots folder", Action::OpenFolder))),
    (6, None),
    (7, Some(("Check for updates…", Action::CheckUpdates))),
    (8, Some(("Quit", Action::Quit))),
];

/// The menu as shown. Flatpak updates clipt itself, and the sandbox has no
/// network access to check, so the update entry (and its separator) go.
fn menu() -> impl Iterator<Item = &'static (i32, Option<(&'static str, Action)>)> {
    let flatpak = crate::update::flatpak();
    MENU.iter().filter(move |(id, _)| !(flatpak && matches!(id, 6 | 7)))
}

fn run_action(a: Action, save_dir: &str) {
    let exe = crate::update::current_exe();
    let r = match a {
        Action::Region => Command::new(&exe).args(["gui", "-d", CLICK_DELAY]).spawn(),
        Action::RegionDelayed => Command::new(&exe).args(["gui", "-d", "3"]).spawn(),
        Action::Full => Command::new(&exe).args(["full", "-c", "-d", CLICK_DELAY]).spawn(),
        Action::OpenFolder => {
            let _ = std::fs::create_dir_all(save_dir);
            Command::new("xdg-open").arg(save_dir).spawn()
        }
        Action::CheckUpdates => {
            std::thread::spawn(|| updates(true));
            return;
        }
        Action::Quit => std::process::exit(0),
    };
    if let Err(e) = r {
        eprintln!("clipt tray: launching failed: {e}");
    }
}

// ---- updates ----------------------------------------------------------------

/// Check for a release and tell the user. `manual` also reports "up to date"
/// and errors; the background check only speaks up about a new version.
fn updates(manual: bool) {
    use crate::update::{self, Install};
    let r = match update::check() {
        Ok(r) => r,
        Err(e) => {
            if manual {
                notify("Couldn't check for updates", &format!("{e:#}"), &[]);
            }
            return;
        }
    };
    let Some(r) = r else {
        if manual {
            notify("clipt is up to date", &format!("You have the latest version, {}.", env!("CARGO_PKG_VERSION")), &[]);
        }
        return;
    };
    let install = Install::detect();
    let title = format!("clipt {} is available", r.version());
    let action = match &install {
        Install::SelfManaged(_) => {
            let body = format!("You have {}.", env!("CARGO_PKG_VERSION"));
            notify(&title, &body, &[("update", "Update now"), ("notes", "What's new")])
        }
        other => {
            let body = format!("You have {}. To update, {}.", env!("CARGO_PKG_VERSION"), other.how_to_update());
            notify(&title, &body, &[("notes", "What's new")])
        }
    };
    match (action.as_deref(), install) {
        (Some("notes"), _) => {
            let _ = Command::new("xdg-open").arg(&r.html_url).spawn();
        }
        (Some("update"), Install::SelfManaged(exe)) => match update::install(&r, &exe) {
            Ok(new) => {
                notify(&format!("Updated to clipt {}", r.version()), "The tray icon restarts on the new version.", &[]);
                // Replace this process with the new binary; the bus
                // connection closes on exec and the new tray takes its place.
                use std::os::unix::process::CommandExt;
                let err = Command::new(&new).arg("tray").env("CLIPT_TRAY_RESTART", "1").exec();
                eprintln!("clipt tray: restarting failed: {err}");
            }
            Err(e) => {
                notify("Update failed", &format!("{e:#}"), &[]);
            }
        },
        _ => {}
    }
}

/// Show a notification and, when it has actions, wait for the user to pick
/// one (or dismiss it). Returns the chosen action key.
fn notify(summary: &str, body: &str, actions: &[(&str, &str)]) -> Option<String> {
    let r = (|| -> Result<Option<String>> {
        let conn = crate::dbus::session()?;
        let p = crate::dbus::Proxy::new(
            &conn,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
        )?;
        // Subscribe before sending so a fast click can't be missed.
        let mut invoked = (!actions.is_empty()).then(|| p.receive_signal("ActionInvoked")).transpose()?;
        let mut closed = (!actions.is_empty()).then(|| p.receive_signal("NotificationClosed")).transpose()?;
        let mut hints: HashMap<&str, Variant> = HashMap::new();
        hints.insert("desktop-entry", Variant::from("clipt"));
        let acts: Vec<&str> = actions.iter().flat_map(|(k, l)| [*k, *l]).collect();
        // Update offers stay until answered; plain results time out.
        let timeout = if actions.is_empty() { 6000i32 } else { 0 };
        let id: u32 = p.call("Notify", &("clipt", 0u32, "clipt", summary, body, acts, hints, timeout))?;
        let (Some(inv), Some(cl)) = (invoked.as_mut(), closed.as_mut()) else { return Ok(None) };
        // Two signal streams; whichever names our id first wins. Closing
        // happens after an action too, so read invoked on its own thread.
        let (tx, rx) = std::sync::mpsc::channel();
        let tx2 = tx.clone();
        std::thread::scope(|sc| {
            sc.spawn(|| {
                while let Some(m) = inv.next() {
                    if let Ok((nid, key)) = m.body().deserialize::<(u32, String)>()
                        && nid == id
                    {
                        let _ = tx.send(Some(key));
                        return;
                    }
                }
            });
            sc.spawn(|| {
                while let Some(m) = cl.next() {
                    if let Ok((nid, _)) = m.body().deserialize::<(u32, u32)>()
                        && nid == id
                    {
                        // Let an ActionInvoked that raced this one land first.
                        std::thread::sleep(Duration::from_millis(300));
                        let _ = tx2.send(None);
                        return;
                    }
                }
            });
            let got = rx.recv().ok().flatten();
            Ok(got)
        })
    })();
    r.unwrap_or_else(|e| {
        eprintln!("clipt tray: notification failed: {e:#}");
        None
    })
}

struct Item {
    save_dir: String,
    /// Drawn once; hosts pick the size closest to their panel.
    pixmaps: Pixmaps,
}

fn pixmaps() -> Pixmaps {
    [16, 22, 24, 32, 48, 64, 128].into_iter().map(|s| (s as i32, s as i32, crate::icon::argb32(s))).collect()
}

#[interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    fn activate(&self, _x: i32, _y: i32) {
        run_action(Action::Region, &self.save_dir);
    }

    fn secondary_activate(&self, _x: i32, _y: i32) {
        run_action(Action::Full, &self.save_dir);
    }

    fn context_menu(&self, _x: i32, _y: i32) {}

    fn scroll(&self, _delta: i32, _orientation: &str) {}

    #[zbus(property)]
    fn category(&self) -> &str {
        "ApplicationStatus"
    }

    #[zbus(property)]
    fn id(&self) -> &str {
        "clipt"
    }

    #[zbus(property)]
    fn title(&self) -> &str {
        "clipt"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "Active"
    }

    #[zbus(property)]
    fn window_id(&self) -> i32 {
        0
    }

    #[zbus(property)]
    fn icon_name(&self) -> &str {
        // Empty, so hosts use the pixmaps instead of a theme icon.
        ""
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> Pixmaps {
        self.pixmaps.clone()
    }

    #[zbus(property)]
    fn overlay_icon_name(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn attention_icon_name(&self) -> &str {
        ""
    }

    #[zbus(property, name = "ToolTip")]
    fn tool_tip(&self) -> (&str, Pixmaps, &str, &str) {
        ("", Vec::new(), "clipt — screenshot", "Click: capture a region\nMiddle-click: copy the whole desktop\nRight-click: more")
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(MENU_PATH).expect("valid path")
    }
}

struct Menu {
    save_dir: String,
}

type Props = HashMap<String, OwnedVariant>;

fn owned(v: Variant<'_>) -> OwnedVariant {
    OwnedVariant::try_from(v).expect("no fds in menu properties")
}

fn item_props(entry: Option<(&str, Action)>) -> Props {
    let mut p = Props::new();
    match entry {
        Some((label, _)) => {
            p.insert("label".into(), owned(Variant::from(label)));
        }
        None => {
            p.insert("type".into(), owned(Variant::from("separator")));
        }
    }
    p
}

fn root_props() -> Props {
    let mut p = Props::new();
    p.insert("children-display".into(), owned(Variant::from("submenu")));
    p
}

fn child_layout(id: i32, props: Props) -> OwnedVariant {
    let children: Vec<Variant<'static>> = Vec::new();
    let s: Structure<'static> =
        Structure::builder().add_field(id).add_field(props).add_field(children).build().expect("menu layout structure");
    owned(Variant::from(s))
}

#[interface(name = "com.canonical.dbusmenu")]
impl Menu {
    #[zbus(out_args("revision", "layout"))]
    fn get_layout(&self, parent_id: i32, _recursion_depth: i32, _property_names: Vec<String>) -> (u32, (i32, Props, Vec<OwnedVariant>)) {
        if parent_id != 0 {
            let props = menu().find(|(id, _)| *id == parent_id).map(|(_, e)| item_props(*e)).unwrap_or_default();
            return (1, (parent_id, props, Vec::new()));
        }
        let children = menu().map(|(id, e)| child_layout(*id, item_props(*e))).collect();
        (1, (0, root_props(), children))
    }

    fn get_group_properties(&self, ids: Vec<i32>, _property_names: Vec<String>) -> Vec<(i32, Props)> {
        menu().filter(|(id, _)| ids.is_empty() || ids.contains(id)).map(|(id, e)| (*id, item_props(*e))).collect()
    }

    fn get_property(&self, id: i32, name: &str) -> zbus::fdo::Result<OwnedVariant> {
        let props = if id == 0 { root_props() } else { menu().find(|(i, _)| *i == id).map(|(_, e)| item_props(*e)).unwrap_or_default() };
        props.get(name).map(|v| v.try_clone().expect("no fds")).ok_or_else(|| zbus::fdo::Error::InvalidArgs(name.into()))
    }

    fn event(&self, id: i32, event_id: &str, _data: OwnedVariant, _timestamp: u32) {
        if event_id != "clicked" {
            return;
        }
        if let Some((_, Some((_, a)))) = menu().find(|(i, _)| *i == id) {
            run_action(*a, &self.save_dir);
        }
    }

    fn event_group(&self, events: Vec<(i32, String, OwnedVariant, u32)>) -> Vec<i32> {
        for (id, ev, data, ts) in events {
            self.event(id, &ev, data, ts);
        }
        Vec::new()
    }

    fn about_to_show(&self, _id: i32) -> bool {
        false
    }

    fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        (Vec::new(), Vec::new())
    }

    #[zbus(signal)]
    async fn layout_updated(emitter: &SignalEmitter<'_>, revision: u32, parent: i32) -> zbus::Result<()>;

    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }
}

/// In a Flatpak there's no install.sh to write an autostart entry, so ask the
/// Background portal to start the tray with the desktop. The desktop asks the
/// user once and remembers; it's revoked in the desktop's app settings.
fn request_autostart() {
    let r = (|| -> Result<()> {
        let conn = crate::dbus::session()?;
        let p = crate::dbus::Proxy::new(
            &conn,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Background",
        )?;
        let mut opts: HashMap<&str, Variant> = HashMap::new();
        opts.insert("reason", Variant::from("Start the clipt tray icon when you log in"));
        opts.insert("autostart", Variant::from(true));
        opts.insert("commandline", Variant::from(vec!["clipt", "tray"]));
        let _: OwnedObjectPath = p.call("RequestBackground", &("", opts))?;
        Ok(())
    })();
    if let Err(e) = r {
        eprintln!("clipt tray: couldn't request autostart: {e:#}");
    }
}

async fn register(conn: &Connection, name: &str) -> zbus::Result<()> {
    conn.call_method(Some(WATCHER), "/StatusNotifierWatcher", Some(WATCHER), "RegisterStatusNotifierItem", &(name,)).await.map(drop)
}

pub fn run(save_dir: String, check_updates: bool) -> Result<()> {
    // Children are fire-and-forget; let the kernel reap them.
    // SAFETY: setting a signal disposition to SIG_IGN has no handler to race.
    unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) };

    zbus::block_on(async move {
        let conn = zbus::connection::Builder::session()
            .serve_at(ITEM_PATH, Item { save_dir: save_dir.clone(), pixmaps: pixmaps() })
            .serve_at(MENU_PATH, Menu { save_dir })
            .build()
            .await
            .context("connecting to the session bus")?;

        let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
        use zbus::fdo::{RequestNameFlags, RequestNameReply};
        // After a self-update the old tray's connection may still be closing.
        let tries = if std::env::var_os("CLIPT_TRAY_RESTART").is_some() { 20 } else { 1 };
        let lock = lock_name();
        let mut owned = false;
        for _ in 0..tries {
            if dbus.request_name(lock.as_str().try_into()?, RequestNameFlags::DO_NOT_QUEUE).await? == RequestNameReply::PrimaryOwner {
                owned = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        anyhow::ensure!(owned, "a clipt tray is already running");
        let flatpak = crate::update::flatpak();
        if flatpak {
            std::thread::spawn(request_autostart);
        }
        if check_updates && !flatpak {
            // Once shortly after login, then daily.
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_secs(60));
                loop {
                    updates(false);
                    std::thread::sleep(Duration::from_secs(24 * 3600));
                }
            });
        }

        // The spec's well-known name can't be owned from a Flatpak sandbox
        // (and its PID would be the sandbox's); watchers also accept the
        // connection's unique name, with the item at the default path.
        let name = if flatpak {
            conn.unique_name().context("no unique bus name")?.to_string()
        } else {
            let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
            conn.request_name(name.as_str()).await?;
            name
        };

        let mut owners = dbus.receive_name_owner_changed_with_args(&[(0, WATCHER)]).await?;
        if let Err(e) = register(&conn, &name).await {
            eprintln!("clipt tray: no tray host yet ({e}); waiting for one");
        }
        // Re-register whenever the panel (and its watcher) restarts.
        loop {
            let sig = std::future::poll_fn(|cx| futures_core::Stream::poll_next(std::pin::Pin::new(&mut owners), cx))
                .await
                .context("lost the session bus")?;
            let args = sig.args()?;
            if args.new_owner().is_some() {
                // Give the new watcher a moment to export its object.
                std::thread::sleep(Duration::from_millis(200));
                if let Err(e) = register(&conn, &name).await {
                    eprintln!("clipt tray: registering with the new tray host failed: {e}");
                }
            }
        }
    })
}
