//! On Windows, the app's icon goes into `deskbeam.exe`, with its name under *Properties ▸
//! Details*: Explorer, the taskbar and shortcuts show the file's own icon, not the window's.
//! Elsewhere there is nothing to do here: the window is given the icon when it opens, and a
//! macOS app bundle carries `assets/deskbeam.icns`.

fn main() {
    println!("cargo:rerun-if-changed=assets/deskbeam.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("assets/deskbeam.ico")
            .set("ProductName", "DeskBeam")
            .set(
                "FileDescription",
                "DeskBeam: remote desktop, files and chat between paired devices",
            );
        // Without the Windows resource compiler the app is still built, only with no icon.
        if let Err(why) = resource.compile() {
            println!("cargo:warning=the icon could not be put into deskbeam.exe: {why}");
        }
    }
}
