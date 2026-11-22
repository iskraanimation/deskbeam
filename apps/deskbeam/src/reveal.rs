//! Showing a received file in the system's file manager, or opening it in its app.

use std::{path::Path, process::Command};

/// Opens the file manager at `path`, with it selected where the OS can do that.
pub fn reveal(path: &Path) {
    let opened = if cfg!(target_os = "macos") {
        Command::new("open").arg("-R").arg(path).spawn()
    } else if cfg!(target_os = "windows") {
        let mut select = std::ffi::OsString::from("/select,");
        select.push(path);
        Command::new("explorer").arg(select).spawn()
    } else {
        let folder = if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(path)
        };
        Command::new("xdg-open").arg(folder).spawn()
    };
    if let Err(e) = opened {
        tracing::warn!("could not show {}: {e}", path.display());
    }
}

/// Opens `path` in the app the system uses for it (a video in its player).
pub fn open(path: &Path) {
    let opened = if cfg!(target_os = "macos") {
        Command::new("open").arg(path).spawn()
    } else if cfg!(target_os = "windows") {
        // Explorer opens a file in its default app, with no console window and no cmd
        // parsing of characters like `&` in the name.
        Command::new("explorer").arg(path).spawn()
    } else {
        Command::new("xdg-open").arg(path).spawn()
    };
    if let Err(e) = opened {
        tracing::warn!("could not open {}: {e}", path.display());
    }
}
