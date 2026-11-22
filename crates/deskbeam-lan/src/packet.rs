//! The little of IPv4 that LAN mode reads, and the length-prefixed frames packets travel
//! in over streams.

use std::net::Ipv4Addr;

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The largest packet the adapter hands over: its MTU. Anything larger is not a packet
/// from it.
pub const MTU: u16 = 1280;

/// An IPv4 packet's source and destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv4Header {
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
}

/// Reads the header of an IPv4 packet; `None` when it isn't a whole one (IPv6 included).
pub fn parse(packet: &[u8]) -> Option<Ipv4Header> {
    if packet.len() < 20 || packet[0] >> 4 != 4 {
        return None;
    }
    let header_len = usize::from(packet[0] & 0x0f) * 4;
    let total = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    if header_len < 20 || total < header_len || total > packet.len() {
        return None;
    }
    let ip = |at: usize| Ipv4Addr::new(packet[at], packet[at + 1], packet[at + 2], packet[at + 3]);
    Some(Ipv4Header {
        src: ip(12),
        dst: ip(16),
    })
}

/// Writes one frame: the length as two big-endian bytes, then the packet.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, packet: &[u8]) -> std::io::Result<()> {
    let len = u16::try_from(packet.len())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "packet too large"))?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(packet).await
}

/// Reads one frame; `None` at a clean end of the stream.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<Bytes>> {
    let mut len = [0u8; 2];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let mut packet = vec![0u8; usize::from(u16::from_be_bytes(len))];
    r.read_exact(&mut packet).await?;
    Ok(Some(packet.into()))
}

/// A minimal IPv4/UDP packet, for tests.
pub fn udp_packet(src: Ipv4Addr, dst: Ipv4Addr, payload: &[u8]) -> Vec<u8> {
    let total = 20 + 8 + payload.len();
    let mut p = vec![0u8; total];
    p[0] = 0x45;
    p[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    p[8] = 64; // TTL
    p[9] = 17; // UDP
    p[12..16].copy_from_slice(&src.octets());
    p[16..20].copy_from_slice(&dst.octets());
    let mut sum: u32 = p[..20]
        .chunks(2)
        .map(|c| u32::from(u16::from_be_bytes([c[0], c[1]])))
        .sum();
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    p[10..12].copy_from_slice(&(!(sum as u16)).to_be_bytes());
    p[20..22].copy_from_slice(&9u16.to_be_bytes());
    p[22..24].copy_from_slice(&9u16.to_be_bytes());
    p[24..26].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    p[28..].copy_from_slice(payload);
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ipv4_and_rejects_the_rest() {
        let a = Ipv4Addr::new(100, 88, 1, 2);
        let b = Ipv4Addr::new(100, 88, 3, 4);
        let p = udp_packet(a, b, b"hello");
        assert_eq!(parse(&p), Some(Ipv4Header { src: a, dst: b }));
        assert_eq!(parse(&p[..19]), None);
        assert_eq!(
            parse(&p[..p.len() - 1]),
            None,
            "shorter than its total length"
        );
        let mut v6 = p.clone();
        v6[0] = 0x60;
        assert_eq!(parse(&v6), None);
    }

    #[tokio::test]
    async fn frames_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        write_frame(&mut a, b"one").await.unwrap();
        write_frame(&mut a, &[7u8; 1500]).await.unwrap();
        drop(a);
        assert_eq!(read_frame(&mut b).await.unwrap().unwrap(), &b"one"[..]);
        assert_eq!(read_frame(&mut b).await.unwrap().unwrap().len(), 1500);
        assert!(read_frame(&mut b).await.unwrap().is_none());
    }
}
