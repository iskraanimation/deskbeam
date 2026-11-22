//! The helper: the one part of LAN mode that runs with admin rights.
//!
//! Creating a network adapter needs them; DeskBeam itself never runs with them. When LAN
//! mode is turned on, the app starts its own program again as `<exe> --lan-helper …`
//! through the OS's prompt for admin rights (the password dialog on macOS, UAC on Windows,
//! polkit on Linux). The helper creates the adapter, connects back to the app over
//! loopback, proves it was started for this request with a one-time token, and then only
//! passes packets between the adapter and the app. When the app closes the connection
//! (LAN mode off, or the app quits or crashes), the helper exits and the adapter goes away.

use std::{
    future::Future,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    pin::Pin,
    time::Duration,
};

use anyhow::Context;
use bytes::Bytes;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tracing::{debug, warn};

use crate::{
    addr,
    device::{Device, QUEUE, Tasks},
    packet::{read_frame, write_frame},
};

/// The first argument that makes DeskBeam's programs run as the helper.
pub const ARG: &str = "--lan-helper";

/// How long the user has to answer the prompt for admin rights.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(180);
const TOKEN_LEN: usize = 64;
/// Starts the helper's first frame when it could not make the adapter.
const ERROR_PREFIX: &str = "error: ";

/// Starts the helper from `exe` with admin rights and returns its adapter.
pub async fn launch(exe: &Path, address: Ipv4Addr) -> anyhow::Result<Device> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    let token = new_token()?;
    let token_file = TokenFile::write(&token)?;

    let args = vec![
        ARG.to_string(),
        "--port".into(),
        port.to_string(),
        "--token-file".into(),
        token_file.0.to_string_lossy().into_owned(),
        "--address".into(),
        address.to_string(),
    ];
    let exited = elevate(exe, &args)?;

    let socket = tokio::select! {
        accepted = tokio::time::timeout(PROMPT_TIMEOUT, accept_helper(&listener, &token)) => {
            accepted.map_err(|_| anyhow::anyhow!("no answer to the request for administrator rights"))??
        }
        status = exited => {
            anyhow::bail!(match status {
                Ok(s) if s => "the helper stopped before it was ready".to_string(),
                Ok(_) => "administrator rights were not given".to_string(),
                Err(e) => format!("could not ask for administrator rights: {e}"),
            })
        }
    };
    drop(token_file);
    let _ = socket.set_nodelay(true);
    let mut socket = socket;
    // The helper's first frame says whether it made the adapter: its name, or an error.
    let ready = tokio::time::timeout(Duration::from_secs(30), read_frame(&mut socket))
        .await
        .map_err(|_| anyhow::anyhow!("the helper did not make the adapter in time"))??
        .context("the helper stopped before it was ready")?;
    let ready = String::from_utf8_lossy(&ready).into_owned();
    if let Some(error) = ready.strip_prefix(ERROR_PREFIX) {
        anyhow::bail!("{error}");
    }
    debug!(adapter = %ready, "LAN helper ready");
    Ok(pump_socket(socket, &ready))
}

/// Accepts connections until one presents the token.
async fn accept_helper(listener: &TcpListener, token: &str) -> anyhow::Result<TcpStream> {
    loop {
        let (mut socket, from) = listener.accept().await?;
        let mut presented = [0u8; TOKEN_LEN];
        let read =
            tokio::time::timeout(Duration::from_secs(5), socket.read_exact(&mut presented)).await;
        if matches!(read, Ok(Ok(_))) && same(&presented, token.as_bytes()) {
            return Ok(socket);
        }
        warn!(%from, "a connection to the LAN helper's port did not have the token");
    }
}

/// Compares without stopping at the first difference.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn new_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; TOKEN_LEN / 2];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("random: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The token in a file only this user (and the admin-rights helper) can read. The
/// command line, which other users can see, carries only its path.
struct TokenFile(PathBuf);

impl TokenFile {
    fn write(token: &str) -> anyhow::Result<Self> {
        let mut name = [0u8; 8];
        getrandom::fill(&mut name).map_err(|e| anyhow::anyhow!("random: {e}"))?;
        let name: String = name.iter().map(|b| format!("{b:02x}")).collect();
        let path = std::env::temp_dir().join(format!("deskbeam-lan-{name}.token"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        std::io::Write::write_all(&mut file, token.as_bytes())?;
        Ok(Self(path))
    }
}

impl Drop for TokenFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Resolves when the elevated process ends: `Ok(true)` if it exited successfully.
type Exited = Pin<Box<dyn Future<Output = std::io::Result<bool>> + Send>>;

/// Starts `exe args` with admin rights, through the OS's own prompt.
fn elevate(exe: &Path, args: &[String]) -> anyhow::Result<Exited> {
    #[cfg(target_os = "macos")]
    {
        let command = std::iter::once(exe.to_string_lossy().into_owned())
            .chain(args.iter().cloned())
            .map(|a| shell_quote(&a))
            .collect::<Vec<_>>()
            .join(" ");
        // Running in the background lets `do shell script` return once the helper starts;
        // the helper outlives it and ends when the app's connection closes.
        let script = format!(
            "do shell script \"{} > /dev/null 2>&1 &\" with prompt \"DeskBeam needs to create a network adapter for LAN mode.\" with administrator privileges",
            applescript_escape(&command)
        );
        let mut child = tokio::process::Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("starting osascript")?;
        Ok(Box::pin(async move {
            let ok = child.wait().await?.success();
            // A successful start returns at once; keep waiting for the helper itself.
            if ok {
                std::future::pending::<()>().await;
            }
            Ok(ok)
        }))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut child = tokio::process::Command::new("pkexec")
            .arg(exe)
            .args(args)
            .stdin(std::process::Stdio::null())
            .spawn()
            .context("starting pkexec (polkit)")?;
        Ok(Box::pin(async move { Ok(child.wait().await?.success()) }))
    }
    #[cfg(windows)]
    {
        let (exe, args) = (exe.to_path_buf(), args.to_vec());
        let task = tokio::task::spawn_blocking(move || {
            runas::Command::new(exe)
                .args(&args)
                .gui(true)
                .show(false)
                .status()
        });
        Ok(Box::pin(async move {
            match task.await {
                Ok(status) => Ok(status?.success()),
                Err(e) => Err(std::io::Error::other(e)),
            }
        }))
    }
}

#[cfg(target_os = "macos")]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(target_os = "macos")]
fn applescript_escape(s: &str) -> String {
    s.replace('\\', r"\\").replace('"', "\\\"")
}

/// Turns a connection carrying framed packets into a device.
fn pump_socket(socket: TcpStream, name: &str) -> Device {
    let (mut read, mut write) = socket.into_split();
    let (to_os, mut to_os_rx) = mpsc::channel::<Bytes>(QUEUE);
    let (from_os_tx, from_os) = mpsc::channel(QUEUE);
    let reader = tokio::spawn(async move {
        while let Ok(Some(packet)) = read_frame(&mut read).await {
            if from_os_tx.send(packet).await.is_err() {
                return;
            }
        }
        debug!("LAN helper connection closed");
    });
    let writer = tokio::spawn(async move {
        while let Some(packet) = to_os_rx.recv().await {
            if write_frame(&mut write, &packet).await.is_err() {
                return;
            }
        }
        let _ = write.shutdown().await;
    });
    Device::new(name.into(), to_os, from_os, Tasks(vec![reader, writer]))
}

/// The helper's command line.
#[derive(Debug, PartialEq, Eq)]
struct HelperArgs {
    port: u16,
    token_file: PathBuf,
    address: Ipv4Addr,
}

fn parse_args(args: &[String]) -> anyhow::Result<HelperArgs> {
    let mut port = None;
    let mut token_file = None;
    let mut address = None;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let value = it.next().with_context(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--port" => port = Some(value.parse()?),
            "--token-file" => token_file = Some(PathBuf::from(value)),
            "--address" => address = Some(value.parse()?),
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    let address: Ipv4Addr = address.context("--address is missing")?;
    // The helper runs with admin rights: it sets nothing but a LAN-mode address.
    anyhow::ensure!(
        addr::is_device_address(address),
        "{address} is not a LAN-mode address"
    );
    Ok(HelperArgs {
        port: port.context("--port is missing")?,
        token_file: token_file.context("--token-file is missing")?,
        address,
    })
}

/// Runs the helper if the program was started as one: call first thing in `main`. Returns
/// `None` when the first argument isn't [`ARG`], else the exit code.
pub fn run_if_helper() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some(ARG) {
        return None;
    }
    let result = parse_args(&args[1..]).and_then(|args| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(serve(args))
    });
    Some(match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("DeskBeam LAN helper: {e:#}");
            1
        }
    })
}

/// Creates the adapter and passes packets until the app hangs up.
async fn serve(args: HelperArgs) -> anyhow::Result<()> {
    let token = std::fs::read_to_string(&args.token_file)
        .with_context(|| format!("reading {}", args.token_file.display()))?;
    let _ = std::fs::remove_file(&args.token_file);
    anyhow::ensure!(token.len() == TOKEN_LEN, "the token file is not valid");

    let mut socket = TcpStream::connect((Ipv4Addr::LOCALHOST, args.port)).await?;
    let _ = socket.set_nodelay(true);
    socket.write_all(token.as_bytes()).await?;
    let mut device = match Device::open_here(args.address) {
        Ok(device) => {
            write_frame(&mut socket, device.name().as_bytes()).await?;
            device
        }
        Err(e) => {
            let message = format!("{ERROR_PREFIX}{e:#}");
            write_frame(&mut socket, message.as_bytes()).await?;
            return Err(e);
        }
    };
    let (mut read, mut write) = socket.into_split();

    let to_os = device.to_os.clone();
    let inbound = async move {
        while let Ok(Some(packet)) = read_frame(&mut read).await {
            if to_os.send(packet).await.is_err() {
                break;
            }
        }
    };
    let outbound = async {
        while let Some(packet) = device.from_os.recv().await {
            if write_frame(&mut write, &packet).await.is_err() {
                break;
            }
        }
    };
    // Either side ending ends the helper, and with it the adapter.
    tokio::select! {
        () = inbound => {}
        () = outbound => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_the_helper_command_line() {
        let args = parse_args(&strings(&[
            "--port",
            "4242",
            "--token-file",
            "/tmp/t",
            "--address",
            "100.88.3.4",
        ]))
        .unwrap();
        assert_eq!(
            args,
            HelperArgs {
                port: 4242,
                token_file: "/tmp/t".into(),
                address: Ipv4Addr::new(100, 88, 3, 4),
            }
        );
    }

    #[test]
    fn refuses_addresses_outside_the_lan() {
        let args = strings(&[
            "--port",
            "4242",
            "--token-file",
            "/tmp/t",
            "--address",
            "192.168.1.1",
        ]);
        assert!(parse_args(&args).is_err());
        assert!(parse_args(&strings(&["--port", "1"])).is_err());
        assert!(parse_args(&strings(&["--bogus", "1"])).is_err());
    }

    #[test]
    fn tokens_compare_whole() {
        let t = new_token().unwrap();
        assert_eq!(t.len(), TOKEN_LEN);
        assert!(same(t.as_bytes(), t.as_bytes()));
        assert!(!same(t.as_bytes(), new_token().unwrap().as_bytes()));
        assert!(!same(b"ab", b"abc"));
    }

    #[test]
    fn token_files_are_private_and_removed() {
        let file = TokenFile::write("secret").unwrap();
        let path = file.0.clone();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        drop(file);
        assert!(!path.exists());
    }

    /// The app's side against a stand-in helper: packets pass both ways, and a connection
    /// without the token is turned away.
    #[tokio::test]
    async fn the_app_side_checks_the_token_and_pumps_packets() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let token = new_token().unwrap();

        let helper_token = token.clone();
        let helper = tokio::spawn(async move {
            let mut stranger = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                .await
                .unwrap();
            stranger.write_all(&[b'x'; TOKEN_LEN]).await.unwrap();
            let mut socket = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                .await
                .unwrap();
            socket.write_all(helper_token.as_bytes()).await.unwrap();
            write_frame(&mut socket, b"utun9").await.unwrap();
            write_frame(&mut socket, b"from the adapter").await.unwrap();
            let back = read_frame(&mut socket).await.unwrap().unwrap();
            assert_eq!(back, &b"to the adapter"[..]);
        });

        let mut socket = accept_helper(&listener, &token).await.unwrap();
        assert_eq!(
            read_frame(&mut socket).await.unwrap().unwrap(),
            &b"utun9"[..]
        );
        let mut device = pump_socket(socket, "test");
        assert_eq!(
            device.from_os.recv().await.unwrap(),
            &b"from the adapter"[..]
        );
        device
            .to_os
            .send(Bytes::from_static(b"to the adapter"))
            .await
            .unwrap();
        helper.await.unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn quotes_for_the_shell_and_applescript() {
        assert_eq!(shell_quote("/Apps/Desk Beam/x"), "'/Apps/Desk Beam/x'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(applescript_escape(r#"a"b\c"#), r#"a\"b\\c"#);
    }
}
