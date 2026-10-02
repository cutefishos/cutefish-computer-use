// An MCP server over stdio that lets an agent see the Cutefish desktop and use
// its pointer and keyboard, once the user allows it.

mod apps;
mod desktop;
mod image;
mod keys;
mod protocol;
mod session;
mod shortcuts;

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufRead, Write};
use std::os::fd::FromRawFd;
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{json, Value};

use desktop::{Desktop, Window};
use keys::{Key, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, KEY_LEFTALT, KEY_LEFTCTRL, KEY_LEFTMETA, KEY_LEFTSHIFT};

const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

// Text goes in bursts the application can take in before the next one: a
// toolkit that stops reading while it handles a long burst can overflow its
// connection, and a commit overtakes keys still queued.
const TEXT_BURST: usize = 32;
const TEXT_PAUSE: Duration = Duration::from_millis(100);
// The longest to wait for the screen to settle after a window change or
// before a screenshot in actions; something that keeps animating costs this.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(1);
// How long a started application gets to show a window before the tool returns.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(5);
const LAUNCH_POLL: Duration = Duration::from_millis(250);
const MAX_HOLD_SECONDS: f64 = 10.0;
const MAX_DELAY_SECONDS: f64 = 30.0;
const READ_ONLY_TOOLS: &[&str] = &[
    "screenshot",
    "zoom",
    "cursor_position",
    "keyboard_shortcuts",
    "list_windows",
    "list_applications",
];
// The compositor reads no more than this.
const MAX_CLIPBOARD_BYTES: usize = 16 * 1024 * 1024;
const MAX_ACTION_STEPS: usize = 50;

const INSTRUCTIONS: &str = "Controls the Cutefish desktop the user is sitting at. \
Take a screenshot first: every x and y is a pixel of the latest screenshot. \
A screenshot of one window shows it at full resolution in fewer pixels. \
Use zoom to read small text. Prefer open_application, open and \
activate_window over clicking through the desktop to reach an application, \
and close_window and set_window_frame over dragging window edges. \
Call keyboard_shortcuts before using shortcuts: the shortcut key may be \
Command (super) or Ctrl. For long or multi-line text, or an editor that \
indents and closes brackets by itself, set_clipboard and paste instead of \
type_text. \
When the next steps need no look at the screen in between, run them in one \
actions call, e.g. click a field, type, press Return and take a screenshot. \
The first action asks the user for permission. \
Input pauses while the user uses the pointer or keyboard, and system dialogs \
(permission and password prompts) are only for the user to answer.";

fn tools() -> Value {
    let point = |description: &str| {
        json!({ "type": "number", "description": description })
    };
    let mut tools = json!([
        {
            "name": "screenshot",
            "description": "Captures the whole desktop, every display, as one image, or only one window's frame at up to the display's full resolution. Coordinates of the other tools are pixels of the latest screenshot, a window's one included. Give a delay to let an application start or a page load first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "delay": { "type": "number", "minimum": 0, "maximum": MAX_DELAY_SECONDS, "description": "Seconds to wait before capturing." },
                    "window": { "type": "integer", "description": "A window number from list_windows. Whatever covers the window shows too: activate it first." }
                }
            }
        },
        {
            "name": "zoom",
            "description": "Captures the screen again and shows one region of it at up to the display's full resolution, to read small text. No screenshot is needed first. The region is in pixels of the latest screenshot, and the other tools' coordinates still refer to that screenshot.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": point("Left edge, a screenshot pixel column."),
                    "y": point("Top edge, a screenshot pixel row."),
                    "width": point("In screenshot pixels."),
                    "height": point("In screenshot pixels.")
                },
                "required": ["x", "y", "width", "height"]
            }
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
            "name": "mouse_down",
            "description": "Presses a mouse button and keeps it pressed, at a point if one is given; mouse_up releases it. For drags that drag cannot do. The user's own input releases it too.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": point("Screenshot pixel column."),
                    "y": point("Screenshot pixel row."),
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "default": "left" }
                }
            }
        },
        {
            "name": "mouse_up",
            "description": "Releases a mouse button mouse_down pressed, at a point if one is given.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": point("Screenshot pixel column."),
                    "y": point("Screenshot pixel row."),
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "default": "left" }
                }
            }
        },
        {
            "name": "cursor_position",
            "description": "Tells where the pointer is, in pixels of the latest screenshot.",
            "inputSchema": { "type": "object", "properties": {} }
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
            "name": "hold_keys",
            "description": "Holds a key or shortcut down for a while, then releases it, e.g. to repeat an arrow key. Keys are named as for press_keys.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "keys": { "type": "string" },
                    "seconds": { "type": "number", "minimum": 0, "maximum": MAX_HOLD_SECONDS }
                },
                "required": ["keys", "seconds"]
            }
        },
        {
            "name": "set_clipboard",
            "description": "Puts text on the clipboard, replacing what the user copied, to paste with the application's paste shortcut. Faster and more exact than type_text for long or multi-line text.",
            "inputSchema": {
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }
        },
        {
            "name": "keyboard_shortcuts",
            "description": "Tells which key applications use for shortcuts (Command or Ctrl) and lists the system shortcuts, as press_keys chords.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "list_windows",
            "description": "Lists the application windows on every desk, most recently used first, with their numbers for the window tools and their frames in screenshot pixels.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "activate_window",
            "description": "Brings a window forward and gives it the keyboard focus, restoring it if it is minimized and switching to its desk.",
            "inputSchema": {
                "type": "object",
                "properties": { "window": { "type": "integer", "description": "The number list_windows gave it." } },
                "required": ["window"]
            }
        },
        {
            "name": "close_window",
            "description": "Asks a window to close, as its close button does. The application may ask about unsaved work first: take a screenshot afterwards.",
            "inputSchema": {
                "type": "object",
                "properties": { "window": { "type": "integer", "description": "The number list_windows gave it." } },
                "required": ["window"]
            }
        },
        {
            "name": "set_window_frame",
            "description": "Moves and resizes a window's frame, in screenshot pixels. A maximized or full-screen window becomes a normal one; the window keeps to its size limits and stays on screen enough to grab. Returns the frame it got.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": { "type": "integer", "description": "The number list_windows gave it." },
                    "x": point("Left edge of the frame."),
                    "y": point("Top edge of the frame."),
                    "width": point("Frame width."),
                    "height": point("Frame height.")
                },
                "required": ["window", "x", "y", "width", "height"]
            }
        },
        {
            "name": "list_applications",
            "description": "Lists the installed applications, by name and id.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "open_application",
            "description": "Starts an installed application and waits a few seconds for its window.",
            "inputSchema": {
                "type": "object",
                "properties": { "name": { "type": "string", "description": "Its name or id, as list_applications gives them." } },
                "required": ["name"]
            }
        },
        {
            "name": "open",
            "description": "Opens a file, folder or URL in the application that handles it.",
            "inputSchema": {
                "type": "object",
                "properties": { "target": { "type": "string", "description": "An absolute path, a path starting with ~/, or a URL." } },
                "required": ["target"]
            }
        }
    ]);
    let names: Vec<Value> = tools.as_array().into_iter().flatten().map(|tool| tool["name"].clone()).collect();
    tools.as_array_mut().unwrap().push(json!({
        "name": "actions",
        "description": "Runs several of the other tools in order in one call, to save a round trip for each. Use it for steps that need no look at the screen in between, and end with a screenshot to see the result. Stops at the first step that fails and says which one. Only the image of the last screenshot or zoom step is returned.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "steps": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_ACTION_STEPS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "tool": { "type": "string", "enum": names },
                            "arguments": { "type": "object", "description": "The arguments as that tool's own schema names them." }
                        },
                        "required": ["tool"]
                    }
                }
            },
            "required": ["steps"]
        }
    }));
    // Agent apps may ask the user before each tool; these only look.
    for tool in tools.as_array_mut().into_iter().flatten() {
        if READ_ONLY_TOOLS.contains(&tool["name"].as_str().unwrap_or_default()) {
            tool["annotations"] = json!({ "readOnlyHint": true });
        }
    }
    tools
}

fn text(message: impl Into<String>) -> Value {
    json!({ "content": [{ "type": "text", "text": message.into() }] })
}

fn failure(message: impl Into<String>) -> Value {
    json!({ "content": [{ "type": "text", "text": message.into() }], "isError": true })
}

fn captioned_image(data: Vec<u8>, mime_type: &str, caption: String) -> Value {
    json!({ "content": [
        {
            "type": "image",
            "data": base64::engine::general_purpose::STANDARD.encode(data),
            "mimeType": mime_type
        },
        {
            "type": "text",
            "text": caption
        }
    ] })
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

// A memfd holding `bytes`, read from the start.
fn memory_file(bytes: &[u8]) -> Result<File, String> {
    let failed = |_| "Could not hold the text.".to_string();
    // SAFETY: the name is a NUL-terminated literal.
    let fd = unsafe { libc::memfd_create(c"cutefish-clipboard".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err("Could not hold the text.".into());
    }
    // SAFETY: memfd_create returned a new descriptor that nothing else owns.
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(bytes).map_err(failed)?;
    Ok(file)
}

fn window_handle(arguments: &Value) -> Result<u32, String> {
    arguments
        .get("window")
        .and_then(Value::as_u64)
        .and_then(|handle| u32::try_from(handle).ok())
        .ok_or_else(|| "\"window\" must be a number from list_windows.".to_string())
}

// A point, when both x and y are given.
fn optional_point(desktop: &mut Desktop, arguments: &Value) -> Result<Option<(f64, f64)>, String> {
    match (arguments.get("x"), arguments.get("y")) {
        (None | Some(Value::Null), None | Some(Value::Null)) => Ok(None),
        (Some(_), Some(_)) => {
            Ok(Some(desktop.to_logical(number(arguments, "x")?, number(arguments, "y")?)?))
        }
        _ => Ok(None),
    }
}

fn string<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, String> {
    arguments.get(name).and_then(Value::as_str).ok_or_else(|| format!("\"{name}\" must be a string."))
}

fn describe_window(window: &Window, apps: &[apps::App], layout: &image::Layout) -> String {
    let mut line = format!("{}. \"{}\"", window.handle, window.title);
    match apps::for_window(apps, &window.app_id) {
        Some(app) => line += &format!(", {} ({})", app.display_name(), app.id),
        None if !window.app_id.is_empty() => line += &format!(", {}", window.app_id),
        None => {}
    }
    let mut states = Vec::new();
    if window.active {
        states.push("active");
    }
    if window.minimized {
        states.push("minimized");
    }
    if window.hidden {
        states.push("hidden");
    }
    if window.other_desk {
        states.push("on another desk");
    }
    for state in &states {
        line += &format!(", {state}");
    }
    if !window.minimized && !window.hidden && !window.other_desk {
        let rect = window.rect;
        let (left, top) = layout.to_image_point(rect.x as f64, rect.y as f64);
        let (right, bottom) =
            layout.to_image_point((rect.x + rect.width) as f64, (rect.y + rect.height) as f64);
        line += &format!(
            ", at {},{} size {}x{}",
            left.round(),
            top.round(),
            (right - left).round(),
            (bottom - top).round()
        );
    }
    line
}

// A file target as an absolute path, or a URL as it is.
fn open_target(target: &str) -> Result<String, String> {
    let target = target.trim();
    if let Some((scheme, _)) = target.split_once(':') {
        let is_scheme = scheme.len() > 1
            && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        if is_scheme {
            return Ok(target.to_string());
        }
    }
    let path = match target.strip_prefix("~/") {
        Some(rest) => {
            let home = session::variable("HOME").ok_or("The home folder is unknown.")?;
            Path::new(&home).join(rest)
        }
        None => Path::new(target).to_path_buf(),
    };
    if !path.is_absolute() {
        return Err("Give an absolute path, one starting with ~/, or a URL.".into());
    }
    if !path.exists() {
        return Err(format!("{} does not exist.", path.display()));
    }
    Ok(path.to_string_lossy().into_owned())
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
}

impl Server {
    fn desktop(&mut self) -> Result<&mut Desktop, String> {
        self.desktop.as_mut().map_err(|e| e.clone())
    }

    // Waits for a window that is not among `before`, and says what came up.
    fn new_window(&mut self, before: &HashSet<u32>) -> Result<String, String> {
        let deadline = Instant::now() + LAUNCH_TIMEOUT;
        let desktop = self.desktop()?;
        loop {
            sleep(LAUNCH_POLL);
            let windows = desktop.windows()?;
            if let Some(window) = windows.iter().find(|w| !before.contains(&w.handle)) {
                let layout = desktop.layout()?;
                return Ok(format!(
                    "Its window: {}.",
                    describe_window(window, &apps::named(), &layout)
                ));
            }
            if Instant::now() >= deadline {
                return Ok("No new window has shown yet; take a screenshot.".into());
            }
        }
    }

    fn handles(&mut self) -> Result<HashSet<u32>, String> {
        Ok(self.desktop()?.windows()?.iter().map(|w| w.handle).collect())
    }

    // Each step's text, numbered, and the image of the last step that had one:
    // the agent app refuses a result that is too large.
    fn actions(&mut self, arguments: &Value) -> Result<Value, String> {
        let steps = arguments
            .get("steps")
            .and_then(Value::as_array)
            .filter(|steps| !steps.is_empty())
            .ok_or("\"steps\" must be a list of steps.")?;
        if steps.len() > MAX_ACTION_STEPS {
            return Err(format!("Give at most {MAX_ACTION_STEPS} steps."));
        }
        // A misspelt tool fails the call before any step runs.
        let tools = tools();
        let known = |name: &str| tools.as_array().into_iter().flatten().any(|tool| tool["name"] == name);
        for (index, step) in steps.iter().enumerate() {
            let name = step.get("tool").and_then(Value::as_str).unwrap_or_default();
            if name == "actions" || !known(name) {
                return Err(format!("Step {}: \"{name}\" is not a tool actions can run.", index + 1));
            }
        }

        let no_arguments = json!({});
        let mut lines = Vec::new();
        let mut image = None;
        let mut failed = false;
        let mut acted = false;
        for (index, step) in steps.iter().enumerate() {
            let number = index + 1;
            let name = step["tool"].as_str().unwrap_or_default();
            let step_arguments = step.get("arguments").unwrap_or(&no_arguments);
            // An application shows input a moment after it arrives, which the
            // agent's own round trip covers between single calls.
            if matches!(name, "screenshot" | "zoom") {
                if acted && step_arguments.get("delay").is_none_or(Value::is_null) {
                    self.desktop()?.settle(SETTLE_TIMEOUT)?;
                }
                acted = false;
            } else if !READ_ONLY_TOOLS.contains(&name) {
                acted = true;
            }
            match self.call(name, step_arguments) {
                Ok(result) => {
                    let mut texts = Vec::new();
                    for block in result["content"].as_array().into_iter().flatten() {
                        if block["type"] == "image" {
                            image = Some(block.clone());
                        } else if let Some(line) = block["text"].as_str() {
                            texts.push(line);
                        }
                    }
                    lines.push(format!("{number}. {name}: {}", texts.join(" ")));
                }
                Err(error) => {
                    lines.push(format!("{number}. {name} failed: {error}"));
                    match steps.len() - number {
                        0 => {}
                        1 => lines.push(format!("Step {} did not run.", steps.len())),
                        _ => lines.push(format!("Steps {} to {} did not run.", number + 1, steps.len())),
                    }
                    failed = true;
                    break;
                }
            }
        }
        let mut content: Vec<Value> = image.into_iter().collect();
        content.push(json!({ "type": "text", "text": lines.join("\n") }));
        Ok(json!({ "content": content, "isError": failed }))
    }

    fn call(&mut self, name: &str, arguments: &Value) -> Result<Value, String> {
        match name {
            "screenshot" => {
                let delay = match arguments.get("delay") {
                    None | Some(Value::Null) => 0.0,
                    Some(_) => number(arguments, "delay")?.clamp(0.0, MAX_DELAY_SECONDS),
                };
                sleep(Duration::from_secs_f64(delay));
                let desktop = self.desktop()?;
                let window = match arguments.get("window") {
                    None | Some(Value::Null) => None,
                    Some(_) => {
                        let handle = window_handle(arguments)?;
                        let windows = desktop.windows()?;
                        let window = windows
                            .into_iter()
                            .find(|window| window.handle == handle)
                            .ok_or("That window is gone; list the windows again.")?;
                        if window.minimized || window.hidden || window.other_desk {
                            return Err("That window is not on screen: activate it first.".into());
                        }
                        Some(window)
                    }
                };
                let (data, mime_type, layout) = match &window {
                    Some(window) => desktop.window_screenshot(window.rect)?,
                    None => desktop.screenshot()?,
                };
                let caption = match &window {
                    Some(window) => format!(
                        "Window {} \"{}\" at {}x{} pixels. The other tools' coordinates now refer to this image.",
                        window.handle, window.title, layout.width, layout.height
                    ),
                    None => format!("The desktop at {}x{} pixels.", layout.width, layout.height),
                };
                Ok(captioned_image(data, mime_type, caption))
            }
            "zoom" => {
                let (x, y) = (number(arguments, "x")?, number(arguments, "y")?);
                let (width, height) = (number(arguments, "width")?, number(arguments, "height")?);
                if width < 1.0 || height < 1.0 {
                    return Err("\"width\" and \"height\" must be at least 1.".into());
                }
                let (data, mime_type, layout) = self.desktop()?.zoom(x, y, width, height)?;
                let caption = format!(
                    "Screenshot pixels {x},{y} to {},{} at {}x{} pixels. Coordinates still refer to the screenshot.",
                    x + width,
                    y + height,
                    layout.width,
                    layout.height
                );
                Ok(captioned_image(data, mime_type, caption))
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
            "mouse_down" | "mouse_up" => {
                let button = button(arguments)?;
                let pressed = name == "mouse_down";
                let desktop = self.desktop()?;
                let point = optional_point(desktop, arguments)?;
                desktop.inject(&|batch| {
                    if let Some(point) = point {
                        batch.motion(point);
                        batch.pause(20);
                    }
                    batch.button(button, pressed);
                })?;
                Ok(text(if pressed { "Pressed." } else { "Released." }))
            }
            "cursor_position" => {
                let desktop = self.desktop()?;
                let (x, y) = desktop.pointer()?;
                let (x, y) = desktop.layout()?.to_image_point(x, y);
                Ok(text(format!("The pointer is at {},{}.", x.floor(), y.floor())))
            }
            "scroll" => {
                let vertical = optional_integer(arguments, "vertical", 0)?.clamp(-100, 100) as i32;
                let horizontal = optional_integer(arguments, "horizontal", 0)?.clamp(-100, 100) as i32;
                if vertical == 0 && horizontal == 0 {
                    return Err("Give \"vertical\" or \"horizontal\" clicks.".into());
                }
                let desktop = self.desktop()?;
                let point = optional_point(desktop, arguments)?;
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
                let text_argument = string(arguments, "text")?;
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
                let chord = string(arguments, "keys")?;
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
            "hold_keys" => {
                let keys = keys::parse_chord(string(arguments, "keys")?)?;
                let seconds = number(arguments, "seconds")?.clamp(0.0, MAX_HOLD_SECONDS);
                self.desktop()?.inject(&|batch| {
                    for &key in &keys {
                        batch.key(key, true);
                    }
                    batch.pause((seconds * 1000.0) as u64);
                    for &key in keys.iter().rev() {
                        batch.key(key, false);
                    }
                })?;
                Ok(text("Held."))
            }
            "set_clipboard" => {
                let content = string(arguments, "text")?;
                if content.len() > MAX_CLIPBOARD_BYTES {
                    return Err("The text is over 16 MiB.".into());
                }
                let file = memory_file(content.as_bytes())?;
                self.desktop()?.inject(&|batch| batch.set_clipboard(&file))?;
                Ok(text("On the clipboard."))
            }
            "keyboard_shortcuts" => {
                let convention = session::shortcut_convention()
                    .ok_or("The compositor did not say which shortcut key is in use.")?;
                let accelerators = session::accelerators().unwrap_or_default();
                Ok(text(shortcuts::describe(&convention, &accelerators)))
            }
            "list_windows" => {
                let desktop = self.desktop()?;
                let windows = desktop.windows()?;
                if windows.is_empty() {
                    return Ok(text("No application windows are open."));
                }
                let layout = desktop.layout()?;
                let apps = apps::named();
                let lines: Vec<String> =
                    windows.iter().map(|w| describe_window(w, &apps, &layout)).collect();
                Ok(text(lines.join("\n")))
            }
            "activate_window" => {
                let handle = window_handle(arguments)?;
                let desktop = self.desktop()?;
                desktop.inject(&|batch| batch.activate_window(handle))?;
                desktop.settle(SETTLE_TIMEOUT)?;
                Ok(text("Activated."))
            }
            "close_window" => {
                let handle = window_handle(arguments)?;
                let desktop = self.desktop()?;
                desktop.inject(&|batch| batch.close_window(handle))?;
                desktop.settle(SETTLE_TIMEOUT)?;
                Ok(text("Asked the window to close."))
            }
            "set_window_frame" => {
                let handle = window_handle(arguments)?;
                let desktop = self.desktop()?;
                let layout = desktop.layout()?;
                let frame = layout.to_logical_frame(
                    number(arguments, "x")?,
                    number(arguments, "y")?,
                    number(arguments, "width")?,
                    number(arguments, "height")?,
                );
                desktop.inject(&|batch| batch.set_window_frame(handle, frame))?;
                desktop.settle(SETTLE_TIMEOUT)?;
                let windows = desktop.windows()?;
                let window = windows
                    .iter()
                    .find(|window| window.handle == handle)
                    .ok_or("The window closed.")?;
                Ok(text(describe_window(window, &apps::named(), &layout)))
            }
            "list_applications" => {
                let lines: Vec<String> = apps::installed()
                    .iter()
                    .map(|app| format!("{} ({})", app.display_name(), app.id))
                    .collect();
                Ok(text(lines.join("\n")))
            }
            "open_application" => {
                let app = apps::find(&apps::installed(), string(arguments, "name")?)?;
                let before = self.handles()?;
                session::launch_application(&app.id)?;
                let window = self.new_window(&before)?;
                Ok(text(format!("Started {}. {window}", app.display_name())))
            }
            "open" => {
                let target = open_target(string(arguments, "target")?)?;
                let before = self.handles()?;
                session::open(&target)?;
                let window = self.new_window(&before)?;
                Ok(text(format!("Opened {target}. {window}")))
            }
            "actions" => self.actions(arguments),
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
                    if let Ok(desktop) = &mut self.desktop {
                        desktop.agent_name = name.to_string();
                    }
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
    let mut server = Server { desktop };

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
    fn open_targets_are_urls_or_existing_absolute_paths() {
        assert_eq!(open_target(" https://example.com/a:b ").unwrap(), "https://example.com/a:b");
        assert_eq!(open_target("mailto:someone@example.com").unwrap(), "mailto:someone@example.com");
        assert_eq!(open_target("/").unwrap(), "/");
        assert!(open_target("-x:y").is_err());
        assert!(open_target("--help").is_err());
        assert!(open_target("relative/path").is_err());
        assert!(open_target("/nonexistent/file").is_err());
    }

    #[test]
    fn actions_check_every_tool_first_and_stop_at_a_failure() {
        let mut server = Server { desktop: Err("No desktop.".into()) };
        let run = |server: &mut Server, steps: Value| server.call("actions", &json!({ "steps": steps }));
        assert!(run(&mut server, json!([])).is_err());
        assert!(run(&mut server, json!([{ "tool": "actions" }])).is_err());
        assert!(run(&mut server, json!([{ "tool": "list_windows" }, { "tool": "clik" }]))
            .unwrap_err()
            .starts_with("Step 2:"));

        let steps = json!([
            { "tool": "click", "arguments": { "x": 1, "y": 1 } },
            { "tool": "type_text", "arguments": { "text": "a" } },
            { "tool": "screenshot" }
        ]);
        let result = run(&mut server, steps).unwrap();
        assert_eq!(result["isError"], true);
        assert_eq!(
            result["content"][0]["text"],
            "1. click failed: No desktop.\nSteps 2 to 3 did not run."
        );
    }

    #[test]
    fn text_is_segmented_at_keys() {
        assert_eq!(segments("abc"), ["abc"]);
        assert_eq!(segments("你好\r\n\tä\u{8}"), ["你好", "\r\n\t", "ä", "\u{8}"]);
        assert_eq!(segments("\nx"), ["\n", "x"]);
        assert!(segments("").is_empty());
    }
}
