// SPDX-FileCopyrightText: 2026 Reion Wong <reionwong@gmail.com>
// SPDX-License-Identifier: BSD-3-Clause

// The user's session: its environment, its application runtime and its
// keyboard shortcuts.

use std::env;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use serde_json::Value;

pub fn runtime_dir() -> PathBuf {
    env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| {
        // SAFETY: getuid takes no arguments and cannot fail.
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/run/user/{uid}"))
    })
}

// Some agent apps start MCP servers with a trimmed environment, without the
// Wayland variables; the session exports its own to systemd.
pub fn exported(name: &str) -> Option<String> {
    static EXPORTED: OnceLock<Vec<(String, String)>> = OnceLock::new();
    let exported = EXPORTED.get_or_init(|| {
        let Ok(output) = Command::new("systemctl")
            .args(["--user", "show-environment"])
            .env("XDG_RUNTIME_DIR", runtime_dir())
            .output()
        else {
            return Vec::new();
        };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    });
    exported.iter().find(|(n, _)| n == name).map(|(_, value)| value.clone())
}

// A variable of this process, or else of the session.
pub fn variable(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty()).or_else(|| exported(name))
}

// Calls org.cutefish.AppRuntime, which starts applications with the session's
// environment, and returns the pid it reports.
fn call_app_runtime(method: &str, signature: &str, arguments: &[&str]) -> Result<u32, String> {
    let failed = || "The session could not start it.".to_string();
    // "--" keeps an argument that starts with a dash from reading as an option.
    let output = Command::new("busctl")
        .args(["--user", "--", "call", "org.cutefish.AppRuntime", "/AppRuntime"])
        .args(["org.cutefish.AppRuntime", method, signature])
        .args(arguments)
        .env("XDG_RUNTIME_DIR", runtime_dir())
        .output()
        .map_err(|_| failed())?;
    let pid = String::from_utf8_lossy(&output.stdout)
        .trim()
        .strip_prefix("u ")
        .and_then(|pid| pid.parse::<u32>().ok())
        .unwrap_or(0);
    if !output.status.success() || pid == 0 {
        return Err(failed());
    }
    Ok(pid)
}

// Launches the desktop entry `id`.
pub fn launch_application(id: &str) -> Result<u32, String> {
    call_app_runtime("launchApplication", "sas", &[id, "0"])
}

// Opens a file or URL in its default application, as the shell does.
pub fn open(target: &str) -> Result<u32, String> {
    call_app_runtime("launchCommand", "sass", &["", "2", "xdg-open", target, ""])
}

// Runs busctl on the session bus and returns the "data" of its JSON answer.
fn busctl(arguments: &[&str]) -> Option<Value> {
    let output = Command::new("busctl")
        .args(["--user", "--json=short", "--"])
        .args(arguments)
        .env("XDG_RUNTIME_DIR", runtime_dir())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut answer: Value = serde_json::from_slice(&output.stdout).ok()?;
    Some(answer["data"].take())
}

const SHORTCUTS: [&str; 3] =
    ["org.cutefish.Shortcuts1", "/org/cutefish/Shortcuts1", "org.cutefish.Shortcuts1"];

// "command" or "pc".
pub fn shortcut_convention() -> Option<String> {
    let [service, path, interface] = SHORTCUTS;
    busctl(&["get-property", service, path, interface, "shortcutConvention"])?
        .as_str()
        .map(str::to_string)
}

// The accelerator table as JSON.
pub fn accelerators() -> Option<String> {
    let [service, path, interface] = SHORTCUTS;
    busctl(&["call", service, path, interface, "accelerators"])?
        .get(0)?
        .as_str()
        .map(str::to_string)
}
