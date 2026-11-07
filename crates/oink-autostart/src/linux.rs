//! XDG desktop autostart; queries never register a login item.
use super::{
    AUTOSTART_ARG, LaunchAtLoginProblem as Problem, LaunchAtLoginState as State, LaunchAtLoginStatus as Status,
};
use std::os::unix::fs::OpenOptionsExt;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
const ENTRY: &str = "io.github.oinkvm.Oink.desktop";
fn config_home() -> Result<PathBuf, String> {
    if let Some(value) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        let value = PathBuf::from(value);
        if value.is_absolute() {
            return Ok(value);
        }
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|p| p.join(".config"))
        .ok_or_else(|| "system_unavailable".into())
}
fn entry_path() -> Result<PathBuf, String> {
    Ok(config_home()?.join("autostart").join(ENTRY))
}
fn value_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}
fn exec_for(exe: &Path) -> Result<String, String> {
    let value = exe.to_str().ok_or("installed_app_required")?;
    if !exe.is_absolute() || value.chars().any(|c| c == '\0' || c == '\n' || c == '\r' || c == '=') {
        return Err("installed_app_required".into());
    }
    let mut quoted = String::from("\"");
    for ch in value.chars() {
        match ch {
            '%' => quoted.push_str("%%"),
            '\\' | '"' | '$' | '\x60' => {
                quoted.push('\\');
                quoted.push(ch);
            }
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    Ok(format!("{} {AUTOSTART_ARG}", value_escape(&quoted)))
}
fn desktop_entry(exe: &Path, enabled: bool) -> Result<String, String> {
    let command = exec_for(exe)?;
    Ok(format!(
        "[Desktop Entry]\nType=Application\nName=Oink\nComment=Share keyboard, mouse and audio\nExec={command}\nTerminal=false\nHidden={}\nX-GNOME-Autostart-enabled={}\nX-Oink-Executable={}\n",
        !enabled,
        enabled,
        value_escape(exe.to_str().ok_or("installed_app_required")?)
    ))
}
fn field<'a>(entry: &'a str, key: &str) -> Option<&'a str> {
    let mut inside = false;
    let mut found = None;
    for line in entry.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            inside = line == "[Desktop Entry]";
            continue;
        }
        if inside && !line.starts_with('#') {
            if let Some((name, value)) = line.split_once('=') {
                if name.trim() == key {
                    found = Some(value.trim());
                }
            }
        }
    }
    found
}
fn evaluate(entry: &str, exe: &Path) -> Status {
    if field(entry, "Hidden") == Some("true") || field(entry, "X-GNOME-Autostart-enabled") == Some("false") {
        return Status::new(State::Disabled, None);
    }
    let identity = value_escape(&exe.to_string_lossy());
    if field(entry, "X-Oink-Executable") != Some(identity.as_str()) {
        return Status::new(State::Disabled, Some(Problem::DifferentInstallation));
    }
    if field(entry, "Type") != Some("Application")
        || field(entry, "Exec") != exec_for(exe).ok().as_deref()
        || field(entry, "OnlyShowIn").is_some()
        || field(entry, "NotShowIn").is_some()
        || field(entry, "DBusActivatable") == Some("true")
        || field(entry, "TryExec").is_some_and(|value| value != exe.to_string_lossy())
    {
        return Status::new(State::Disabled, Some(Problem::InvalidRunEntry));
    }
    Status::new(State::Enabled, None)
}
fn read_status(exe: &Path, user_path: &Path) -> Status {
    let mut candidates = vec![user_path.to_owned()];
    let system = std::env::var_os("XDG_CONFIG_DIRS")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/etc/xdg".into());
    candidates.extend(
        std::env::split_paths(&system)
            .filter(|p| p.is_absolute())
            .map(|p| p.join("autostart").join(ENTRY)),
    );
    for file in candidates {
        match fs::read_to_string(file) {
            Ok(entry) => return evaluate(&entry, exe),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(%e, "cannot read XDG autostart entry");
                return Status::unavailable(Problem::SystemUnavailable);
            }
        }
    }
    Status::new(State::Disabled, None)
}
fn ui_exe() -> Result<PathBuf, String> {
    let current = std::env::current_exe().map_err(|_| "system_unavailable")?;
    if current.file_name().is_some_and(|n| n == "oink") {
        return Ok(current);
    }
    let ui = current.parent().ok_or("main_app_required")?.join("oink");
    if ui.is_file() {
        Ok(ui)
    } else {
        Err("main_app_required".into())
    }
}
pub(super) fn status() -> Status {
    match (ui_exe(), entry_path()) {
        (Ok(exe), Ok(file)) => read_status(&exe, &file),
        _ => Status::unavailable(Problem::SystemUnavailable),
    }
}
fn write_entry(file: &Path, exe: &Path, enabled: bool) -> Result<(), String> {
    if fs::symlink_metadata(file).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("system_unavailable".into());
    }
    let parent = file.parent().ok_or("system_unavailable")?;
    fs::create_dir_all(parent).map_err(|e| {
        tracing::warn!(%e, "cannot create autostart directory");
        "system_unavailable".to_string()
    })?;
    let temporary = parent.join(format!(".{ENTRY}.{}.tmp", std::process::id()));
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|_| "system_unavailable".to_string())?;
    let result = (|| {
        output
            .write_all(desktop_entry(exe, enabled)?.as_bytes())
            .map_err(|_| "system_unavailable".to_string())?;
        output.sync_all().map_err(|_| "system_unavailable".to_string())?;
        fs::rename(&temporary, file).map_err(|_| "system_unavailable".to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
pub(super) fn set_enabled(enabled: bool) -> Result<Status, String> {
    let file = entry_path()?;
    let exe = ui_exe()?;
    write_entry(&file, &exe, enabled)?;
    let after = read_status(&exe, &file);
    if (after.state == State::Enabled) == enabled && after.problem.is_none() {
        Ok(after)
    } else {
        Err("system_unavailable".into())
    }
}
pub(super) fn open_system_settings() -> Result<(), String> {
    let directory = config_home()?.join("autostart");
    fs::create_dir_all(&directory).map_err(|_| "system_unavailable")?;
    std::process::Command::new("xdg-open")
        .arg(directory)
        .spawn()
        .map(|_| ())
        .map_err(|_| "system_unavailable".into())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escaped_executable_and_autostart_marker_round_trip() {
        let exe = Path::new("/opt/Oink 100%/oink\"$\\");
        let entry = desktop_entry(exe, true).unwrap();
        assert_eq!(evaluate(&entry, exe).state, State::Enabled);
        assert!(field(&entry, "Exec").unwrap().contains("100%%"));
        assert!(field(&entry, "Exec").unwrap().ends_with(" --autostart"));
        assert_eq!(
            evaluate(&entry, Path::new("/other/oink")).problem,
            Some(Problem::DifferentInstallation)
        );
    }
    #[test]
    fn disabled_and_modified_entries_never_claim_enabled() {
        let exe = Path::new("/opt/oink");
        assert_eq!(
            evaluate(&desktop_entry(exe, false).unwrap(), exe).state,
            State::Disabled
        );
        let wrong = desktop_entry(exe, true).unwrap().replace(" --autostart", " --service");
        assert_eq!(evaluate(&wrong, exe).problem, Some(Problem::InvalidRunEntry));
        assert!(desktop_entry(Path::new("/bad\npath"), true).is_err());
        assert!(desktop_entry(Path::new("/bad=path"), true).is_err());
    }
    #[test]
    fn isolated_registration_persists_and_disables() {
        let directory = std::env::temp_dir().join(format!(
            "oink-autostart-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let file = directory.join(ENTRY);
        let exe = Path::new("/opt/Oink/oink");
        write_entry(&file, exe, true).unwrap();
        assert_eq!(evaluate(&fs::read_to_string(&file).unwrap(), exe).state, State::Enabled);
        write_entry(&file, exe, false).unwrap();
        assert_eq!(
            evaluate(&fs::read_to_string(&file).unwrap(), exe).state,
            State::Disabled
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
