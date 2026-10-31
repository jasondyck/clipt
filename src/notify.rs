//! org.freedesktop.Notifications, fire-and-forget.

use std::collections::HashMap;
use zbus::Variant as Value;

pub fn send(summary: &str, body: &str, image: Option<&std::path::Path>) {
    let r = (|| -> zbus::Result<()> {
        let conn = crate::dbus::session()?;
        let mut hints: HashMap<&str, Value> = HashMap::new();
        hints.insert("desktop-entry", Value::from("glint"));
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
            &("glint", 0u32, icon.as_str(), summary, body, actions, hints, 4000i32),
        )?;
        Ok(())
    })();
    if let Err(e) = r {
        eprintln!("glint: notification failed: {e}");
    }
}
