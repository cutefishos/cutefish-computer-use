// An MCP server over stdio that lets an agent see the Cutefish desktop and use
// its pointer and keyboard, once the user allows it.

mod desktop;
mod image;
mod keys;
mod protocol;

use std::io::{self, BufRead, Write};
use std::thread::sleep;
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};

use desktop::Desktop;
use keys::{Key, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, KEY_LEFTALT, KEY_LEFTCTRL, KEY_LEFTMETA, KEY_LEFTSHIFT};

const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

// Text goes in bursts the application can take in before the next one: a
// toolkit that stops reading while it handles a long burst can overflow its
// connection, and a commit overtakes keys still queued.
const TEXT_BURST: usize = 32;
const TEXT_PAUSE: Duration = Duration::from_millis(100);

const INSTRUCTIONS: &str = "Controls the Cutefish desktop the user is sitting at. \
Take a screenshot first: every x and y is a pixel of the latest screenshot. \
The first action asks the user for permission. \
Input pauses while the user uses the pointer or keyboard, and system dialogs \
(permission and password prompts) are only for the user to answer.";

fn tools() -> Value {
    let point = |description: &str| {
        json!({ "type": "number", "description": description })
    };
    json!([
        {
            "name": "screenshot",
            "description": "Captures the whole desktop, every display, as one image. Coordinates of the other tools are pixels of the latest screenshot.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "click",
            "description": "Moves the pointer to a point and clicks.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": point("Screenshot pixel column."),
                    "y": point("Screenshot pixel row."),
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "default": "left" },
                    "count": { "type": "integer", "minimum": 1, "maximum": 3, "default": 1, "description": "2 for a double click." },
                    "modifiers": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["ctrl", "shift", "alt", "super"] },
                        "description": "Held during the click."
                    }
                },
                "required": ["x", "y"]
            }
        },
        {
            "name": "move_pointer",
            "description": "Moves the pointer to a point, e.g. to hover.",
            "inputSchema": {
                "type": "object",
                "properties": { "x": point("Screenshot pixel column."), "y": point("Screenshot pixel row.") },
                "required": ["x", "y"]
            }
        },
        {
            "name": "drag",
            "description": "Presses a button at one point, moves to another and releases it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "start_x": point("Where the drag starts."),
                    "start_y": point("Where the drag starts."),
                    "end_x": point("Where it ends."),
                    "end_y": point("Where it ends."),
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "default": "left" }
                },
                "required": ["start_x", "start_y", "end_x", "end_y"]
            }
        },
        {
            "name": "scroll",
            "description": "Scrolls by wheel clicks, at a point if one is given.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": point("Screenshot pixel column."),
                    "y": point("Screenshot pixel row."),
                    "vertical": { "type": "integer", "description": "Clicks; positive scrolls down." },
                    "horizontal": { "type": "integer", "description": "Clicks; positive scrolls right." }
                }
            }
        },
        {
            "name": "type_text",
            "description": "Types text into whatever has the keyboard focus. Line breaks are typed as Enter and tabs as Tab.",
            "inputSchema": {
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }
        },
        {
            "name": "press_keys",
            "description": "Presses a key or shortcut, e.g. \"Return\", \"ctrl+s\", \"ctrl+shift+t\", \"super\". Modifiers are ctrl, shift, alt and super (the Meta or Command key). A character names the key that types it on the current layout. Use type_text to enter text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "keys": { "type": "string" },
                    "repeat": { "type": "integer", "minimum": 1, "maximum": 50, "default": 1 }
                },
                "required": ["keys"]
            }
        },
        {
            "name": "wait",
            "description": "Waits, e.g. for an application to start.",
            "inputSchema": {
                "type": "object",
                "properties": { "seconds": { "type": "number", "minimum": 0, "maximum": 30 } },
                "required": ["seconds"]
            }
        }
    ])
}

fn text(message: impl Into<String>) -> Value {
    json!({ "content": [{ "type": "text", "text": message.into() }] })
}

fn failure(message: impl Into<String>) -> Value {
    json!({ "content": [{ "type": "text", "text": message.into() }], "isError": true })
}

fn number(arguments: &Value, name: &str) -> Result<f64, String> {
    arguments
        .get(name)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .ok_or_else(|| format!("\"{name}\" must be a number."))
}

fn optional_integer(arguments: &Value, name: &str, default: i64) -> Result<i64, String> {
    match arguments.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value.as_i64().ok_or_else(|| format!("\"{name}\" must be an integer.")),
    }
}

fn button(arguments: &Value) -> Result<u32, String> {
    match arguments.get("button").and_then(Value::as_str).unwrap_or("left") {
        "left" => Ok(BTN_LEFT),
        "right" => Ok(BTN_RIGHT),
        "middle" => Ok(BTN_MIDDLE),
        other => Err(format!("Unknown button \"{other}\".")),
    }
}

fn modifiers(arguments: &Value) -> Result<Vec<Key>, String> {
    let Some(list) = arguments.get("modifiers").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    list.iter()
        .map(|name| match name.as_str() {
            Some("ctrl") => Ok(Key::Code(KEY_LEFTCTRL)),
            Some("shift") => Ok(Key::Code(KEY_LEFTSHIFT)),
            Some("alt") => Ok(Key::Code(KEY_LEFTALT)),
            Some("super") => Ok(Key::Code(KEY_LEFTMETA)),
            _ => Err(format!("Unknown modifier {name}.")),
        })
        .collect()
}

// Pieces of at most TEXT_BURST characters, never between a CR and its LF.
fn chunks(text: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while let Some((mut end, _)) = rest.char_indices().nth(TEXT_BURST) {
        if rest[..end].ends_with('\r') {
            end -= 1;
        }
        let (piece, tail) = rest.split_at(end);
        pieces.push(piece);
        rest = tail;
    }
    if !rest.is_empty() {
        pieces.push(rest);
    }
    pieces
}

// Runs of text and runs of the control characters typed as keys, in order.
fn segments(text: &str) -> Vec<&str> {
    let control = |c: char| matches!(c, '\n' | '\r' | '\t' | '\u{8}');
    let mut parts = Vec::new();
    let mut start = 0;
    let mut previous = None;
    for (index, character) in text.char_indices() {
        let kind = control(character);
        if previous.is_some_and(|p| p != kind) {
            parts.push(&text[start..index]);
            start = index;
        }
        previous = Some(kind);
    }
    if start < text.len() {
        parts.push(&text[start..]);
    }
    parts
}

fn untypable(typed: bool) -> String {
    if typed {
        format!("{} The text before that was typed.", desktop::UNTYPABLE)
    } else {
        desktop::UNTYPABLE.to_string()
    }
}

struct Server {
    desktop: Result<Desktop, String>,
    agent_name: String,
}

impl Server {
    fn desktop(&mut self) -> Result<&mut Desktop, String> {
        let desktop = self.desktop.as_mut().map_err(|e| e.clone())?;
        desktop.agent_name = self.agent_name.clone();
        Ok(desktop)
    }

    fn call(&mut self, name: &str, arguments: &Value) -> Result<Value, String> {
        match name {
            "screenshot" => {
                let (data, mime_type, layout) = self.desktop()?.screenshot()?;
                Ok(json!({ "content": [
                    {
                        "type": "image",
                        "data": base64::engine::general_purpose::STANDARD.encode(data),
                        "mimeType": mime_type
                    },
                    {
                        "type": "text",
                        "text": format!("The desktop at {}x{} pixels.", layout.width, layout.height)
                    }
                ] }))
            }
            "click" => {
                let (x, y) = (number(arguments, "x")?, number(arguments, "y")?);
                let button = button(arguments)?;
                let count = optional_integer(arguments, "count", 1)?.clamp(1, 3);
                let modifiers = modifiers(arguments)?;
                let desktop = self.desktop()?;
                let point = desktop.to_logical(x, y)?;
                desktop.inject(&|batch| {
                    batch.motion(point);
                    batch.pause(20);
                    for &key in &modifiers {
                        batch.key(key, true);
                    }
                    for click in 0..count {
                        if click > 0 {
                            batch.pause(60);
                        }
                        batch.button(button, true);
                        batch.button(button, false);
                    }
                    for &key in modifiers.iter().rev() {
                        batch.key(key, false);
                    }
                })?;
                Ok(text("Clicked."))
            }
            "move_pointer" => {
                let (x, y) = (number(arguments, "x")?, number(arguments, "y")?);
                let desktop = self.desktop()?;
                let point = desktop.to_logical(x, y)?;
                desktop.inject(&|batch| batch.motion(point))?;
                Ok(text("Moved."))
            }
            "drag" => {
                let button = button(arguments)?;
                let desktop = self.desktop()?;
                let start = desktop.to_logical(number(arguments, "start_x")?, number(arguments, "start_y")?)?;
                let end = desktop.to_logical(number(arguments, "end_x")?, number(arguments, "end_y")?)?;
                desktop.inject(&|batch| {
                    const STEPS: u32 = 12;
                    batch.motion(start);
                    batch.pause(30);
                    batch.button(button, true);
                    batch.pause(50);
                    for step in 1..=STEPS {
                        let t = step as f64 / STEPS as f64;
                        batch.motion((start.0 + (end.0 - start.0) * t, start.1 + (end.1 - start.1) * t));
                        batch.pause(16);
                    }
                    batch.pause(50);
                    batch.button(button, false);
                })?;
                Ok(text("Dragged."))
            }
            "scroll" => {
                let vertical = optional_integer(arguments, "vertical", 0)?.clamp(-100, 100) as i32;
                let horizontal = optional_integer(arguments, "horizontal", 0)?.clamp(-100, 100) as i32;
                if vertical == 0 && horizontal == 0 {
                    return Err("Give \"vertical\" or \"horizontal\" clicks.".into());
                }
                let desktop = self.desktop()?;
                let point = match (arguments.get("x"), arguments.get("y")) {
                    (Some(_), Some(_)) => Some(desktop.to_logical(number(arguments, "x")?, number(arguments, "y")?)?),
                    _ => None,
                };
                desktop.inject(&|batch| {
                    if let Some(point) = point {
                        batch.motion(point);
                        batch.pause(20);
                    }
                    batch.axis(horizontal, vertical);
                })?;
                Ok(text("Scrolled."))
            }
            "type_text" => {
                let text_argument = arguments
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or("\"text\" must be a string.")?;
                let desktop = self.desktop()?;
                // Each piece on its own, so a paused piece is not typed twice.
                // The pause between pieces lets the application take the keys
                // of one before the next arrives as a commit, which overtakes
                // them.
                let mut typed = false;
                for piece in chunks(text_argument) {
                    if typed {
                        sleep(TEXT_PAUSE);
                    }
                    if desktop.try_inject(&|batch| batch.type_text(piece))? {
                        typed = true;
                        continue;
                    }
                    // A string the layout cannot type goes as one commit, which
                    // cannot also carry Enter, Tab or Backspace: send those apart.
                    let parts = segments(piece);
                    if parts.len() == 1 {
                        return Err(untypable(typed));
                    }
                    for part in parts {
                        if typed {
                            sleep(TEXT_PAUSE);
                        }
                        if !desktop.try_inject(&|batch| batch.type_text(part))? {
                            return Err(untypable(typed));
                        }
                        typed = true;
                    }
                }
                Ok(text("Typed."))
            }
            "press_keys" => {
                let chord = arguments
                    .get("keys")
                    .and_then(Value::as_str)
                    .ok_or("\"keys\" must be a string.")?;
                let keys = keys::parse_chord(chord)?;
                let repeat = optional_integer(arguments, "repeat", 1)?.clamp(1, 50);
                let desktop = self.desktop()?;
                for _ in 0..repeat {
                    desktop.inject(&|batch| {
                        for &key in &keys {
                            batch.key(key, true);
                        }
                        for &key in keys.iter().rev() {
                            batch.key(key, false);
                        }
                    })?;
                }
                Ok(text("Pressed."))
            }
            "wait" => {
                let seconds = number(arguments, "seconds")?.clamp(0.0, 30.0);
                sleep(Duration::from_secs_f64(seconds));
                Ok(text("Waited."))
            }
            _ => Err(format!("Unknown tool \"{name}\".")),
        }
    }

    // The response to one JSON-RPC message; nothing for a notification.
    fn handle(&mut self, message: &Value) -> Option<Value> {
        let id = message.get("id").cloned()?;
        let method = message.get("method").and_then(Value::as_str).unwrap_or_default();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => {
                let client = &params["clientInfo"];
                if let Some(name) = client["title"].as_str().or(client["name"].as_str()) {
                    self.agent_name = name.to_string();
                }
                let requested = params["protocolVersion"].as_str().unwrap_or_default();
                let version = PROTOCOL_VERSIONS
                    .iter()
                    .find(|v| **v == requested)
                    .unwrap_or(&PROTOCOL_VERSIONS[0]);
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "cutefish-computer-use", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": INSTRUCTIONS
                })
            }
            "ping" => json!({}),
            "tools/list" => json!({ "tools": tools() }),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default();
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                self.call(name, &arguments).unwrap_or_else(failure)
            }
            _ => {
                return Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("Method not found: {method}") }
                }));
            }
        };
        Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }
}

fn main() {
    let desktop = Desktop::connect();
    // Only now: the compositor has identified this process by /proc, which a
    // non-dumpable process hides. From here, same-user processes cannot ptrace
    // it or read its memory.
    // SAFETY: prctl with integer arguments only.
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
    }
    if let Err(error) = &desktop {
        eprintln!("cutefish-computer-use: {error}");
    }
    let mut server = Server { desktop, agent_name: String::new() };

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(message) => server.handle(&message),
            Err(_) => Some(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32700, "message": "Parse error" }
            })),
        };
        if let Some(response) = response {
            if writeln!(stdout, "{response}").and_then(|_| stdout.flush()).is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_text_is_split_on_characters_and_line_breaks() {
        let text = format!("{}\r\n{}", "a".repeat(TEXT_BURST - 1), "é".repeat(TEXT_BURST + 1));
        let pieces = chunks(&text);
        assert_eq!(pieces.concat(), text);
        assert!(pieces.iter().all(|p| p.chars().count() <= TEXT_BURST && !p.ends_with('\r')));
        assert!(pieces[1].starts_with('\r'));
        assert_eq!(pieces.len(), 3);
    }

    #[test]
    fn text_is_segmented_at_keys() {
        assert_eq!(segments("abc"), ["abc"]);
        assert_eq!(segments("你好\r\n\tä\u{8}"), ["你好", "\r\n\t", "ä", "\u{8}"]);
        assert_eq!(segments("\nx"), ["\n", "x"]);
        assert!(segments("").is_empty());
    }
}
