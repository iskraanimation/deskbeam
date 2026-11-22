//! System notifications, for things that need the user while the window is closed.
//!
//! Message notifications are kept per chat, so that reading the chat removes them from the
//! screen and the notification centre ([`clear_messages`]): on Windows by the toast's group,
//! on Linux by the handle the notification server gave back. macOS's notifications go out
//! through `NSUserNotificationCenter`, which notify-rust can't remove them from, so they stay
//! until dismissed there.

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

/// Shows a notification without waiting for it: on some systems showing one blocks.
pub fn show(summary: String, body: String) {
    std::thread::spawn(move || {
        #[cfg(target_os = "macos")]
        set_mac_sender();
        if let Err(e) = notify_rust::Notification::new()
            .appname("DeskBeam")
            .summary(&summary)
            .body(&body)
            .show()
        {
            tracing::debug!("notification not shown: {e}");
        }
    });
}

/// How many times each chat's notifications were cleared. A notification still being shown
/// when its chat is read sees the count change and removes itself.
fn cleared() -> &'static Mutex<HashMap<String, u64>> {
    static CLEARED: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    CLEARED.get_or_init(Mutex::default)
}

fn times_cleared(conversation: &str) -> u64 {
    cleared()
        .lock()
        .expect("lock")
        .get(conversation)
        .copied()
        .unwrap_or(0)
}

/// Shows a notification for a new message in `conversation`, to be removed by
/// [`clear_messages`] once that chat is read.
pub fn show_message(conversation: String, summary: String, body: String) {
    let asked = times_cleared(&conversation);
    std::thread::spawn(move || {
        let shown = platform::show_message(&conversation, &summary, &body);
        if shown && times_cleared(&conversation) != asked {
            // Read while it was being shown.
            platform::clear_messages(&conversation);
        }
    });
}

/// Removes the message notifications of `conversation`, now that it has been read.
pub fn clear_messages(conversation: &str) {
    *cleared()
        .lock()
        .expect("lock")
        .entry(conversation.to_string())
        .or_default() += 1;
    let conversation = conversation.to_string();
    std::thread::spawn(move || platform::clear_messages(&conversation));
}

#[cfg(target_os = "windows")]
mod platform {
    //! Toasts made here rather than through notify-rust, which gives them no group to remove
    //! them by.

    use windows::{
        Data::Xml::Dom::XmlDocument,
        UI::Notifications::{ToastNotification, ToastNotificationManager},
        core::HSTRING,
    };

    /// notify-rust's sender for an app with no registered AppUserModelID, so that message
    /// toasts look like DeskBeam's other notifications.
    const APP_ID: &str =
        "{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe";

    pub fn show_message(conversation: &str, summary: &str, body: &str) -> bool {
        let shown = (|| -> windows::core::Result<()> {
            let xml = XmlDocument::new()?;
            xml.LoadXml(&HSTRING::from(format!(
                "<toast><visual><binding template=\"ToastGeneric\">\
                 <text>{}</text><text>{}</text>\
                 </binding></visual></toast>",
                escape(summary),
                escape(body)
            )))?;
            let toast = ToastNotification::CreateToastNotification(&xml)?;
            toast.SetGroup(&group(conversation))?;
            ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))?
                .Show(&toast)
        })();
        if let Err(e) = &shown {
            tracing::debug!("message toast not shown: {e}");
        }
        shown.is_ok()
    }

    pub fn clear_messages(conversation: &str) {
        let removed = ToastNotificationManager::History().and_then(|history| {
            history.RemoveGroupWithId(&group(conversation), &HSTRING::from(APP_ID))
        });
        if let Err(e) = removed {
            tracing::debug!("message toasts not removed: {e}");
        }
    }

    /// A toast's group is at most 64 characters, and conversation ids are longer.
    fn group(conversation: &str) -> HSTRING {
        HSTRING::from(format!("chat-{:016x}", super::short_hash(conversation)))
    }

    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use std::{
        collections::HashMap,
        sync::{Mutex, OnceLock},
    };

    use notify_rust::NotificationHandle;

    /// What is on screen per chat, to close it by.
    fn handles() -> &'static Mutex<HashMap<String, Vec<NotificationHandle>>> {
        static HANDLES: OnceLock<Mutex<HashMap<String, Vec<NotificationHandle>>>> = OnceLock::new();
        HANDLES.get_or_init(Mutex::default)
    }

    pub fn show_message(conversation: &str, summary: &str, body: &str) -> bool {
        match notify_rust::Notification::new()
            .appname("DeskBeam")
            .summary(summary)
            .body(body)
            .show()
        {
            Ok(handle) => {
                handles()
                    .lock()
                    .expect("lock")
                    .entry(conversation.to_string())
                    .or_default()
                    .push(handle);
                true
            }
            Err(e) => {
                tracing::debug!("notification not shown: {e}");
                false
            }
        }
    }

    pub fn clear_messages(conversation: &str) {
        let shown = handles().lock().expect("lock").remove(conversation);
        for handle in shown.into_iter().flatten() {
            handle.close();
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    pub fn show_message(_conversation: &str, summary: &str, body: &str) -> bool {
        super::show(summary.to_string(), body.to_string());
        false
    }

    pub fn clear_messages(_conversation: &str) {}
}

/// A short, stable hash of `text` (FNV-1a), the same in every run.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn short_hash(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// DeskBeam's bundle id, as `cargo xtask bundle` writes it into `Info.plist`
/// (`xtask/src/bundle.rs`).
#[cfg(target_os = "macos")]
const BUNDLE_ID: &str = "org.deskbeam.DeskBeam";

/// On macOS a notification is sent as some app, named by bundle id. Left unset, the library
/// asks AppleScript for the id of an app called "use_default", which makes macOS put up a
/// "Choose Application" dialog. Inside `DeskBeam.app` they are sent as DeskBeam, with its
/// name and icon; the bare program (`cargo run`) sends them as Terminal, which it runs from.
#[cfg(target_os = "macos")]
fn set_mac_sender() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let sender = if in_app_bundle() {
            BUNDLE_ID
        } else {
            "com.apple.Terminal"
        };
        if let Err(e) = notify_rust::set_application(sender) {
            tracing::debug!("notifications: {e}");
        }
    });
}

/// Whether this program runs from inside an app bundle (`….app/Contents/MacOS/deskbeam`).
#[cfg(target_os = "macos")]
fn in_app_bundle() -> bool {
    std::env::current_exe().is_ok_and(|exe| {
        exe.parent()
            .is_some_and(|dir| dir.ends_with("Contents/MacOS"))
    })
}

#[cfg(test)]
mod tests {
    use super::short_hash;

    #[test]
    fn chat_groups_are_stable_and_distinct() {
        assert_eq!(short_hash("g:abc"), short_hash("g:abc"));
        assert_ne!(short_hash("g:abc"), short_hash("g:abd"));
        assert_eq!(short_hash(""), 0xcbf2_9ce4_8422_2325);
    }
}
