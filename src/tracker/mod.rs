//! Module 2: Tracker communication.
//!
//! A tracker tells us which peers are sharing a torrent. We support both
//! transports a `.torrent` may reference:
//!
//! * `http://` / `https://` — bencoded request/response ([`http`]).
//! * `udp://` — binary connect+announce protocol, BEP 15 ([`udp`]).
//!
//! Callers use [`announce`], which dispatches on the URL scheme, and receive a
//! transport-agnostic [`AnnounceResponse`].

mod http;
mod udp;

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use serde_bencode::value::Value;

use crate::error::{Error, Result};
use crate::metainfo::InfoHash;
use crate::peer::PeerId;

/// Width in bytes of one compact IPv4 peer entry: 4 (addr) + 2 (port).
const COMPACT_PEER_V4_LEN: usize = 6;
/// Width in bytes of one compact IPv6 peer entry: 16 (addr) + 2 (port).
const COMPACT_PEER_V6_LEN: usize = 18;

/// The announce "event", reported to the tracker to mark download lifecycle
/// transitions. `None` means a periodic re-announce (no special event).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// First announce for this torrent in this session.
    Started,
    /// Graceful shutdown / leaving the swarm.
    Stopped,
    /// We just finished downloading (sent once).
    Completed,
    /// A routine periodic re-announce.
    None,
}

impl Event {
    /// The string form used in HTTP query strings (empty = omit the param).
    fn as_http_str(self) -> &'static str {
        match self {
            Event::Started => "started",
            Event::Stopped => "stopped",
            Event::Completed => "completed",
            Event::None => "",
        }
    }

    /// The numeric code used in the UDP announce packet (BEP 15).
    fn as_udp_code(self) -> u32 {
        match self {
            Event::None => 0,
            Event::Completed => 1,
            Event::Started => 2,
            Event::Stopped => 3,
        }
    }
}

/// Everything needed to issue an announce, independent of transport.
#[derive(Debug, Clone)]
pub struct AnnounceParams {
    /// Identifies the torrent.
    pub info_hash: InfoHash,
    /// Identifies us in the swarm.
    pub peer_id: PeerId,
    /// TCP port we listen on for incoming peer connections.
    pub port: u16,
    /// Total bytes uploaded so far this session.
    pub uploaded: u64,
    /// Total bytes downloaded so far this session.
    pub downloaded: u64,
    /// Bytes still needed to complete the torrent.
    pub left: u64,
    /// Lifecycle event for this announce.
    pub event: Event,
    /// Request the compact peer list (always preferred; we parse both forms).
    pub compact: bool,
    /// How many peers to request; `None` lets the tracker decide.
    pub numwant: Option<u32>,
}

/// A transport-agnostic parsed tracker response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnounceResponse {
    /// Seconds the client should wait before re-announcing.
    pub interval: u64,
    /// Peer socket addresses to attempt connections to.
    pub peers: Vec<SocketAddr>,
    /// Number of seeders, if the tracker reported it.
    pub seeders: Option<u32>,
    /// Number of leechers, if the tracker reported it.
    pub leechers: Option<u32>,
}

/// Announce to a tracker, dispatching on the URL scheme.
pub async fn announce(url: &str, params: &AnnounceParams) -> Result<AnnounceResponse> {
    let parsed = url::Url::parse(url)?;
    match parsed.scheme() {
        "http" | "https" => http::announce(url, params).await,
        "udp" => udp::announce(&parsed, params).await,
        other => Err(Error::UnsupportedScheme(other.to_string())),
    }
}

/// Parse a flat compact peer blob (`n * COMPACT_PEER_V4_LEN` bytes).
fn parse_compact_peers_v4(bytes: &[u8]) -> Result<Vec<SocketAddr>> {
    if !bytes.len().is_multiple_of(COMPACT_PEER_V4_LEN) {
        return Err(Error::InvalidPeersData(bytes.len(), COMPACT_PEER_V4_LEN));
    }
    Ok(bytes
        .chunks_exact(COMPACT_PEER_V4_LEN)
        .map(|c| {
            let ip = Ipv4Addr::new(c[0], c[1], c[2], c[3]);
            let port = u16::from_be_bytes([c[4], c[5]]);
            SocketAddr::from((ip, port))
        })
        .collect())
}

/// Parse a flat compact IPv6 peer blob (`n * COMPACT_PEER_V6_LEN` bytes).
fn parse_compact_peers_v6(bytes: &[u8]) -> Result<Vec<SocketAddr>> {
    if !bytes.len().is_multiple_of(COMPACT_PEER_V6_LEN) {
        return Err(Error::InvalidPeersData(bytes.len(), COMPACT_PEER_V6_LEN));
    }
    Ok(bytes
        .chunks_exact(COMPACT_PEER_V6_LEN)
        .map(|c| {
            let mut addr = [0u8; 16];
            addr.copy_from_slice(&c[..16]);
            let ip = Ipv6Addr::from(addr);
            let port = u16::from_be_bytes([c[16], c[17]]);
            SocketAddr::from((ip, port))
        })
        .collect())
}

/// Parse the non-compact (dictionary-model) peer list: a list of dicts each
/// with `ip` (a string) and `port` (an integer).
fn parse_peer_dicts(list: &[Value]) -> Result<Vec<SocketAddr>> {
    let mut peers = Vec::with_capacity(list.len());
    for entry in list {
        let Value::Dict(d) = entry else { continue };
        let ip = match d.get(b"ip".as_slice()) {
            Some(Value::Bytes(b)) => String::from_utf8_lossy(b).into_owned(),
            _ => continue,
        };
        let port = match d.get(b"port".as_slice()) {
            Some(Value::Int(p)) => *p as u16,
            _ => continue,
        };
        // `ip` may be a hostname or a literal address; only keep parseable IPs.
        if let Ok(addr) = ip.parse::<std::net::IpAddr>() {
            peers.push(SocketAddr::new(addr, port));
        }
    }
    Ok(peers)
}

/// Look up an integer field in a bencode dictionary.
fn dict_int(dict: &HashMap<Vec<u8>, Value>, key: &str) -> Option<i64> {
    match dict.get(key.as_bytes()) {
        Some(Value::Int(v)) => Some(*v),
        _ => None,
    }
}

/// Parse a bencoded HTTP tracker response body into an [`AnnounceResponse`].
///
/// Lives in the parent module (not [`http`]) because it's pure logic over
/// bencode and is convenient to unit-test here.
fn parse_http_response(body: &[u8]) -> Result<AnnounceResponse> {
    let value: Value = serde_bencode::from_bytes(body)?;
    let Value::Dict(dict) = value else {
        return Err(Error::TrackerFailure(
            "response was not a bencode dictionary".into(),
        ));
    };

    if let Some(Value::Bytes(reason)) = dict.get(b"failure reason".as_slice()) {
        return Err(Error::TrackerFailure(
            String::from_utf8_lossy(reason).into_owned(),
        ));
    }

    let interval = dict_int(&dict, "interval").unwrap_or(0).max(0) as u64;
    let seeders = dict_int(&dict, "complete").map(|v| v.max(0) as u32);
    let leechers = dict_int(&dict, "incomplete").map(|v| v.max(0) as u32);

    let mut peers = Vec::new();
    match dict.get(b"peers".as_slice()) {
        Some(Value::Bytes(b)) => peers.extend(parse_compact_peers_v4(b)?),
        Some(Value::List(l)) => peers.extend(parse_peer_dicts(l)?),
        _ => {}
    }
    if let Some(Value::Bytes(b)) = dict.get(b"peers6".as_slice()) {
        peers.extend(parse_compact_peers_v6(b)?);
    }

    Ok(AnnounceResponse {
        interval,
        peers,
        seeders,
        leechers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ipv4(a: u8, b: u8, c: u8, d: u8, port: u16) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::new(a, b, c, d), port))
    }

    #[test]
    fn parses_compact_v4_peers() {
        // 1.2.3.4:6881 and 10.0.0.1:6882
        let bytes = [1, 2, 3, 4, 0x1a, 0xe1, 10, 0, 0, 1, 0x1a, 0xe2];
        let peers = parse_compact_peers_v4(&bytes).unwrap();
        assert_eq!(peers, vec![ipv4(1, 2, 3, 4, 6881), ipv4(10, 0, 0, 1, 6882)]);
    }

    #[test]
    fn rejects_misaligned_compact_peers() {
        let bytes = [1, 2, 3, 4, 5]; // 5 bytes, not a multiple of 6
        assert!(matches!(
            parse_compact_peers_v4(&bytes),
            Err(Error::InvalidPeersData(5, COMPACT_PEER_V4_LEN))
        ));
    }

    #[test]
    fn parses_full_compact_http_response() {
        // d8:intervali1800e8:completei5e10:incompletei3e5:peers6:....e
        let mut body = Vec::new();
        body.extend_from_slice(b"d8:completei5e10:incompletei3e8:intervali1800e5:peers6:");
        body.extend_from_slice(&[1, 2, 3, 4, 0x1a, 0xe1]); // 1.2.3.4:6881
        body.extend_from_slice(b"e");

        let resp = parse_http_response(&body).unwrap();
        assert_eq!(resp.interval, 1800);
        assert_eq!(resp.seeders, Some(5));
        assert_eq!(resp.leechers, Some(3));
        assert_eq!(resp.peers, vec![ipv4(1, 2, 3, 4, 6881)]);
    }

    #[test]
    fn parses_dictionary_model_peers() {
        // peers as a list of {ip, port} dicts
        let body = b"d8:intervali900e5:peersld2:ip9:127.0.0.14:porti6881eeee";
        let resp = parse_http_response(body).unwrap();
        assert_eq!(resp.interval, 900);
        assert_eq!(resp.peers, vec![ipv4(127, 0, 0, 1, 6881)]);
    }

    #[test]
    fn surfaces_tracker_failure_reason() {
        let body = b"d14:failure reason17:torrent not founde";
        let err = parse_http_response(body).unwrap_err();
        assert!(matches!(err, Error::TrackerFailure(reason) if reason == "torrent not found"));
    }
}
