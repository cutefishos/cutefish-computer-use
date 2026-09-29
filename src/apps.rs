// Installed applications, from their desktop entries, filtered as the
// Cutefish application registry filters them.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::session;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct App {
    pub id: String,
    pub name: String,
    // The name in the user's language, when the entry has one.
    pub local_name: Option<String>,
    pub wm_class: Option<String>,
    // What window matching also looks at: the first word of Exec, Icon and the
    // desktop file's own name.
    program: String,
    icon: String,
    file_name: String,
    hidden: bool,
    no_display: bool,
    shown: bool,
}

impl App {
    pub fn display_name(&self) -> &str {
        self.local_name.as_deref().unwrap_or(&self.name)
    }

    fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.name.as_str()).chain(self.local_name.as_deref())
    }
}

// The locale's names for Name[...] lookups, most specific first: zh_CN.UTF-8
// gives zh_CN and zh.
fn locale_names() -> Vec<String> {
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|name| session::variable(name))
        .unwrap_or_default();
    let base = locale.split(['.', '@']).next().unwrap_or_default();
    if base.is_empty() || base == "C" || base == "POSIX" {
        return Vec::new();
    }
    let mut names = vec![base.to_string()];
    if let Some((language, _)) = base.split_once('_') {
        names.push(language.to_string());
    }
    names
}

fn executable_exists(program: &str) -> bool {
    let runs = |path: &Path| {
        fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        return runs(Path::new(program));
    }
    let path = session::variable("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
    path.split(':').any(|dir| runs(&Path::new(dir).join(program)))
}

fn listed_in(list: &str, desktops: &[String]) -> bool {
    list.split(';').any(|wanted| desktops.iter().any(|d| d.eq_ignore_ascii_case(wanted)))
}

// The entry, or nothing when it is not an application. `shown` is false for
// one the launcher leaves out; Hidden entries come back too, as they mask a
// system entry of the same id.
fn parse(
    id: &str,
    file_name: &str,
    text: &str,
    locales: &[String],
    desktops: &[String],
) -> Option<App> {
    let mut in_entry = false;
    let mut values = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
        } else if let Some((key, value)) = line.split_once('=').filter(|_| in_entry) {
            values.push((key.trim(), value.trim()));
        }
    }
    let get = |key: &str| values.iter().find(|(k, _)| *k == key).map(|(_, value)| *value);
    let yes = |key: &str| get(key).is_some_and(|value| value.eq_ignore_ascii_case("true"));
    if get("Type") != Some("Application") {
        return None;
    }
    let name = get("Name").unwrap_or_default().to_string();
    let local_name = locales
        .iter()
        .find_map(|locale| get(&format!("Name[{locale}]")))
        .filter(|local| *local != name)
        .map(str::to_string);
    let program = get("Exec")
        .and_then(|exec| exec.split_whitespace().next())
        .unwrap_or_default()
        .trim_matches('"')
        .to_string();
    let shown = !yes("Hidden")
        && !yes("NoDisplay")
        && !name.is_empty()
        && (get("Exec").is_some_and(|exec| !exec.is_empty()) || yes("DBusActivatable"))
        && get("TryExec").is_none_or(|program| program.is_empty() || executable_exists(program))
        && get("OnlyShowIn").is_none_or(|list| list.is_empty() || listed_in(list, desktops))
        && !get("NotShowIn").is_some_and(|list| listed_in(list, desktops));
    Some(App {
        id: id.to_string(),
        name,
        local_name,
        wm_class: get("StartupWMClass").map(str::to_string),
        program,
        icon: get("Icon").unwrap_or_default().to_string(),
        file_name: file_name.to_string(),
        hidden: yes("Hidden"),
        no_display: yes("NoDisplay"),
        shown,
    })
}

fn application_dirs() -> Vec<PathBuf> {
    let home = session::variable("HOME").unwrap_or_default();
    let data_home =
        session::variable("XDG_DATA_HOME").unwrap_or_else(|| format!("{home}/.local/share"));
    let data_dirs =
        session::variable("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    std::iter::once(data_home.as_str())
        .chain(data_dirs.split(':'))
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join("applications"))
        .collect()
}

// Desktop files under `dir`, with their ids: the path below it, with dashes
// for slashes.
fn desktop_files(dir: &Path, prefix: &str, files: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            desktop_files(&path, &format!("{prefix}{name}-"), files);
        } else if let Some(stem) = name.strip_suffix(".desktop") {
            files.push((format!("{prefix}{stem}"), path));
        }
    }
}

// Every application entry, the first of each id winning, shown or not.
fn entries() -> Vec<App> {
    let locales = locale_names();
    let desktops: Vec<String> = session::variable("XDG_CURRENT_DESKTOP")
        .unwrap_or_else(|| "Cutefish".into())
        .split(':')
        .filter(|d| !d.is_empty())
        .map(str::to_string)
        .collect();
    let mut seen = HashSet::new();
    let mut apps = Vec::new();
    for dir in application_dirs() {
        let mut files = Vec::new();
        desktop_files(&dir, "", &mut files);
        for (id, path) in files {
            if !seen.insert(id.clone()) {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else { continue };
            let file_name = path.file_stem().unwrap_or_default().to_string_lossy();
            if let Some(app) = parse(&id, &file_name, &text, &locales, &desktops) {
                apps.push(app);
            }
        }
    }
    apps
}

// What the launcher shows, by name.
pub fn installed() -> Vec<App> {
    let mut apps: Vec<App> = entries().into_iter().filter(|app| app.shown).collect();
    apps.sort_by_key(|app| app.display_name().to_lowercase());
    apps
}

// Every entry that can own windows, including helpers the launcher leaves out.
pub fn named() -> Vec<App> {
    entries().into_iter().filter(|app| !app.hidden && !app.name.is_empty()).collect()
}

// Whether `value` is `prefix` or starts with it followed by a word break.
fn starts_with_word(value: &str, prefix: &str) -> bool {
    !prefix.is_empty()
        && value.strip_prefix(prefix).is_some_and(|rest| {
            rest.is_empty() || rest.starts_with(['-', '.', '_', ' '])
        })
}

// The entry of a window's app_id, scored as the dock scores a Wayland window
// (cutefish-framework `ApplicationRegistry::resolveWindow` with only the
// app_id, which stands in for the window class too).
pub fn for_window(apps: &[App], app_id: &str) -> Option<App> {
    let wanted = app_id.trim().to_lowercase();
    if wanted.is_empty() {
        return None;
    }
    let mut best: Option<(&App, u32, u32)> = None;
    for app in apps {
        let id = app.id.to_lowercase();
        let class = app.wm_class.as_deref().unwrap_or_default().trim().to_lowercase();
        let rules = [
            (id == wanted, 100),
            (class == wanted, 95),
            (starts_with_word(&class, &wanted), 70),
            (starts_with_word(&id, &wanted), 60),
            (starts_with_word(&app.file_name.to_lowercase(), &wanted), 60),
            (starts_with_word(&app.program.to_lowercase(), &wanted), 55),
            (starts_with_word(&app.icon.to_lowercase(), &wanted), 55),
            (starts_with_word(&app.name.to_lowercase(), &wanted), 50),
        ];
        let score = rules.iter().filter(|(hit, _)| *hit).map(|(_, score)| *score).max().unwrap_or(0);
        // On equal scores, a launcher entry beats one for another desktop,
        // and either beats a NoDisplay helper.
        let preference = if app.shown { 2 } else if app.no_display { 0 } else { 1 };
        match best {
            _ if score == 0 => {}
            Some((_, best_score, _)) if score < best_score => {}
            Some((_, best_score, best_preference))
                if score == best_score && preference <= best_preference => {}
            _ => best = Some((app, score, preference)),
        }
    }
    best.map(|(app, _, _)| app.clone())
}

// The application `query` names: its id, its name, or else the one name or id
// that contains it.
pub fn find(apps: &[App], query: &str) -> Result<App, String> {
    let query = query.trim();
    let query = query.strip_suffix(".desktop").unwrap_or(query).to_lowercase();
    if query.is_empty() {
        return Err("Name an application.".into());
    }
    if let Some(app) = apps.iter().find(|app| app.id.to_lowercase() == query) {
        return Ok(app.clone());
    }
    if let Some(app) = apps.iter().find(|app| app.names().any(|name| name.to_lowercase() == query)) {
        return Ok(app.clone());
    }
    let matches: Vec<&App> = apps
        .iter()
        .filter(|app| {
            app.id.to_lowercase().contains(&query)
                || app.names().any(|name| name.to_lowercase().contains(&query))
        })
        .collect();
    match matches.as_slice() {
        [app] => Ok((*app).clone()),
        [] => Err(format!("No application is called \"{query}\". Use list_applications.")),
        several => {
            let names: Vec<String> = several
                .iter()
                .take(10)
                .map(|app| format!("{} ({})", app.display_name(), app.id))
                .collect();
            Err(format!("Several applications match \"{query}\": {}. Give the id.", names.join(", ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desktops() -> Vec<String> {
        vec!["Cutefish".to_string()]
    }

    #[test]
    fn entries_are_filtered_as_the_launcher_filters_them() {
        let locales = vec!["zh_CN".to_string(), "zh".to_string()];
        let text = "[Desktop Entry]\nType=Application\nName=Text Editor\nName[zh]=文本编辑器\nExec=gedit %U\n\
                    StartupWMClass=Gedit\n[Desktop Action new]\nName=New\n";
        let app = parse("org.gnome.gedit", "org.gnome.gedit", text, &locales, &desktops()).unwrap();
        assert!(app.shown);
        assert_eq!(app.display_name(), "文本编辑器");
        assert_eq!(app.wm_class.as_deref(), Some("Gedit"));

        let hidden = |extra: &str| {
            let text = format!("[Desktop Entry]\nType=Application\nName=X\nExec=x\n{extra}\n");
            !parse("x", "x", &text, &locales, &desktops()).unwrap().shown
        };
        assert!(hidden("NoDisplay=true"));
        assert!(hidden("Hidden=true"));
        assert!(hidden("OnlyShowIn=KDE;"));
        assert!(hidden("NotShowIn=GNOME;Cutefish;"));
        assert!(hidden("TryExec=/nonexistent/program"));
        assert!(!hidden("OnlyShowIn=GNOME;cutefish;"));
        assert!(parse("x", "x", "[Desktop Entry]\nType=Link\nName=X\n", &locales, &desktops()).is_none());
    }

    #[test]
    fn applications_are_found_by_id_name_or_a_unique_part() {
        let app = |id: &str, name: &str, local: Option<&str>| App {
            id: id.into(),
            name: name.into(),
            local_name: local.map(str::to_string),
            shown: true,
            ..App::default()
        };
        let apps = vec![
            app("org.gnome.gedit", "Text Editor", Some("文本编辑器")),
            app("firefox-esr", "Firefox ESR", None),
            app("cutefish-terminal", "Terminal", Some("终端")),
            app("xterm", "XTerm", None),
        ];
        assert_eq!(find(&apps, "org.gnome.gedit.desktop").unwrap().id, "org.gnome.gedit");
        assert_eq!(find(&apps, "text editor").unwrap().id, "org.gnome.gedit");
        assert_eq!(find(&apps, "终端").unwrap().id, "cutefish-terminal");
        assert_eq!(find(&apps, "firefox").unwrap().id, "firefox-esr");
        assert_eq!(find(&apps, "xterm").unwrap().id, "xterm");
        assert!(find(&apps, "term").unwrap_err().contains("Several"));
        assert!(find(&apps, "writer").unwrap_err().contains("No application"));
    }

    #[test]
    fn windows_are_matched_as_the_dock_matches_them() {
        let locales = Vec::new();
        let entry = |id: &str, text: &str| {
            let text = format!("[Desktop Entry]\nType=Application\n{text}\n");
            parse(id, id, &text, &locales, &desktops()).unwrap()
        };
        let apps = vec![
            entry("org.gnome.gedit", "Name=gedit\nExec=gedit %U\nIcon=org.gnome.gedit"),
            entry("firefox-esr", "Name=Firefox ESR\nExec=/usr/lib/firefox-esr/firefox-esr %u"),
            entry("code-helper", "Name=Code Helper\nExec=code --helper\nNoDisplay=true"),
            entry("code", "Name=Visual Studio Code\nExec=/usr/share/code/code\nStartupWMClass=Code"),
            entry("codeblocks", "Name=Code::Blocks IDE\nExec=codeblocks"),
        ];
        let id = |app_id: &str| for_window(&apps, app_id).map(|app| app.id);
        assert_eq!(id("gedit").as_deref(), Some("org.gnome.gedit"));
        assert_eq!(id("Firefox-ESR").as_deref(), Some("firefox-esr"));
        assert_eq!(id("code").as_deref(), Some("code"));
        assert_eq!(id("unknown"), None);
    }
}
