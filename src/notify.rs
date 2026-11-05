//! Desktop notifications, fire-and-forget: org.freedesktop.Notifications
//! on Wayland and X11, AppleScript on macOS (`macos::notify`), the tray
//! icon's balloon on Windows (`win32::notify`).

/// Show a notification; `image`, a saved file, is its icon where the
/// desktop shows one.
pub fn send(summary: &str, body: &str, image: Option<&std::path::Path>) {
    #[cfg(wayland_x11)]
    freedesktop(summary, body, image);
    #[cfg(target_os = "macos")]
    {
        let _ = image;
        crate::macos::notify(summary, body);
    }
    #[cfg(windows)]
    crate::win32::notify(summary, body, image);
}

#[cfg(wayland_x11)]
fn freedesktop(summary: &str, body: &str, image: Option<&std::path::Path>) {
    use std::collections::HashMap;
    use zbus::Variant as Value;
    let r = (|| -> zbus::Result<()> {
        let conn = crate::dbus::session()?;
        let mut hints: HashMap<&str, Value> = HashMap::new();
        hints.insert("desktop-entry", Value::from("clipt"));
        let icon = image.map(|p| p.display().to_string()).unwrap_or_else(|| "camera-photo".into());
        if let Some(p) = image {
            hints.insert("image-path", Value::from(p.display().to_string()));
        }
        let actions: Vec<&str> = Vec::new();
        crate::dbus::call_method(
            &conn,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
            "Notify",
            &("clipt", 0u32, icon.as_str(), summary, body, actions, hints, 4000i32),
        )?;
        Ok(())
    })();
    if let Err(e) = r {
        eprintln!("clipt: notification failed: {e}");
    }
}
