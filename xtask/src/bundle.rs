//! `cargo xtask bundle`: the macOS app (plan §4, Packaging).
//!
//! Builds `deskbeam` in release for Apple Silicon and Intel, joins the two into one universal
//! program with `lipo`, and puts it into `target/bundle/DeskBeam.app` with its `Info.plist`
//! and icon. Then makes `target/bundle/DeskBeam-<version>-macos.dmg`: the app and a link to
//! Applications, to drag it onto.
//!
//! The app is signed ad hoc (`codesign --sign -`), which Apple Silicon needs to run it at all.
//! A copy downloaded from elsewhere is still quarantined by macOS until it is signed with a
//! Developer ID and notarized; see the README.
//!
//! `--native` builds for this Mac's processor only, which is quicker for trying it out.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

/// The app's bundle id. `apps/deskbeam/src/notify.rs` sends notifications as this app, so the
/// two must match.
pub const BUNDLE_ID: &str = "org.deskbeam.DeskBeam";

/// The oldest macOS the app says it runs on. Rust's Intel target builds for 10.12; 10.13 is
/// the oldest wgpu's Metal backend supports.
const MIN_MACOS: &str = "10.13";

const VERSION: &str = env!("CARGO_PKG_VERSION");

const TARGETS: [&str; 2] = ["aarch64-apple-darwin", "x86_64-apple-darwin"];

pub fn run(args: Vec<String>) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("bundle: makes the macOS app, so it runs on macOS only".into());
    }
    let native = match args.as_slice() {
        [] => false,
        [flag] if flag == "--native" => true,
        _ => return Err("usage: cargo xtask bundle [--native]".into()),
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("no workspace root")?
        .to_path_buf();
    let target = root.join("target");

    let program = if native {
        crate::cargo(&["build", "--release", "--locked", "-p", "deskbeam"])?;
        target.join("release/deskbeam")
    } else {
        let mut built = Vec::new();
        for triple in TARGETS {
            crate::cargo(&[
                "build",
                "--release",
                "--locked",
                "-p",
                "deskbeam",
                "--target",
                triple,
            ])?;
            built.push(target.join(triple).join("release/deskbeam"));
        }
        let universal = target.join("universal/deskbeam");
        std::fs::create_dir_all(universal.parent().unwrap()).map_err(io)?;
        let mut lipo = Command::new("lipo");
        lipo.arg("-create")
            .arg("-output")
            .arg(&universal)
            .args(&built);
        run_tool(&mut lipo)?;
        universal
    };

    let out = target.join("bundle");
    let app = out.join("DeskBeam.app");
    if app.exists() {
        std::fs::remove_dir_all(&app).map_err(io)?;
    }
    let contents = app.join("Contents");
    std::fs::create_dir_all(contents.join("MacOS")).map_err(io)?;
    std::fs::create_dir_all(contents.join("Resources")).map_err(io)?;
    std::fs::copy(&program, contents.join("MacOS/deskbeam")).map_err(io)?;
    std::fs::copy(
        root.join("apps/deskbeam/assets/deskbeam.icns"),
        contents.join("Resources/deskbeam.icns"),
    )
    .map_err(io)?;
    std::fs::write(contents.join("Info.plist"), info_plist(VERSION)).map_err(io)?;
    std::fs::write(contents.join("PkgInfo"), "APPL????").map_err(io)?;
    run_tool(
        Command::new("plutil")
            .arg("-lint")
            .arg(contents.join("Info.plist")),
    )?;
    run_tool(
        Command::new("codesign")
            .args(["--force", "--sign", "-", "--identifier", BUNDLE_ID])
            .arg(&app),
    )?;
    println!("bundle: {}", app.display());

    let dmg = out.join(format!("DeskBeam-{VERSION}-macos.dmg"));
    make_dmg(&app, &dmg)?;
    println!("bundle: {}", dmg.display());
    Ok(())
}

/// A compressed disk image with the app and a link to `/Applications`.
fn make_dmg(app: &Path, dmg: &Path) -> Result<(), String> {
    let stage: PathBuf = app.with_file_name("dmg");
    if stage.exists() {
        std::fs::remove_dir_all(&stage).map_err(io)?;
    }
    std::fs::create_dir_all(&stage).map_err(io)?;
    run_tool(
        Command::new("ditto")
            .arg(app)
            .arg(stage.join("DeskBeam.app")),
    )?;
    std::os::unix::fs::symlink("/Applications", stage.join("Applications")).map_err(io)?;
    if dmg.exists() {
        std::fs::remove_file(dmg).map_err(io)?;
    }
    run_tool(
        Command::new("hdiutil")
            .args([
                "create", "-volname", "DeskBeam", "-fs", "HFS+", "-format", "UDZO",
            ])
            .arg("-srcfolder")
            .arg(&stage)
            .arg(dmg),
    )?;
    std::fs::remove_dir_all(&stage).map_err(io)
}

/// The app's `Info.plist`.
///
/// - `LSUIElement` is left out, so DeskBeam has a Dock icon like any app; closing the window
///   still leaves it in the menu bar.
/// - `NSLocalNetworkUsageDescription` is what macOS 15 shows when it asks whether DeskBeam
///   may use the local network: finding nearby devices and connecting directly to them.
/// - `NSMicrophoneUsageDescription` is what it shows the first time a call opens the
///   microphone; without it macOS stops the app there. `NSCameraUsageDescription` is the
///   same for the camera, and `NSCameraUseContinuityCameraDeviceType` lets an iPhone near
///   the Mac show as a camera.
fn info_plist(version: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleDisplayName</key>
	<string>DeskBeam</string>
	<key>CFBundleExecutable</key>
	<string>deskbeam</string>
	<key>CFBundleIconFile</key>
	<string>deskbeam</string>
	<key>CFBundleIdentifier</key>
	<string>{BUNDLE_ID}</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>DeskBeam</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>{version}</string>
	<key>CFBundleVersion</key>
	<string>{version}</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.productivity</string>
	<key>LSMinimumSystemVersion</key>
	<string>{MIN_MACOS}</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSHumanReadableCopyright</key>
	<string>DeskBeam contributors. AGPL-3.0-only.</string>
	<key>NSMicrophoneUsageDescription</key>
	<string>DeskBeam uses the microphone during calls with your paired devices.</string>
	<key>NSCameraUsageDescription</key>
	<string>DeskBeam uses the camera when you turn it on in a call with a paired device.</string>
	<key>NSCameraUseContinuityCameraDeviceType</key>
	<true/>
	<key>NSLocalNetworkUsageDescription</key>
	<string>DeskBeam finds your paired devices on this network and connects to them directly.</string>
	<key>NSSupportsAutomaticGraphicsSwitching</key>
	<true/>
</dict>
</plist>
"#
    )
}

fn run_tool(command: &mut Command) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|e| format!("{:?}: {e}", command.get_program()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{:?} failed: {status}", command.get_program()))
    }
}

fn io(e: std::io::Error) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_plist_names_the_app_and_version() {
        let plist = info_plist("1.2.3");
        assert!(plist.contains("<string>org.deskbeam.DeskBeam</string>"));
        assert!(plist.contains("<string>1.2.3</string>"));
        assert!(plist.contains("<string>deskbeam</string>"));
        assert!(!plist.contains("LSUIElement"));
    }
}
