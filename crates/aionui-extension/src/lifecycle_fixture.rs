//! Native hook scripts shared by unit and integration tests.

use std::path::Path;

pub(super) fn write_script(dir: &Path, relative: &str, unix: &str, windows: &str) -> String {
    let mut relative = std::path::PathBuf::from(relative);
    if cfg!(windows) {
        relative.set_extension("cmd");
    }
    let full = dir.join(&relative);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    let content = if cfg!(windows) {
        format!("@echo off\r\n{}\r\n", windows.replace('\n', "\r\n"))
    } else {
        format!("#!/bin/sh\n{unix}\n")
    };
    std::fs::write(&full, content).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&full, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    relative.to_string_lossy().into_owned()
}
