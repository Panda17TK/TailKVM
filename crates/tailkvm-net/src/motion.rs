//! UDP motion datagrams (protocol v2, opt-in).
//!
//! High-frequency pointer motion is loss-tolerant: a dropped `SetPosition` is
//! superseded by the next one, and relative `Move` deltas can be coalesced.
//! Carrying motion over UDP instead of the TCP control stream removes it from
//! the head-of-line-blocking path — a stalled TCP receive window can no longer
//! wedge motion (or, on the receiver, the failsafe tick that shares the TCP
//! session loop). Control-plane messages (Hello, Heartbeat, keys, clipboard,
//! disconnect) stay on TCP where ordering and reliability matter.
//!
//! Wire format (little-endian, fixed 17 bytes): `u64 seq | u8 tag | i32 a | i32 b`.
//! A compact binary frame keeps the per-datagram cost far below the JSON control
//! messages, which matters at >125 Hz. Datagrams are self-contained (no framing)
//! because UDP preserves message boundaries.

use std::net::SocketAddr;

const TAG_SET_POSITION: u8 = 1;
const TAG_MOVE: u8 = 2;

/// Encoded size of one motion datagram: seq(8) + tag(1) + two i32 (8) = 17.
pub const MOTION_DATAGRAM_LEN: usize = 8 + 1 + 4 + 4;

/// A single motion event carried over UDP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    /// Absolute cursor position (virtual-screen coordinates) — newest wins.
    SetPosition { x: i32, y: i32 },
    /// Relative movement delta — accumulated by the receiver.
    Move { dx: i32, dy: i32 },
}

/// A sequence-numbered motion datagram. The sequence lets the receiver drop
/// datagrams that arrive out of order (UDP may reorder) so a stale absolute
/// position never overwrites a newer one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MotionDatagram {
    pub seq: u64,
    pub motion: Motion,
}

impl MotionDatagram {
    pub fn encode(&self) -> [u8; MOTION_DATAGRAM_LEN] {
        let mut buf = [0u8; MOTION_DATAGRAM_LEN];
        buf[0..8].copy_from_slice(&self.seq.to_le_bytes());
        let (tag, a, b) = match self.motion {
            Motion::SetPosition { x, y } => (TAG_SET_POSITION, x, y),
            Motion::Move { dx, dy } => (TAG_MOVE, dx, dy),
        };
        buf[8] = tag;
        buf[9..13].copy_from_slice(&a.to_le_bytes());
        buf[13..17].copy_from_slice(&b.to_le_bytes());
        buf
    }

    /// Decode a datagram, returning `None` for a wrong length or unknown tag
    /// (a corrupt or foreign UDP packet is dropped, never trusted).
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != MOTION_DATAGRAM_LEN {
            return None;
        }
        let seq = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
        let a = i32::from_le_bytes(bytes[9..13].try_into().ok()?);
        let b = i32::from_le_bytes(bytes[13..17].try_into().ok()?);
        let motion = match bytes[8] {
            TAG_SET_POSITION => Motion::SetPosition { x: a, y: b },
            TAG_MOVE => Motion::Move { dx: a, dy: b },
            _ => return None,
        };
        Some(Self { seq, motion })
    }
}

/// Monotonic sequence source for the sender.
#[derive(Debug, Default)]
pub struct SeqSource {
    next: u64,
}

impl SeqSource {
    pub fn new() -> Self {
        Self { next: 0 }
    }
    pub fn next(&mut self) -> u64 {
        let seq = self.next;
        self.next = self.next.wrapping_add(1);
        seq
    }
}

/// Receiver-side gate that accepts only datagrams newer than the last accepted
/// one, so reordered/duplicate UDP packets can't apply a stale position.
#[derive(Debug, Default)]
pub struct SeqGate {
    last: Option<u64>,
}

impl SeqGate {
    pub fn new() -> Self {
        Self { last: None }
    }

    /// Whether `seq` should be accepted (strictly newer than the last accepted),
    /// updating the high-water mark when it is. The first datagram is always
    /// accepted.
    pub fn accept(&mut self, seq: u64) -> bool {
        match self.last {
            Some(last) if seq <= last => false,
            _ => {
                self.last = Some(seq);
                true
            }
        }
    }
}

/// Whether `addr` is a loopback/tailnet-plausible peer address. Motion is only
/// applied from the address that completed the TCP handshake; the caller passes
/// the expected peer and this rejects datagrams from anyone else (a UDP port is
/// unauthenticated on its own, so it inherits the TCP session's trust).
pub fn is_expected_peer(from: SocketAddr, expected_ip: std::net::IpAddr) -> bool {
    from.ip() == expected_ip
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn set_position_roundtrips() {
        let d = MotionDatagram {
            seq: 42,
            motion: Motion::SetPosition { x: -1920, y: 1080 },
        };
        assert_eq!(MotionDatagram::decode(&d.encode()), Some(d));
    }

    #[test]
    fn move_roundtrips() {
        let d = MotionDatagram {
            seq: u64::MAX,
            motion: Motion::Move { dx: 7, dy: -3 },
        };
        assert_eq!(MotionDatagram::decode(&d.encode()), Some(d));
    }

    #[test]
    fn decode_rejects_wrong_length() {
        assert_eq!(MotionDatagram::decode(&[0u8; 4]), None);
        assert_eq!(MotionDatagram::decode(&[0u8; MOTION_DATAGRAM_LEN + 1]), None);
        assert_eq!(MotionDatagram::decode(&[]), None);
    }

    #[test]
    fn decode_rejects_unknown_tag() {
        let mut bytes = MotionDatagram {
            seq: 1,
            motion: Motion::Move { dx: 1, dy: 1 },
        }
        .encode();
        bytes[8] = 0xFF; // unknown tag
        assert_eq!(MotionDatagram::decode(&bytes), None);
    }

    #[test]
    fn seq_source_is_monotonic() {
        let mut src = SeqSource::new();
        assert_eq!(src.next(), 0);
        assert_eq!(src.next(), 1);
        assert_eq!(src.next(), 2);
    }

    #[test]
    fn seq_gate_accepts_newer_and_drops_stale_and_duplicate() {
        let mut gate = SeqGate::new();
        assert!(gate.accept(5), "first datagram is accepted");
        assert!(gate.accept(6), "newer is accepted");
        assert!(!gate.accept(6), "a duplicate is dropped");
        assert!(!gate.accept(4), "an older (reordered) datagram is dropped");
        assert!(gate.accept(7), "a newer one after a reorder is still accepted");
    }

    #[test]
    fn expected_peer_matches_ip_only() {
        let ip = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 5));
        assert!(is_expected_peer(SocketAddr::new(ip, 47111), ip));
        assert!(is_expected_peer(SocketAddr::new(ip, 60000), ip)); // any source port
        let other = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 6));
        assert!(!is_expected_peer(SocketAddr::new(other, 47111), ip));
    }
}
