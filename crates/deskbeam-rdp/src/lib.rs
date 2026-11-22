//! Microsoft Remote Desktop on top of the tunnel (plan §2).
//!
//! DeskBeam does not speak RDP. It hands the system's own RDP client a `.rdp` file that
//! points at the tunnel's local port: mstsc on Windows, Windows App (formerly Microsoft
//! Remote Desktop) on macOS, FreeRDP or Remmina on Linux. The file turns on drive and
//! clipboard redirection, which is how files move inside a session.

use std::{
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

/// The port Remote Desktop listens on.
pub const RDP_PORT: u16 = 3389;

/// The `.rdp` file for a session through the tunnel on `port`.
///
/// - The user signs in with the host's Windows account, as on a LAN.
/// - Drives and the clipboard are shared into the session: `\\tsclient\…` on the host.
/// - The host's certificate is issued to its own name, not to `localhost`, so the client
///   warns about it and lets the user go on (`authentication level:i:2`). The tunnel itself
///   is already authenticated by the paired device's key.
pub fn rdp_file(port: u16) -> String {
    [
        format!("full address:s:localhost:{port}"),
        "prompt for credentials:i:1".into(),
        "authentication level:i:2".into(),
        "redirectclipboard:i:1".into(),
        "drivestoredirect:s:*".into(),
        "audiomode:i:0".into(),
        "screen mode id:i:2".into(),
        "use multimon:i:0".into(),
        "autoreconnection enabled:i:1".into(),
    ]
    .join("\r\n")
        + "\r\n"
}

/// Writes the `.rdp` file for `name` (the host's name) into `dir` and returns its path.
pub fn write_rdp_file(dir: &Path, name: &str, port: u16) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.rdp", file_name(name)));
    std::fs::write(&path, rdp_file(port))?;
    Ok(path)
}

/// A file name from a device name: letters, digits, spaces, dashes and underscores.
fn file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "DeskBeam".into()
    } else {
        cleaned.to_string()
    }
}

/// Opens the system's RDP client on the `.rdp` file.
pub fn launch(rdp_file: &Path, port: u16) -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    {
        let _ = port;
        Command::new("mstsc").arg(rdp_file).spawn()?;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        let _ = port;
        // Windows App (or the older Microsoft Remote Desktop) opens .rdp files.
        let status = Command::new("open").arg(rdp_file).status()?;
        anyhow::ensure!(
            status.success(),
            "no app opens .rdp files; install Windows App from the App Store"
        );
        Ok(())
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        // FreeRDP 3, then 2, then Remmina; the first one installed wins.
        let address = format!("/v:localhost:{port}");
        for freerdp in ["xfreerdp3", "xfreerdp"] {
            if Command::new(freerdp)
                .args([
                    address.as_str(),
                    "+clipboard",
                    "/drive:home,~",
                    "/cert:ignore",
                ])
                .spawn()
                .is_ok()
            {
                return Ok(());
            }
        }
        if Command::new("remmina")
            .arg("-c")
            .arg(rdp_file)
            .spawn()
            .is_ok()
        {
            return Ok(());
        }
        anyhow::bail!("no RDP client found; install FreeRDP (xfreerdp) or Remmina")
    }
}

/// Whether this machine can be controlled over Remote Desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSupport {
    /// Something accepts connections on `127.0.0.1:3389`.
    pub listening: bool,
    /// What to do about it, for this OS, when not listening.
    pub hint: String,
}

/// Looks for a Remote Desktop server on this machine's own port 3389.
pub fn host_support() -> HostSupport {
    host_support_at(SocketAddr::from((Ipv4Addr::LOCALHOST, RDP_PORT)))
}

pub fn host_support_at(address: SocketAddr) -> HostSupport {
    let listening = TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_ok();
    let hint = if listening {
        String::new()
    } else if cfg!(target_os = "windows") {
        "Turn on Remote Desktop in Settings ▸ System ▸ Remote Desktop. Windows Home editions \
         cannot be controlled; they can still connect to other devices."
            .into()
    } else if cfg!(target_os = "macos") {
        "macOS has no Remote Desktop server. This Mac can connect to other devices.".into()
    } else {
        "Install and start xrdp to let paired devices connect to this desktop.".into()
    };
    HostSupport { listening, hint }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rdp_file_points_at_the_tunnel_and_shares_drives() {
        let file = rdp_file(13390);
        assert!(file.contains("full address:s:localhost:13390\r\n"));
        assert!(file.contains("drivestoredirect:s:*"));
        assert!(file.contains("redirectclipboard:i:1"));
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(file_name("Office PC"), "Office PC");
        assert_eq!(file_name("../etc/passwd"), "___etc_passwd");
        assert_eq!(file_name("  "), "DeskBeam");
    }

    #[test]
    fn writes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_rdp_file(dir.path(), "Home PC", 13391).unwrap();
        assert_eq!(path.file_name().unwrap(), "Home PC.rdp");
        assert!(
            std::fs::read_to_string(path)
                .unwrap()
                .contains("localhost:13391")
        );
    }

    #[test]
    fn host_support_sees_a_listener() {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        assert!(host_support_at(address).listening);
        drop(listener);
        let closed = host_support_at(address);
        assert!(!closed.listening);
        assert!(!closed.hint.is_empty());
    }
}
