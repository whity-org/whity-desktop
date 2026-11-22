//! The hardening the PHP host runs under (ADR 0001, decision 3a). FrankenPHP's
//! `php-server` takes no `-d` flags, and the shipped `php.ini` next to the
//! binary cannot know where the app's data directory is, so the per-device
//! settings go in a second ini file that PHP picks up through the
//! `PHP_INI_SCAN_DIR` environment variable.
//!
//! What is locked here and why:
//! - `open_basedir`: the host's own code, the data directory and the system
//!   temp directory. A plugin cannot read the person's documents or another
//!   app's files.
//! - `disable_functions`: process control and shell access. Hardware and the
//!   outside world are reached through the native bridge, which checks the
//!   plugin's declared capabilities, never through `exec`.
//! - `allow_url_fopen` and `allow_url_include` off: no network from PHP
//!   streams. The bridge client switches to the curl extension when it lands.
//! - `expose_php` off, `display_errors` off: errors go to the log Rust reads.

use std::path::{Path, PathBuf};

/// The functions a plugin on the device may not call. Each spawns a process,
/// changes the process, or reaches outside the sandbox.
pub const DISABLED_FUNCTIONS: &[&str] = &[
    "exec",
    "passthru",
    "shell_exec",
    "system",
    "proc_open",
    "proc_nice",
    "popen",
    "pcntl_exec",
    "pcntl_fork",
    "putenv",
    "dl",
    "mail",
    "socket_create",
    "socket_connect",
    "fsockopen",
    "pfsockopen",
    "stream_socket_client",
    "stream_socket_server",
    "chown",
    "chgrp",
    "symlink",
    "link",
];

/// Render the sandbox ini for a host whose code is at `php_host_dir` and whose
/// writable data lives under `data_dir`. Paths are joined with the platform's
/// `open_basedir` separator and given to PHP in its own slash style.
pub fn render_ini(php_host_dir: &Path, data_dir: &Path, temp_dir: &Path) -> String {
    let separator = if cfg!(windows) { ";" } else { ":" };
    let roots = [php_host_dir, data_dir, temp_dir]
        .iter()
        .map(|p| ini_path(p))
        .collect::<Vec<_>>()
        .join(separator);
    format!(
        "; Written by Whity Desktop at every start. Edits are overwritten.\n\
         open_basedir = \"{roots}\"\n\
         disable_functions = {functions}\n\
         allow_url_fopen = Off\n\
         allow_url_include = Off\n\
         expose_php = Off\n\
         display_errors = Off\n\
         log_errors = On\n\
         error_log = \"{log}\"\n\
         sys_temp_dir = \"{temp}\"\n\
         upload_tmp_dir = \"{temp}\"\n\
         session.save_path = \"{temp}\"\n",
        functions = DISABLED_FUNCTIONS.join(","),
        log = ini_path(&data_dir.join("php-host.log")),
        temp = ini_path(temp_dir),
    )
}

/// Write the sandbox ini into `<data_dir>/php-ini/` and return that directory,
/// the value for `PHP_INI_SCAN_DIR`. The directory holds nothing else, so a
/// plugin cannot slip an ini of its own in beside it (it is outside
/// `open_basedir`'s writable roots only by convention; the data directory is
/// inside them, which is why the directory is recreated and the file
/// rewritten at every start).
pub fn install(php_host_dir: &Path, data_dir: &Path) -> Result<PathBuf, String> {
    let ini_dir = data_dir.join("php-ini");
    let temp_dir = data_dir.join("php-temp");
    std::fs::create_dir_all(&ini_dir)
        .map_err(|e| format!("failed to create {}: {e}", ini_dir.display()))?;
    std::fs::create_dir_all(&temp_dir)
        .map_err(|e| format!("failed to create {}: {e}", temp_dir.display()))?;
    for entry in std::fs::read_dir(&ini_dir)
        .map_err(|e| e.to_string())?
        .flatten()
    {
        if entry.file_name() != "whity-sandbox.ini" {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let ini = render_ini(php_host_dir, data_dir, &temp_dir);
    std::fs::write(ini_dir.join("whity-sandbox.ini"), ini)
        .map_err(|e| format!("failed to write the sandbox ini: {e}"))?;
    Ok(ini_dir)
}

/// PHP reads forward slashes on every platform, and a Windows path with
/// backslashes inside a quoted ini value needs no escaping either way; the
/// forward slashes keep the file readable in a log.
fn ini_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ini_locks_paths_functions_and_streams() {
        let ini = render_ini(
            Path::new("/app/php-host"),
            Path::new("/data/whity"),
            Path::new("/data/whity/php-temp"),
        );
        let separator = if cfg!(windows) { ";" } else { ":" };
        assert!(ini.contains(&format!(
            "open_basedir = \"/app/php-host{separator}/data/whity{separator}/data/whity/php-temp\""
        )));
        assert!(ini.contains("disable_functions = exec,passthru,shell_exec,system,proc_open"));
        assert!(ini.contains("allow_url_fopen = Off"));
        assert!(ini.contains("allow_url_include = Off"));
        assert!(ini.contains("error_log = \"/data/whity/php-host.log\""));
    }

    #[test]
    fn windows_paths_become_forward_slashes() {
        assert_eq!(
            ini_path(Path::new(r"C:\Users\x\AppData\whity")),
            "C:/Users/x/AppData/whity"
        );
    }

    #[test]
    fn install_writes_one_file_and_removes_strays() {
        let dir = std::env::temp_dir().join(format!("whity-php-host-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("php-ini")).unwrap();
        std::fs::write(dir.join("php-ini/stray.ini"), "allow_url_fopen = On").unwrap();
        let ini_dir = install(Path::new("/app/php-host"), &dir).unwrap();
        let names: Vec<String> = std::fs::read_dir(&ini_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["whity-sandbox.ini".to_string()]);
        assert!(dir.join("php-temp").is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
