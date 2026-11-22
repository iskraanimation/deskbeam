//! Virtual addresses (plan §2, option B).
//!
//! Every device's address follows from its endpoint id, so no device hands out addresses
//! and no lease table is needed (iroh-lan kept one in iroh-docs). The addresses live in
//! `100.88.0.0/16`, inside the shared address space of RFC 6598, which home networks and
//! Docker don't use; iroh-lan's `172.22.0.0/24` clashed with Docker.

use std::{collections::HashSet, net::Ipv4Addr};

use iroh::EndpointId;

/// The network every LAN-mode device is in.
pub const NETWORK: Ipv4Addr = Ipv4Addr::new(100, 88, 0, 0);
pub const PREFIX: u8 = 16;

/// Whether `ip` is in [`NETWORK`] and could be a device's address.
pub fn is_device_address(ip: Ipv4Addr) -> bool {
    let [a, b, _, d] = ip.octets();
    [a, b] == [100, 88] && d != 0 && d != 255
}

/// The address `id` gets with `salt`. Salt 0 is a device's first choice.
pub fn address_for(id: &EndpointId, salt: u32) -> Ipv4Addr {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"deskbeam lan address v1");
    hasher.update(id.as_bytes());
    hasher.update(&salt.to_le_bytes());
    let hash = hasher.finalize();
    let bytes = hash.as_bytes();
    // The last octet avoids 0 and 255, which some tools take for network or broadcast.
    Ipv4Addr::new(100, 88, bytes[0], 1 + bytes[1] % 254)
}

/// This device's address among its paired devices.
///
/// Two paired devices whose first choices clash would both see it: the one with the
/// larger id moves on to its next choice, so both end up agreeing without asking each
/// other. A clash between two devices that aren't paired with each other but share a
/// third is caught when the third connects them (see [`crate::Refusal::Conflict`]).
pub fn choose(me: &EndpointId, paired: &[EndpointId]) -> Ipv4Addr {
    let taken: HashSet<Ipv4Addr> = paired
        .iter()
        .filter(|p| p.as_bytes() < me.as_bytes())
        .map(|p| address_for(p, 0))
        .collect();
    (0..)
        .map(|salt| address_for(me, salt))
        .find(|a| !taken.contains(a))
        .expect("an address is free")
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    fn id(seed: u8) -> EndpointId {
        SecretKey::from_bytes(&[seed; 32]).public()
    }

    #[test]
    fn addresses_are_stable_and_in_the_network() {
        for seed in 0..=255u8 {
            let a = address_for(&id(seed), 0);
            assert_eq!(a, address_for(&id(seed), 0));
            assert!(is_device_address(a), "{a}");
        }
        assert_ne!(address_for(&id(1), 0), address_for(&id(1), 1));
    }

    #[test]
    fn device_addresses_exclude_the_edges() {
        assert!(is_device_address(Ipv4Addr::new(100, 88, 4, 7)));
        assert!(!is_device_address(Ipv4Addr::new(100, 88, 4, 0)));
        assert!(!is_device_address(Ipv4Addr::new(100, 88, 4, 255)));
        assert!(!is_device_address(Ipv4Addr::new(100, 89, 4, 7)));
        assert!(!is_device_address(Ipv4Addr::new(192, 168, 0, 7)));
    }

    #[test]
    fn the_larger_id_gives_way_on_a_clash() {
        // Find two devices whose first choices clash.
        let ids: Vec<EndpointId> = (0..=255u8).map(id).collect();
        let mut clash = None;
        'outer: for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                if address_for(a, 0) == address_for(b, 0) {
                    clash = Some((*a, *b));
                    break 'outer;
                }
            }
        }
        // 256 devices in ~65k addresses clash with good odds; fall back to checking
        // that nothing moves without a clash.
        let Some((a, b)) = clash else {
            assert_eq!(choose(&ids[0], &ids[1..4]), address_for(&ids[0], 0));
            return;
        };
        let (small, large) = if a.as_bytes() < b.as_bytes() {
            (a, b)
        } else {
            (b, a)
        };
        assert_eq!(choose(&small, &[large]), address_for(&small, 0));
        let moved = choose(&large, &[small]);
        assert_ne!(moved, address_for(&small, 0));
        assert_eq!(moved, address_for(&large, 1));
    }
}
