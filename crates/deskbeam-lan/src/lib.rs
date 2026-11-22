//! LAN mode (plan §2, option B): a virtual network between paired devices.
//!
//! Each device in LAN mode gets an address in `100.88.0.0/16` ([`addr`]) on a virtual
//! network adapter ([`Device`]). Packets for another device's address go to it over iroh
//! ([`LanService`]), so any program that works on a local network (file shares, ping,
//! games, `mstsc` to the address) works between paired devices anywhere.
//!
//! Unlike the remote-desktop tunnel, this makes every service listening on a device
//! reachable to the devices it lets in, and the adapter needs admin rights, which only the
//! [`helper`] process gets. So it is off until turned on, and each device must allow each
//! other device.
//!
//! Ported from iroh-lan (<https://github.com/rustonbsd/iroh-lan>, MIT, see
//! `LICENSE-MIT-iroh-lan`).

pub mod addr;
mod device;
pub mod helper;
pub mod packet;
mod service;

pub use device::{Device, MemoryOs};
pub use service::{ALPN, LanService, LanStatus, PeerState, Peers, Refusal};
