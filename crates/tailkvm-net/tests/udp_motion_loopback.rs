//! End-to-end UDP motion transport over localhost: proves the datagram codec
//! and sequence gate work across a real socket without needing two machines.

use std::net::{IpAddr, Ipv4Addr};
use tailkvm_net::motion::{is_expected_peer, Motion, MotionDatagram, SeqGate, SeqSource};
use tokio::net::UdpSocket;

#[tokio::test]
async fn motion_datagrams_round_trip_over_localhost_udp() {
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let receiver_addr = receiver.local_addr().unwrap();

    let mut seq = SeqSource::new();
    let mut gate = SeqGate::new();
    let expected_ip = IpAddr::V4(Ipv4Addr::LOCALHOST);

    // Send three positions in order.
    for (x, y) in [(10, 10), (20, 25), (30, 40)] {
        let dg = MotionDatagram {
            seq: seq.next(),
            motion: Motion::SetPosition { x, y },
        };
        sender.send_to(&dg.encode(), receiver_addr).await.unwrap();
    }

    // Receive and apply through the gate; the last accepted position wins.
    let mut applied: Option<(i32, i32)> = None;
    let mut buf = [0u8; 64];
    for _ in 0..3 {
        let (n, from) = receiver.recv_from(&mut buf).await.unwrap();
        assert!(is_expected_peer(from, expected_ip));
        let dg = MotionDatagram::decode(&buf[..n]).expect("valid datagram");
        if gate.accept(dg.seq) {
            if let Motion::SetPosition { x, y } = dg.motion {
                applied = Some((x, y));
            }
        }
    }
    assert_eq!(applied, Some((30, 40)), "newest position is applied");
}

#[tokio::test]
async fn reordered_stale_datagram_is_dropped_by_the_gate() {
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let receiver_addr = receiver.local_addr().unwrap();
    let mut gate = SeqGate::new();

    // Deliberately send seq=5 (newer) before seq=3 (older), simulating UDP
    // reordering. The gate must keep the newer position.
    let newer = MotionDatagram {
        seq: 5,
        motion: Motion::SetPosition { x: 500, y: 500 },
    };
    let older = MotionDatagram {
        seq: 3,
        motion: Motion::SetPosition { x: 3, y: 3 },
    };
    sender.send_to(&newer.encode(), receiver_addr).await.unwrap();
    sender.send_to(&older.encode(), receiver_addr).await.unwrap();

    let mut buf = [0u8; 64];
    let mut applied: Option<(i32, i32)> = None;
    for _ in 0..2 {
        let (n, _) = receiver.recv_from(&mut buf).await.unwrap();
        let dg = MotionDatagram::decode(&buf[..n]).unwrap();
        if gate.accept(dg.seq) {
            if let Motion::SetPosition { x, y } = dg.motion {
                applied = Some((x, y));
            }
        }
    }
    assert_eq!(
        applied,
        Some((500, 500)),
        "the stale reordered datagram must not overwrite the newer position"
    );
}
