//! LAN mode end to end in one process: iroh endpoints talking directly (no relay), with
//! memory devices standing in for the network adapters.

use std::{
    collections::{HashMap, HashSet},
    net::Ipv4Addr,
    sync::{Arc, Mutex},
    time::Duration,
};

use bytes::Bytes;
use deskbeam_lan::{
    ALPN, Device, LanService, MemoryOs, PeerState, Peers, Refusal, addr, packet::udp_packet,
};
use iroh::{
    Endpoint, EndpointAddr, EndpointId,
    endpoint::{Connection, RelayMode, presets},
    protocol::Router,
};

const WAIT: Duration = Duration::from_secs(20);

/// Who a test device lets in, and where the others are.
#[derive(Clone, Default)]
struct Book {
    allowed: Arc<Mutex<HashSet<EndpointId>>>,
    addrs: Arc<Mutex<HashMap<EndpointId, EndpointAddr>>>,
    endpoint: Arc<Mutex<Option<Endpoint>>>,
}

impl Peers for Book {
    async fn allow(&self, peer: EndpointId) -> bool {
        self.allowed.lock().unwrap().contains(&peer)
    }

    async fn dial(&self, peer: EndpointId) -> anyhow::Result<Connection> {
        let addr = self.addrs.lock().unwrap().get(&peer).cloned();
        let addr = addr.ok_or_else(|| anyhow::anyhow!("unknown peer"))?;
        let endpoint = self.endpoint.lock().unwrap().clone().unwrap();
        Ok(endpoint.connect(addr, ALPN).await?)
    }
}

struct Node {
    id: EndpointId,
    address: Ipv4Addr,
    lan: LanService<Book>,
    book: Book,
    _router: Router,
}

impl Node {
    async fn new() -> Node {
        let endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        let book = Book::default();
        *book.endpoint.lock().unwrap() = Some(endpoint.clone());
        let lan = LanService::new(book.clone());
        let id = endpoint.id();
        let router = Router::builder(endpoint).accept(ALPN, lan.clone()).spawn();
        Node {
            id,
            address: addr::address_for(&id, 0),
            lan,
            book,
            _router: router,
        }
    }

    fn knows(&self, other: &Node) {
        self.book
            .addrs
            .lock()
            .unwrap()
            .insert(other.id, other._router.endpoint().addr());
    }

    fn allows(&self, other: &Node) {
        self.book.allowed.lock().unwrap().insert(other.id);
    }

    fn start(&self) -> MemoryOs {
        let (device, os) = Device::memory();
        self.lan.start(self.id, self.address, device).unwrap();
        os
    }

    async fn wait_for(&self, peer: &Node, what: impl Fn(&PeerState) -> bool) -> PeerState {
        let mut last = None;
        let reached = tokio::time::timeout(WAIT, async {
            loop {
                let state = self
                    .lan
                    .status()
                    .and_then(|s| s.peers.get(&peer.id).cloned());
                if let Some(state) = &state
                    && what(state)
                {
                    return state.clone();
                }
                last = state;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        reached.unwrap_or_else(|_| panic!("peer state never matched; last {last:?}"))
    }
}

async fn two_allowing_each_other() -> (Node, Node) {
    let (a, b) = (Node::new().await, Node::new().await);
    a.knows(&b);
    b.knows(&a);
    a.allows(&b);
    b.allows(&a);
    (a, b)
}

async fn receive(os: &mut MemoryOs) -> Bytes {
    tokio::time::timeout(WAIT, os.delivered.recv())
        .await
        .expect("no packet arrived")
        .expect("device closed")
}

async fn nothing_arrives(os: &mut MemoryOs) {
    let got = tokio::time::timeout(Duration::from_millis(500), os.delivered.recv()).await;
    assert!(got.is_err(), "a packet arrived: {got:?}");
}

fn connected(state: &PeerState) -> bool {
    matches!(state, PeerState::Connected { .. })
}

#[tokio::test]
async fn packets_go_both_ways_small_and_large() {
    let (a, b) = two_allowing_each_other().await;
    let mut a_os = a.start();
    let mut b_os = b.start();
    // Both dial each other at once; they must settle on one link.
    a.lan.set_wanted([b.id]);
    b.lan.set_wanted([a.id]);
    assert_eq!(
        a.wait_for(&b, connected).await,
        PeerState::Connected { address: b.address }
    );
    b.wait_for(&a, connected).await;

    let small = udp_packet(a.address, b.address, b"hello over the LAN");
    a_os.send.send(small.clone().into()).await.unwrap();
    assert_eq!(receive(&mut b_os).await, small);

    // Larger than a datagram: goes on the link's stream.
    let large = udp_packet(b.address, a.address, &[7u8; 4000]);
    b_os.send.send(large.clone().into()).await.unwrap();
    assert_eq!(receive(&mut a_os).await, large);

    // A full-size packet for the adapter's MTU, whichever way it goes.
    let full = udp_packet(a.address, b.address, &[1u8; 1280 - 28]);
    a_os.send.send(full.clone().into()).await.unwrap();
    assert_eq!(receive(&mut b_os).await, full);

    // Still connected after a while: the duplicate link was closed, not the one in use.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(connected(&a.wait_for(&b, |_| true).await));
    let again = udp_packet(b.address, a.address, b"still here");
    b_os.send.send(again.clone().into()).await.unwrap();
    assert_eq!(receive(&mut a_os).await, again);
}

#[tokio::test]
async fn packets_for_others_or_from_others_are_dropped() {
    let (a, b) = two_allowing_each_other().await;
    let mut a_os = a.start();
    let b_os = b.start();
    a.lan.set_wanted([b.id]);
    b.lan.set_wanted([a.id]);
    a.wait_for(&b, connected).await;

    // B's programs can't send as someone else, and nothing routes to unknown addresses.
    let elsewhere = Ipv4Addr::new(100, 88, 200, 200);
    let spoofed = udp_packet(elsewhere, a.address, b"spoofed");
    b_os.send.send(spoofed.into()).await.unwrap();
    let unknown = udp_packet(b.address, elsewhere, b"to nobody");
    b_os.send.send(unknown.into()).await.unwrap();
    nothing_arrives(&mut a_os).await;
}

/// A peer that skips its own checks still only reaches this device, from its own address.
#[tokio::test]
async fn a_peer_cannot_spoof_on_the_wire() {
    let (a, b) = two_allowing_each_other().await;
    let mut a_os = a.start();
    a.lan.set_wanted([b.id]);

    // B speaks the protocol by hand instead of running a LanService.
    let endpoint = b.book.endpoint.lock().unwrap().clone().unwrap();
    let conn = endpoint
        .connect(a._router.endpoint().addr(), ALPN)
        .await
        .unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let mut hello = b"DBL1".to_vec();
    hello.extend_from_slice(&b.address.octets());
    send.write_all(&hello).await.unwrap();
    let mut answer = [0u8; 5];
    recv.read_exact(&mut answer).await.unwrap();
    assert_eq!(answer[0], 0);
    assert_eq!(
        Ipv4Addr::new(answer[1], answer[2], answer[3], answer[4]),
        a.address
    );

    let other = Ipv4Addr::new(100, 88, 9, 9);
    conn.send_datagram(udp_packet(other, a.address, b"posing").into())
        .unwrap();
    conn.send_datagram(udp_packet(b.address, other, b"elsewhere").into())
        .unwrap();
    nothing_arrives(&mut a_os).await;

    let honest = udp_packet(b.address, a.address, b"honest");
    conn.send_datagram(honest.clone().into()).unwrap();
    assert_eq!(receive(&mut a_os).await, honest);
}

#[tokio::test]
async fn a_device_that_did_not_allow_this_one_refuses() {
    let (a, b) = (Node::new().await, Node::new().await);
    a.knows(&b);
    a.allows(&b);
    let _a_os = a.start();
    let _b_os = b.start();
    a.lan.set_wanted([b.id]);
    assert_eq!(
        a.wait_for(&b, |s| matches!(s, PeerState::Refused(_))).await,
        PeerState::Refused(Refusal::Denied)
    );
}

#[tokio::test]
async fn a_device_with_lan_mode_off_says_so_and_is_tried_again() {
    let (a, b) = two_allowing_each_other().await;
    let _a_os = a.start();
    a.lan.set_wanted([b.id]);
    assert_eq!(
        a.wait_for(&b, |s| matches!(s, PeerState::Refused(_))).await,
        PeerState::Refused(Refusal::Off)
    );

    // B turns LAN mode on and dials A itself.
    let _b_os = b.start();
    b.lan.set_wanted([a.id]);
    a.wait_for(&b, connected).await;
}

#[tokio::test]
async fn stopping_and_unwanting_close_the_link() {
    let (a, b) = two_allowing_each_other().await;
    let _a_os = a.start();
    let _b_os = b.start();
    a.lan.set_wanted([b.id]);
    b.lan.set_wanted([a.id]);
    a.wait_for(&b, connected).await;

    // A no longer wants B (say, its permission was turned off): B sees the link go, and
    // is turned away when it dials again.
    a.lan.set_wanted([]);
    assert!(a.lan.status().unwrap().peers.is_empty());
    assert_eq!(
        b.wait_for(&a, |s| matches!(s, PeerState::Refused(_))).await,
        PeerState::Refused(Refusal::Denied)
    );

    a.lan.set_wanted([b.id]);
    a.wait_for(&b, connected).await;
    b.lan.stop();
    assert!(b.lan.status().is_none());
    a.wait_for(&b, |s| !connected(s)).await;
}

#[tokio::test]
async fn a_clashing_address_is_refused() {
    let (a, b) = two_allowing_each_other().await;
    let _a_os = a.start();
    a.lan.set_wanted([b.id]);
    // B claims A's address.
    let (device, _b_os) = Device::memory();
    b.lan.start(b.id, a.address, device).unwrap();
    b.lan.set_wanted([a.id]);
    assert_eq!(
        b.wait_for(&a, |s| matches!(s, PeerState::Refused(_))).await,
        PeerState::Refused(Refusal::Conflict)
    );
}
