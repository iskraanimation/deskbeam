//! The tray icon (the macOS menu bar, the Windows notification area), so DeskBeam keeps
//! running and reachable with its window closed.
//!
//! Linux comes later: its tray needs GTK running on a thread of its own (plan §3.1b).
//!
//! On macOS this also sets the app menu ("DeskBeam" in the menu bar), with Settings…: the
//! menu library has one event handler for all its menus, so both are made here.

/// What the tray's menu asks the app to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    Open,
    /// Open the Settings window.
    Settings,
    Quit,
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod imp {
    use super::TrayAction;
    use tray_icon::{
        Icon, TrayIcon, TrayIconBuilder, TrayIconEvent,
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    };

    pub struct Tray {
        icon: TrayIcon,
        status: MenuItem,
        shown: String,
        /// Kept alive while it is the menu bar's.
        #[cfg(target_os = "macos")]
        _app_menu: Menu,
    }

    /// The macOS menu bar's "DeskBeam" menu. It replaces winit's default one, which has
    /// no Settings. No Edit menu: its Copy and Paste would take the keys from egui.
    #[cfg(target_os = "macos")]
    mod app_menu {
        use super::TrayAction;
        use tray_icon::menu::{
            AboutMetadata, Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu,
            accelerator::{Accelerator, Code, Modifiers},
        };

        pub struct AppMenu {
            pub menu: Menu,
            pub actions: Vec<(MenuId, TrayAction)>,
        }

        pub fn make() -> anyhow::Result<AppMenu> {
            let settings = MenuItem::new(
                "Settings…",
                true,
                Some(Accelerator::new(Modifiers::META, Code::Comma)),
            );
            // Quits like the tray's Quit, past "keep running in the tray".
            let quit = MenuItem::new(
                "Quit DeskBeam",
                true,
                Some(Accelerator::new(Modifiers::META, Code::KeyQ)),
            );
            let about = AboutMetadata {
                name: Some("DeskBeam".into()),
                version: Some(env!("CARGO_PKG_VERSION").into()),
                license: Some("AGPL-3.0-only".into()),
                ..Default::default()
            };
            let deskbeam = Submenu::new("DeskBeam", true);
            deskbeam.append_items(&[
                &PredefinedMenuItem::about(Some("About DeskBeam"), Some(about)),
                &PredefinedMenuItem::separator(),
                &settings,
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::services(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::hide(Some("Hide DeskBeam")),
                &PredefinedMenuItem::hide_others(None),
                &PredefinedMenuItem::show_all(None),
                &PredefinedMenuItem::separator(),
                &quit,
            ])?;
            let menu = Menu::new();
            menu.append(&deskbeam)?;
            menu.init_for_nsapp();
            Ok(AppMenu {
                actions: vec![
                    (settings.id().clone(), TrayAction::Settings),
                    (quit.id().clone(), TrayAction::Quit),
                ],
                menu,
            })
        }
    }

    impl Tray {
        /// Makes the tray icon. Must run on the main thread once the event loop runs, as the
        /// app's creation does. `on_action` runs on that thread too.
        pub fn new(on_action: impl Fn(TrayAction) + Send + Sync + 'static) -> anyhow::Result<Self> {
            let open = MenuItem::new("Open DeskBeam", true, None);
            let settings = MenuItem::new("Settings…", true, None);
            let status = MenuItem::new("Starting…", false, None);
            let quit = MenuItem::new("Quit DeskBeam", true, None);
            let menu = Menu::new();
            menu.append_items(&[
                &open,
                &settings,
                &PredefinedMenuItem::separator(),
                &status,
                &PredefinedMenuItem::separator(),
                &quit,
            ])?;

            let picture =
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/deskbeam-tray.png"))?;
            let icon = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip("DeskBeam")
                .with_icon(Icon::from_rgba(
                    picture.rgba,
                    picture.width,
                    picture.height,
                )?)
                .build()?;

            #[cfg(target_os = "macos")]
            let app_menu = app_menu::make()?;
            let mut actions = vec![
                (open.id().clone(), TrayAction::Open),
                (settings.id().clone(), TrayAction::Settings),
                (quit.id().clone(), TrayAction::Quit),
            ];
            #[cfg(target_os = "macos")]
            actions.extend(app_menu.actions.iter().cloned());

            let on_action = std::sync::Arc::new(on_action);
            let menu_action = on_action.clone();
            MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
                if let Some((_, action)) = actions.iter().find(|(id, _)| *id == event.id) {
                    menu_action(*action);
                }
            }));
            // On Windows, double-clicking the icon opens the window, as tray apps do there.
            TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
                if matches!(event, TrayIconEvent::DoubleClick { .. }) {
                    on_action(TrayAction::Open);
                }
            }));

            Ok(Self {
                icon,
                status,
                shown: String::new(),
                #[cfg(target_os = "macos")]
                _app_menu: app_menu.menu,
            })
        }

        /// The status line in the menu and the tooltip, e.g. "2 of 3 devices online".
        pub fn set_status(&mut self, text: &str) {
            if self.shown != text {
                self.status.set_text(text);
                let _ = self.icon.set_tooltip(Some(format!("DeskBeam: {text}")));
                self.shown = text.to_string();
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod imp {
    use super::TrayAction;

    /// No tray yet on this platform; the app keeps its window.
    pub struct Tray;

    impl Tray {
        pub fn new(
            _on_action: impl Fn(TrayAction) + Send + Sync + 'static,
        ) -> anyhow::Result<Self> {
            anyhow::bail!("no tray on this platform yet")
        }

        pub fn set_status(&mut self, _text: &str) {}
    }
}

pub use imp::Tray;

/// Whether Settings is in the menu bar's app menu, so the window needs no button for it.
pub const APP_MENU: bool = cfg!(target_os = "macos");

/// Whether this platform has a tray, so closing the window can keep the app running.
pub const AVAILABLE: bool = cfg!(any(target_os = "macos", target_os = "windows"));
