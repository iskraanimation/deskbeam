//! Makes the LAN adapter in this process and shows what programs send into it. Needs admin
//! rights: `sudo cargo run -p deskbeam-lan --example adapter`, then `ping 100.88.0.2` in
//! another terminal and watch the packets arrive.

use std::net::Ipv4Addr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let address = Ipv4Addr::new(100, 88, 0, 1);
    let mut device = deskbeam_lan::Device::open_here(address)?;
    println!(
        "adapter {} at {address}; try `ping 100.88.0.2`",
        device.name()
    );
    while let Some(packet) = device.recv().await {
        if let Some(h) = deskbeam_lan::packet::parse(&packet) {
            println!("{} -> {} ({} bytes)", h.src, h.dst, packet.len());
        }
    }
    Ok(())
}
