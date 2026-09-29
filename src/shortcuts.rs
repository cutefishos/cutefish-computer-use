// The session's keyboard shortcuts, from the compositor's
// com.cutefish.Shortcuts1, written as the chords press_keys takes.

use serde_json::Value;

// Accelerator modifier bits (the compositor's ui::EventFlags subset).
const SHIFT: u64 = 1 << 0;
const CONTROL: u64 = 1 << 1;
const ALT: u64 = 1 << 2;
const META: u64 = 1 << 3;

fn convention_text(convention: &str) -> &'static str {
    if convention == "command" {
        "The shortcut key is Command, which press_keys calls super. Applications \
get super and ctrl both as Ctrl: super+c copies, super+v pastes, super+z undoes. \
In a terminal super+c and super+v copy and paste, and ctrl+c interrupts."
    } else {
        "The shortcut key is Ctrl: applications use ctrl+c, ctrl+v and ctrl+z; \
terminals copy and paste with ctrl+shift+c and ctrl+shift+v."
    }
}

fn chord(accelerator: &Value) -> Option<String> {
    let key = accelerator.get("keyName")?.as_str().filter(|key| !key.is_empty())?;
    let modifiers = accelerator.get("modifiers")?.as_u64()?;
    let mut parts = Vec::new();
    for (bit, name) in [(CONTROL, "ctrl"), (ALT, "alt"), (SHIFT, "shift"), (META, "super")] {
        if modifiers & bit != 0 {
            parts.push(name);
        }
    }
    // A release shortcut is a modifier tapped on its own.
    if accelerator.get("keyState").and_then(Value::as_str) == Some("released") {
        return Some(format!("{} (tapped alone)", key.to_lowercase()));
    }
    parts.push(key);
    Some(parts.join("+"))
}

// `accelerators` is the JSON Shortcuts1.accelerators answers.
pub fn describe(convention: &str, accelerators: &str) -> String {
    let mut lines = vec![convention_text(convention).to_string()];
    let actions: Value = serde_json::from_str(accelerators).unwrap_or(Value::Null);
    let mut shortcuts = Vec::new();
    for action in actions.get("actions").and_then(Value::as_array).into_iter().flatten() {
        let Some(name) = action.get("name").and_then(Value::as_str) else {
            continue;
        };
        let chords: Vec<String> = action
            .get("accelerators")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(chord)
            .collect();
        if !chords.is_empty() {
            shortcuts.push(format!("{name}: {}", chords.join(", ")));
        }
    }
    if !shortcuts.is_empty() {
        lines.push("System shortcuts (desks count from 0):".to_string());
        lines.extend(shortcuts);
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_are_written_as_press_keys_chords() {
        let json = r#"{"actions":[
            {"name":"toggle-app-list","accelerators":[{"keyName":"Super_L","modifiers":8,"keyState":"released"}]},
            {"name":"close-window","accelerators":[]},
            {"name":"cycle-backward-mru","accelerators":[{"keyName":"Tab","modifiers":9,"keyState":"pressed"}]},
            {"name":"take-screenshot","accelerators":[{"keyName":"3","modifiers":9},{"keyName":"Print","modifiers":1}]},
            {"name":"lock-screen","accelerators":[{"keyName":"q","modifiers":10}]}
        ]}"#;
        let text = describe("command", json);
        let lines: Vec<&str> = text.lines().skip(2).collect();
        assert_eq!(
            lines,
            [
                "toggle-app-list: super_l (tapped alone)",
                "cycle-backward-mru: shift+super+Tab",
                "take-screenshot: shift+super+3, shift+Print",
                "lock-screen: ctrl+super+q",
            ]
        );
        assert!(text.starts_with("The shortcut key is Command"));
        assert!(describe("pc", "").starts_with("The shortcut key is Ctrl"));
    }
}
