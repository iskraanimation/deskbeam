//! The virtual network adapter, seen as two channels of IPv4 packets.
//!
//! The LAN service doesn't care where the adapter lives: in this process (when it runs
//! with admin rights), in the helper process that got them ([`crate::helper`]), or in
//! memory for tests. Dropping a [`Device`] removes the adapter.
//!
//! The adapter setup follows iroh-lan's `local_networking.rs` (MIT, see
//! `LICENSE-MIT-iroh-lan`): a layer-3 TUN through `tun-rs`, wintun on Windows.

use std::{any::Any, net::Ipv4Addr, path::Path, sync::Arc};

use bytes::Bytes;
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::{debug, info};

use crate::{
    addr,
    packet::{self, MTU},
};

/// Packets queued towards or from the adapter. A full queue drops packets, as a busy
/// network would; TCP sends them again.
pub(crate) const QUEUE: usize = 1024;

/// Packets between the LAN service and the operating system's network stack.
pub struct Device {
    pub(crate) to_os: mpsc::Sender<Bytes>,
    pub(crate) from_os: mpsc::Receiver<Bytes>,
    name: String,
    _keep: Box<dyn Any + Send + Sync>,
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device").field("name", &self.name).finish()
    }
}

/// The operating system's side of a memory device: what tests send and see.
#[derive(Debug)]
pub struct MemoryOs {
    /// Packets "sent by programs" on this device, for the LAN to carry.
    pub send: mpsc::Sender<Bytes>,
    /// Packets the LAN delivered to this device.
    pub delivered: mpsc::Receiver<Bytes>,
}

/// Aborts tasks when dropped.
pub(crate) struct Tasks(pub(crate) Vec<JoinHandle<()>>);

impl Drop for Tasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

impl Device {
    pub(crate) fn new(
        name: String,
        to_os: mpsc::Sender<Bytes>,
        from_os: mpsc::Receiver<Bytes>,
        keep: impl Any + Send + Sync,
    ) -> Self {
        Self {
            to_os,
            from_os,
            name,
            _keep: Box::new(keep),
        }
    }

    /// A device with no adapter behind it, for tests.
    pub fn memory() -> (Device, MemoryOs) {
        let (send, from_os) = mpsc::channel(QUEUE);
        let (to_os, delivered) = mpsc::channel(QUEUE);
        (
            Device::new("memory".into(), to_os, from_os, ()),
            MemoryOs { send, delivered },
        )
    }

    /// The next packet programs on this device sent into the LAN. For tools and examples;
    /// the LAN service reads them itself.
    pub async fn recv(&mut self) -> Option<Bytes> {
        self.from_os.recv().await
    }

    /// The adapter's name as the OS shows it (`utun5`, `tun0`, `DeskBeam`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Creates the adapter in this process with `address`. Needs admin rights.
    pub fn open_here(address: Ipv4Addr) -> anyhow::Result<Device> {
        anyhow::ensure!(
            addr::is_device_address(address),
            "{address} is not a LAN-mode address"
        );
        let builder = tun_rs::DeviceBuilder::new()
            .ipv4(address, addr::PREFIX, None)
            .mtu(MTU);
        #[cfg(windows)]
        let builder = builder
            .name("DeskBeam")
            .description("DeskBeam LAN")
            .wintun_file(wintun_path()?.to_string_lossy().into_owned());
        let device = Arc::new(builder.build_async()?);
        let name = device.name().unwrap_or_else(|_| "tun".into());
        info!(%address, %name, "LAN adapter up");

        let (to_os, mut to_os_rx) = mpsc::channel::<Bytes>(QUEUE);
        let (from_os_tx, from_os) = mpsc::channel(QUEUE);
        let reader = {
            let device = device.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                loop {
                    let n = match device.recv(&mut buf).await {
                        Ok(n) => n,
                        Err(e) => {
                            debug!("LAN adapter read: {e}");
                            return;
                        }
                    };
                    if packet::parse(&buf[..n]).is_none() {
                        continue;
                    }
                    match from_os_tx.try_send(Bytes::copy_from_slice(&buf[..n])) {
                        Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                        Err(mpsc::error::TrySendError::Closed(_)) => return,
                    }
                }
            })
        };
        let writer = tokio::spawn(async move {
            while let Some(packet) = to_os_rx.recv().await {
                if let Err(e) = device.send(&packet).await {
                    debug!("LAN adapter write: {e}");
                }
            }
        });
        Ok(Device::new(
            name,
            to_os,
            from_os,
            Tasks(vec![reader, writer]),
        ))
    }

    /// Creates the adapter here if this process may, else through the helper started with
    /// admin rights from `helper_exe` (the OS asks the user for them).
    pub async fn open(address: Ipv4Addr, helper_exe: Option<&Path>) -> anyhow::Result<Device> {
        #[cfg(windows)]
        wintun_path()?;
        let here = match Device::open_here(address) {
            Ok(device) => return Ok(device),
            Err(e) => e,
        };
        debug!("LAN adapter in this process: {here:#}");
        let Some(exe) = helper_exe else {
            anyhow::bail!("LAN mode needs administrator rights: {here:#}");
        };
        crate::helper::launch(exe, address).await
    }
}

/// `wintun.dll` next to the program, where tun-rs loads it from.
#[cfg(windows)]
fn wintun_path() -> anyhow::Result<std::path::PathBuf> {
    let path = std::env::current_exe()?.with_file_name("wintun.dll");
    anyhow::ensure!(
        path.exists(),
        "LAN mode needs wintun.dll next to {} (from https://www.wintun.net)",
        path.parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );
    Ok(path)
}
