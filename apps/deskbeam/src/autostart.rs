//! Starting DeskBeam at login, hidden in the tray, so paired devices can reach this one
//! without anyone opening it.
//!
//! The OS keeps this setting (a LaunchAgent on macOS, the Run key on Windows, an autostart
//! entry on Linux), so it is read from there rather than kept in the store.

use auto_launch::{AutoLaunch, AutoLaunchBuilder};

/// The argument the login item passes, so the app starts in the tray.
pub const HIDDEN_ARG: &str = "--hidden";

fn launcher() -> anyhow::Result<AutoLaunch> {
    let exe = std::env::current_exe()?;
    let path = exe
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("the app's path is not valid UTF-8"))?;
    let mut builder = AutoLaunchBuilder::new();
    builder.set_app_name("DeskBeam").set_app_path(path);
    if crate::tray::AVAILABLE {
        builder.set_args(&[HIDDEN_ARG]);
    }
    builder.set_use_launch_agent(true);
    Ok(builder.build()?)
}

pub fn is_enabled() -> bool {
    launcher()
        .and_then(|l| Ok(l.is_enabled()?))
        .unwrap_or(false)
}

pub fn set_enabled(on: bool) -> anyhow::Result<()> {
    let launcher = launcher()?;
    if on {
        launcher.enable()?;
    } else {
        launcher.disable()?;
    }
    Ok(())
}
