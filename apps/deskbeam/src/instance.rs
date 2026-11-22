//! One app per data directory.
//!
//! The first app takes the data directory's lock and listens on a loopback port, which it
//! writes to `deskbeam.port`. A second launch finds the lock taken, asks the first app over
//! that port to show its window, and quits. Only "show" is understood, so another local
//! program sending it can do no more than bring the window up.

use std::{
    io::{BufRead, BufReader, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::Path,
    time::Duration,
};

use deskbeam_engine::{DataDirLock, LockError};

const PORT_FILE: &str = "deskbeam.port";
const SHOW: &str = "show";

pub enum Claim {
    /// This is the only app on the data directory. Keep the lock while the app runs.
    First {
        lock: DataDirLock,
        listener: TcpListener,
    },
    /// Another app has the data directory; it was asked to show its window.
    AlreadyRunning,
}

pub fn claim(data_dir: &Path) -> anyhow::Result<Claim> {
    match DataDirLock::acquire(data_dir) {
        Ok(lock) => {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
            let port = listener.local_addr()?.port();
            std::fs::write(data_dir.join(PORT_FILE), port.to_string())?;
            Ok(Claim::First { lock, listener })
        }
        Err(LockError::InUse(_)) => {
            if let Err(e) = ask_to_show(data_dir) {
                tracing::warn!("DeskBeam is already running, but asking it to show failed: {e:#}");
            }
            Ok(Claim::AlreadyRunning)
        }
        Err(e) => Err(e.into()),
    }
}

fn ask_to_show(data_dir: &Path) -> anyhow::Result<()> {
    let port: u16 = std::fs::read_to_string(data_dir.join(PORT_FILE))?
        .trim()
        .parse()?;
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.write_all(format!("{SHOW}\n").as_bytes())?;
    Ok(())
}

/// Calls `on_show` for every "show" a later launch sends, on a thread of its own.
pub fn listen(listener: TcpListener, on_show: impl Fn() + Send + 'static) {
    std::thread::Builder::new()
        .name("instance".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut line = String::new();
                if BufReader::new(stream).read_line(&mut line).is_ok() && line.trim() == SHOW {
                    on_show();
                }
            }
        })
        .expect("start the instance thread");
}
