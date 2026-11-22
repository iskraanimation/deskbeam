//! Workspace automation: `cargo xtask <layers|ci|icon|bundle>`.
//!
//! - `layers`: enforces the dependency layering (plan §3.1, principle 5). A crate depends
//!   only on lower layers, and nothing under `crates/` depends on a UI, windowing or tray crate.
//! - `ci`: what CI runs: format check, clippy with warnings as errors, tests, layers.
//! - `icon`: the Windows and macOS icons, made from the app's PNG (see [`icon`]).
//! - `bundle`: on macOS, `DeskBeam.app` and a `.dmg` with it (see [`bundle`]).

mod bundle;

use std::process::{Command, ExitCode};

use serde_json::Value;

/// (package name, layer). A crate may depend on workspace crates of a lower layer only.
const LAYERS: &[(&str, u8)] = &[
    ("deskbeam-model", 0),
    ("deskbeam-protocol", 1),
    ("deskbeam-store", 1),
    ("deskbeam-tunnel", 1),
    ("deskbeam-rdp", 1),
    ("deskbeam-chat", 1),
    ("deskbeam-call", 1),
    ("deskbeam-lan", 1),
    ("deskbeam-net", 2),
    ("deskbeam-engine", 3),
    ("deskbeam", 4),
    ("deskbeam-cli", 4),
];

/// The apps' layer: crates below it must not depend on [`UI_ONLY`].
const APP_LAYER: u8 = 4;

/// UI toolkits, windowing, the tray and native dialogs.
const UI_ONLY: &[&str] = &[
    "egui",
    "egui_extras",
    "eframe",
    "egui-wgpu",
    "winit",
    "wgpu",
    "tray-icon",
    "muda",
    "rfd",
];

fn layer_of(name: &str) -> Option<u8> {
    LAYERS.iter().find(|(n, _)| *n == name).map(|(_, l)| *l)
}

/// Each workspace package with its normal and build dependencies (dev-dependencies don't ship).
/// Names are the packages' own, not the names a crate renames them to.
fn workspace_crates() -> Result<Vec<(String, Vec<String>)>, String> {
    let out = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into());
    }
    let metadata: Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    let packages = metadata["packages"]
        .as_array()
        .ok_or("cargo metadata: no packages")?;
    Ok(packages
        .iter()
        .filter(|p| p["name"] != "xtask")
        .map(|p| {
            let deps = p["dependencies"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|d| d["kind"].is_null() || d["kind"] == "build")
                .filter_map(|d| d["name"].as_str().map(str::to_string))
                .collect();
            (p["name"].as_str().unwrap_or_default().to_string(), deps)
        })
        .collect())
}

fn layers() -> Result<(), String> {
    let mut errors = Vec::new();
    for (name, deps) in workspace_crates()? {
        let Some(layer) = layer_of(&name) else {
            errors.push(format!(
                "{name}: not assigned a layer (add it to LAYERS in xtask/src/main.rs)"
            ));
            continue;
        };
        for dep in &deps {
            if layer < APP_LAYER && UI_ONLY.contains(&dep.as_str()) {
                errors.push(format!("{name} (L{layer}) depends on the UI crate `{dep}`"));
            }
            if let Some(dep_layer) = layer_of(dep)
                && dep_layer >= layer
            {
                errors.push(format!(
                    "{name} (L{layer}) depends on {dep} (L{dep_layer}), which is not a lower layer"
                ));
            }
        }
    }
    if errors.is_empty() {
        println!("layers: ok");
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

pub(crate) fn cargo(args: &[&str]) -> Result<(), String> {
    println!("cargo {}", args.join(" "));
    let status = Command::new(env!("CARGO"))
        .args(args)
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo {} failed", args.join(" ")))
    }
}

/// The packages rustfmt and strict clippy check. The forked DashBeam crates
/// (`deskbeam-protocol`, `deskbeam-net`) keep upstream's formatting and lint state, so
/// upstream fixes still apply to them as patches.
const OWN: &[&str] = &[
    "deskbeam-model",
    "deskbeam-store",
    "deskbeam-tunnel",
    "deskbeam-rdp",
    "deskbeam-chat",
    "deskbeam-call",
    "deskbeam-lan",
    "deskbeam-engine",
    "deskbeam",
    "deskbeam-cli",
    "xtask",
];

fn ci() -> Result<(), String> {
    layers()?;
    let mut fmt = vec!["fmt"];
    for package in OWN {
        fmt.extend(["-p", package]);
    }
    fmt.extend(["--", "--check"]);
    cargo(&fmt)?;
    let mut clippy = vec!["clippy", "--all-targets", "--locked", "--no-deps"];
    for package in OWN {
        clippy.extend(["-p", package]);
    }
    clippy.extend(["--", "-D", "warnings"]);
    cargo(&clippy)?;
    cargo(&["test", "--workspace", "--locked", "--", "--test-threads=1"])
}

/// The sizes Windows asks an ICO for: Explorer's lists, the taskbar, and its largest view.
const ICO_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

/// The ICNS entries macOS reads, each a PNG: (type, side in pixels). The `ic1x` types are the
/// Retina (@2x) versions of the smaller sizes.
const ICNS_ENTRIES: [(&[u8; 4], u32); 10] = [
    (b"icp4", 16),
    (b"icp5", 32),
    (b"ic11", 32),
    (b"ic12", 64),
    (b"ic07", 128),
    (b"ic13", 256),
    (b"ic08", 256),
    (b"ic14", 512),
    (b"ic09", 512),
    (b"ic10", 1024),
];

/// Makes the Windows and macOS icons from `apps/deskbeam/assets/deskbeam.png` (1024×1024):
/// `deskbeam.ico`, which `build.rs` puts into `deskbeam.exe`, `deskbeam.icns`, for the
/// macOS app bundle, and `deskbeam-tray.png`, the tray's 64×64 picture. All are kept in the
/// repository, so a build needs neither this task nor an image library.
fn icon() -> Result<(), String> {
    let assets = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../apps/deskbeam/assets");
    let source =
        image::open(assets.join("deskbeam.png")).map_err(|e| format!("deskbeam.png: {e}"))?;
    let png = |side: u32| -> Result<Vec<u8>, String> {
        let resized = source.resize_exact(side, side, image::imageops::FilterType::Lanczos3);
        let mut bytes = Vec::new();
        resized
            .to_rgba8()
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .map_err(|e| e.to_string())?;
        Ok(bytes)
    };

    // An ICO of PNGs: a header, an entry for each size, then the pictures.
    let pictures: Vec<Vec<u8>> = ICO_SIZES
        .iter()
        .map(|side| png(*side))
        .collect::<Result<_, _>>()?;
    let mut ico = vec![0, 0, 1, 0];
    ico.extend((ICO_SIZES.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * ICO_SIZES.len() as u32;
    for (side, picture) in ICO_SIZES.iter().zip(&pictures) {
        // 0 stands for 256.
        let edge = if *side >= 256 { 0 } else { *side as u8 };
        ico.extend([edge, edge, 0, 0]);
        ico.extend(1u16.to_le_bytes());
        ico.extend(32u16.to_le_bytes());
        ico.extend((picture.len() as u32).to_le_bytes());
        ico.extend(offset.to_le_bytes());
        offset += picture.len() as u32;
    }
    pictures.iter().for_each(|picture| ico.extend(picture));
    std::fs::write(assets.join("deskbeam.ico"), ico).map_err(|e| e.to_string())?;

    // An ICNS: "icns" and the file's length, then each entry as its type, its length
    // (header included) and its PNG. Lengths are big-endian.
    let mut entries = Vec::new();
    for (kind, side) in ICNS_ENTRIES {
        let picture = png(side)?;
        entries.extend(kind);
        entries.extend((picture.len() as u32 + 8).to_be_bytes());
        entries.extend(picture);
    }
    let mut icns = b"icns".to_vec();
    icns.extend((entries.len() as u32 + 8).to_be_bytes());
    icns.extend(entries);
    std::fs::write(assets.join("deskbeam.icns"), icns).map_err(|e| e.to_string())?;

    // The tray's: small, since menu bars and notification areas show it at 16-32 points.
    std::fs::write(assets.join("deskbeam-tray.png"), png(64)?).map_err(|e| e.to_string())?;

    println!("icon: apps/deskbeam/assets/deskbeam.ico, deskbeam.icns and deskbeam-tray.png made");
    Ok(())
}

fn main() -> ExitCode {
    let task = std::env::args().nth(1);
    let result = match task.as_deref() {
        Some("layers") => layers(),
        Some("ci") => ci(),
        Some("icon") => icon(),
        Some("bundle") => bundle::run(std::env::args().skip(2).collect()),
        _ => Err("usage: cargo xtask <layers|ci|icon|bundle [--native]>".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
