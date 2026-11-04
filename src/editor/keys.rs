//! Every key and click the editor understands, in one list: the `?` sheet
//! draws it, and a test keeps the README's table the same, so neither falls
//! behind the code.

use super::shapes::Tool;

/// A shortcut with the command key: Cmd on macOS (whose hosts report it as
/// `Mods::ctrl`), Ctrl elsewhere.
macro_rules! cmd {
    ($k:literal) => {
        if cfg!(target_os = "macos") { concat!("Cmd+", $k) } else { concat!("Ctrl+", $k) }
    };
}
pub(crate) use cmd;

macro_rules! alt {
    ($k:literal) => {
        if cfg!(target_os = "macos") { concat!("Option+", $k) } else { concat!("Alt+", $k) }
    };
}

pub struct Group {
    pub title: &'static str,
    /// Which column of the sheet it goes in.
    pub column: usize,
    pub keys: Vec<(String, &'static str)>,
}

fn group(title: &'static str, column: usize, keys: &[(&str, &'static str)]) -> Group {
    Group { title, column, keys: keys.iter().map(|&(k, t)| (k.to_string(), t)).collect() }
}

/// The sheet for the editor as it is: screenshot or record mode, whether it
/// can switch between them, and whether it knows where windows are.
pub fn sheet(record: bool, can_switch: bool, windows: bool, doc: bool) -> Vec<Group> {
    let mut select = vec![("Drag", "Select an area")];
    if doc {
        select.insert(0, ("Scroll", "Move through the page"));
    }
    if windows {
        select.extend([("Click", "The highlighted window or screen"), ("Tab", "Window or its whole screen")]);
    } else {
        select.push(("Click", "The screen under the pointer"));
    }
    select.extend([
        (cmd!("A"), "The whole screen"),
        ("Shift+drag", "A square"),
        ("Arrows", "Move it a pixel"),
        (alt!("Arrows"), "Move it 10 pixels"),
        ("Shift+Arrows", "Resize it"),
    ]);
    let select = group("Select", 0, &select);

    if record {
        let mut keys = vec![
            ("S", "Computer sound on or off"),
            ("M", "Microphone on or off"),
            ("G", "GIF or video"),
            ("Enter", "Start recording"),
            ("Right-click", "Select again"),
        ];
        if can_switch {
            keys.push((cmd!("R"), "A screenshot instead"));
        }
        keys.extend([("Esc", "Cancel"), ("?", "Show or hide the shortcuts")]);
        return vec![select, group("Record", 1, &keys)];
    }

    let mut tools = Group {
        title: "Tools",
        column: 1,
        keys: std::iter::once(Tool::Select).chain(Tool::DRAWING).map(|t| (t.hotkey().to_ascii_uppercase().to_string(), t.name())).collect(),
    };
    tools.keys.push(("Shift".into(), "Square, circle or 45° line"));
    // In a document the wheel scrolls, and changes the width with Ctrl.
    let width_wheel = if doc { cmd!("Scroll") } else { "Scroll" };
    let style = group(
        "Style",
        2,
        &[
            ("1–8", "Colour"),
            ("Right-click", "On a colour: change it"),
            ("[", "Thinner"),
            ("]", "Thicker"),
            (width_wheel, "Thinner or thicker"),
        ],
    );
    let edit = group(
        "Edit",
        2,
        &[
            ("V, click", "Pick a shape to move or restyle"),
            ("Delete", "Remove the picked shape"),
            ("V, double-click", "Edit text"),
            ("Shift+Enter", "New line in text"),
            (cmd!("Z"), "Undo"),
            (cmd!("Shift+Z"), "Redo"),
            ("Right-click", "Back out a step"),
        ],
    );
    let mut done = vec![("Enter", "Copy"), ("Double-click", "Copy"), (cmd!("C"), "Copy"), (cmd!("S"), "Save")];
    if can_switch {
        done.push((cmd!("R"), "Record a video instead"));
    }
    done.extend([("Esc", "Cancel"), ("?", "Show or hide the shortcuts")]);
    vec![select, tools, style, edit, group("Finish", 0, &done)]
}

/// The README's table: the screenshot editor's sheet, then record mode's own
/// keys. Labels are the Linux and Windows ones; the README says Cmd for Mac.
#[cfg(test)]
pub fn markdown() -> String {
    let mut groups = sheet(false, true, true, false);
    groups.extend(sheet(true, true, true, false).into_iter().filter(|g| g.title == "Record"));
    let mut out = String::from("| Key | |\n|---|---|\n");
    for g in groups {
        out += &format!("| **{}** | |\n", g.title);
        for (k, t) in g.keys {
            out += &format!("| <kbd>{k}</kbd> | {t} |\n");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The README's table is the sheet. If this fails, paste the table it
    /// prints between the README's `shortcuts` markers.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn readme_lists_every_shortcut() {
        // Windows checkouts may have CRLF line endings.
        let readme = include_str!("../../README.md").replace('\r', "");
        let start = "<!-- shortcuts: generated from src/editor/keys.rs -->\n";
        let end = "<!-- /shortcuts -->";
        let table = readme.split_once(start).and_then(|(_, r)| r.split_once(end)).map(|(t, _)| t);
        let want = markdown();
        assert_eq!(table, Some(want.as_str()), "the README's shortcuts table is out of date; it should be:\n\n{want}");
    }

    #[test]
    fn record_mode_lists_only_what_works_there() {
        let groups = sheet(true, false, false, false);
        assert_eq!(groups.iter().map(|g| g.title).collect::<Vec<_>>(), ["Select", "Record"]);
        let all: Vec<&str> = groups.iter().flat_map(|g| g.keys.iter().map(|(k, _)| k.as_str())).collect();
        assert!(!all.contains(&"Tab"), "no windows to switch from");
        assert!(!all.iter().any(|k| k.ends_with("+R")), "clipt record can't switch to a screenshot");
    }
}
