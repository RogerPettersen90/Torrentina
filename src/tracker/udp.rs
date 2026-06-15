//! UDP tracker transport (BEP 15).
//!
//! The protocol is a two-step binary exchange:
//!
//! 1. **Connect** — we send a fixed magic protocol ID and a random transaction
//!    ID; the tracker replies with a short-lived `connection_id`.
//! 2. **Announce** — we send the announce fields keyed by that `connection_id`;
//!    the tracker replies with the interval, peer counts, and a compact peer
//!    list.
//!
//! Every reply echoes our transaction ID, which we verify. Packet building and
//! parsing are pure functions so they can be unit-tested without a network.

use std::net::SocketAddr;
use std::time::Duration;

use rand::Rng;
use tokio::net::UdpSocket;
use tokio::time::timeout;

use super::{parse_compact_peers_v4, AnnounceParams, AnnounceResponse};
use crate::error::{Error, Result};

/// Magic constant every connect request starts with (BEP 15).
const PROTOCOL_ID: u64 = 0x0417_2710_1980;

const ACTION_CONNECT: u32 = 0;
const ACTION_ANNOUNCE: u32 = 1;
const ACTION_ERROR: u32 = 3;

/// Minimum sizes of the fixed reply headers, in bytes.
const CONNECT_RESPONSE_LEN: usize = 16;
const ANNOUNCE_RESPONSE_HEADER_LEN: usize = 20;

/// Per-attempt receive timeout. BEP 15 suggests `15 * 2^n`; we keep it modest
/// with a few retries so a dead tracker fails reasonably fast.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ATTEMPTS: u32 = 3;

/// Build the 16-byte connect request.
fn build_connect_request(transaction_id: u32) -> [u8; 16] {
    let mut buf = [0u8; 16];
    buf[0..8].copy_from_slice(&PROTOCOL_ID.to_be_bytes());
    buf[8..12].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
    buf[12..16].copy_from_slice(&transaction_id.to_be_bytes());
    buf
}

/// Validate a connect reply and extract the `connection_id`.
fn parse_connect_response(buf: &[u8], expected_txn: u32) -> Result<u64> {
    if buf.len() < CONNECT_RESPONSE_LEN {
        return Err(Error::UdpProtocol(format!(
            "connect response too short: {} bytes",
            buf.len()
        )));
    }
    let action = u32::from_be_bytes(buf[0..4].try_into().unwrap());
    let txn = u32::from_be_bytes(buf[4..8].try_into().unwrap());
    if txn != expected_txn {
        return Err(Error::UdpProtocol("connect transaction id mismatch".into()));
    }
    if action != ACTION_CONNECT {
        return Err(Error::UdpProtocol(format!(
            "expected connect action, got {action}"
        )));
    }
    Ok(u64::from_be_bytes(buf[8..16].try_into().unwrap()))
}

/// Build the 98-byte announce request.
fn build_announce_request(
    connection_id: u64,
    transaction_id: u32,
    key: u32,
    params: &AnnounceParams,
) -> [u8; 98] {
    let mut buf = [0u8; 98];
    buf[0..8].copy_from_slice(&connection_id.to_be_bytes());
    buf[8..12].copy_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
    buf[12..16].copy_from_slice(&transaction_id.to_be_bytes());
    buf[16..36].copy_from_slice(params.info_hash.as_bytes());
    buf[36..56].copy_from_slice(params.peer_id.as_bytes());
    buf[56..64].copy_from_slice(&params.downloaded.to_be_bytes());
    buf[64..72].copy_from_slice(&params.left.to_be_bytes());
    buf[72..80].copy_from_slice(&params.uploaded.to_be_bytes());
    buf[80..84].copy_from_slice(&params.event.as_udp_code().to_be_bytes());
    // bytes 84..88: IP address — 0 means "use the source address".
    buf[88..92].copy_from_slice(&key.to_be_bytes());
    // num_want: -1 (as i32) lets the tracker choose the default.
    let num_want = params.numwant.map(|n| n as i32).unwrap_or(-1);
    buf[92..96].copy_from_slice(&num_want.to_be_bytes());
    buf[96..98].copy_from_slice(&params.port.to_be_bytes());
    buf
}

/// Validate an announce reply and parse it into an [`AnnounceResponse`].
fn parse_announce_response(buf: &[u8], expected_txn: u32) -> Result<AnnounceResponse> {
    if buf.len() < 8 {
        return Err(Error::UdpProtocol(format!(
            "announce response too short: {} bytes",
            buf.len()
        )));
    }
    let action = u32::from_be_bytes(buf[0..4].try_into().unwrap());
    let txn = u32::from_be_bytes(buf[4..8].try_into().unwrap());
    if txn != expected_txn {
        return Err(Error::UdpProtocol("announce transaction id mismatch".into()));
    }
    if action == ACTION_ERROR {
        let msg = String::from_utf8_lossy(&buf[8..]).into_owned();
        return Err(Error::TrackerFailure(msg));
    }
    if action != ACTION_ANNOUNCE {
        return Err(Error::UdpProtocol(format!(
            "expected announce action, got {action}"
        )));
    }
    if buf.len() < ANNOUNCE_RESPONSE_HEADER_LEN {
        return Err(Error::UdpProtocol(
            "announce response missing header fields".into(),
        ));
    }

    let interval = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as u64;
    let leechers = u32::from_be_bytes(buf[12..16].try_into().unwrap());
    let seeders = u32::from_be_bytes(buf[16..20].try_into().unwrap());
    let peers = parse_compact_peers_v4(&buf[ANNOUNCE_RESPONSE_HEADER_LEN..])?;

    Ok(AnnounceResponse {
        interval,
        peers,
        seeders: Some(seeders),
        leechers: Some(leechers),
    })
}

/// Send `request` and await a reply, retrying on timeout.
async fn exchange(sock: &UdpSocket, request: &[u8]) -> Result<Vec<u8>> {
    let mut recv_buf = vec![0u8; 2048];
    for _ in 0..MAX_ATTEMPTS {
        sock.send(request).await?;
        match timeout(ATTEMPT_TIMEOUT, sock.recv(&mut recv_buf)).await {
            Ok(Ok(n)) => return Ok(recv_buf[..n].to_vec()),
            Ok(Err(e)) => return Err(Error::Io(e)),
            Err(_elapsed) => continue, // timed out; retry
        }
    }
    Err(Error::Timeout)
}

/// Perform a full UDP announce against the host/port in `url`.
pub async fn announce(url: &url::Url, params: &AnnounceParams) -> Result<AnnounceResponse> {
    let host = url
        .host_str()
        .ok_or_else(|| Error::UdpProtocol("udp tracker URL has no host".into()))?;
    let port = url
        .port()
        .ok_or_else(|| Error::UdpProtocol("udp tracker URL has no port".into()))?;

    // Resolve the tracker's address and bind a local ephemeral UDP socket.
    let server: SocketAddr = tokio::net::lookup_host((host, port))
        .await?
        .next()
        .ok_or_else(|| Error::UdpProtocol(format!("could not resolve {host}:{port}")))?;
    let bind_addr = if server.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" };
    let sock = UdpSocket::bind(bind_addr).await?;
    sock.connect(server).await?;

    // Draw all randomness up front and drop the (non-`Send`) `ThreadRng`
    // before any `.await`, so the returned future stays `Send` and can be
    // spawned onto a multithreaded runtime.
    let (connect_txn, announce_txn, key): (u32, u32, u32) = {
        let mut rng = rand::thread_rng();
        (rng.gen(), rng.gen(), rng.gen())
    };

    // Step 1: connect.
    let connect_reply = exchange(&sock, &build_connect_request(connect_txn)).await?;
    let connection_id = parse_connect_response(&connect_reply, connect_txn)?;

    // Step 2: announce.
    let request = build_announce_request(connection_id, announce_txn, key, params);
    let announce_reply = exchange(&sock, &request).await?;
    parse_announce_response(&announce_reply, announce_txn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metainfo::InfoHash;
    use crate::peer::PeerId;
    use crate::tracker::Event;
    use std::net::Ipv4Addr;

    fn sample_params() -> AnnounceParams {
        AnnounceParams {
            info_hash: InfoHash([7u8; 20]),
            peer_id: PeerId([9u8; 20]),
            port: 6881,
            uploaded: 100,
            downloaded: 200,
            left: 300,
            event: Event::Started,
            compact: true,
            numwant: Some(80),
        }
    }

    #[test]
    fn connect_request_has_magic_and_action() {
        let req = build_connect_request(0xdead_beef);
        assert_eq!(u64::from_be_bytes(req[0..8].try_into().unwrap()), PROTOCOL_ID);
        assert_eq!(u32::from_be_bytes(req[8..12].try_into().unwrap()), ACTION_CONNECT);
        assert_eq!(u32::from_be_bytes(req[12..16].try_into().unwrap()), 0xdead_beef);
    }

    #[test]
    fn parses_connect_response_and_rejects_bad_txn() {
        let mut reply = [0u8; 16];
        reply[0..4].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
        reply[4..8].copy_from_slice(&0x1234u32.to_be_bytes());
        reply[8..16].copy_from_slice(&0xCAFEBABE_DEADBEEFu64.to_be_bytes());

        assert_eq!(
            parse_connect_response(&reply, 0x1234).unwrap(),
            0xCAFEBABE_DEADBEEF
        );
        assert!(matches!(
            parse_connect_response(&reply, 0x9999),
            Err(Error::UdpProtocol(_))
        ));
    }

    #[test]
    fn announce_request_layout_is_correct() {
        let p = sample_params();
        let req = build_announce_request(0xABCD, 0x1111, 0x2222, &p);
        assert_eq!(req.len(), 98);
        assert_eq!(u64::from_be_bytes(req[0..8].try_into().unwrap()), 0xABCD);
        assert_eq!(u32::from_be_bytes(req[8..12].try_into().unwrap()), ACTION_ANNOUNCE);
        assert_eq!(u32::from_be_bytes(req[12..16].try_into().unwrap()), 0x1111);
        assert_eq!(&req[16..36], p.info_hash.as_bytes());
        assert_eq!(&req[36..56], p.peer_id.as_bytes());
        assert_eq!(u64::from_be_bytes(req[56..64].try_into().unwrap()), 200); // downloaded
        assert_eq!(u64::from_be_bytes(req[64..72].try_into().unwrap()), 300); // left
        assert_eq!(u64::from_be_bytes(req[72..80].try_into().unwrap()), 100); // uploaded
        assert_eq!(u32::from_be_bytes(req[80..84].try_into().unwrap()), 2); // event=started
        assert_eq!(i32::from_be_bytes(req[92..96].try_into().unwrap()), 80); // num_want
        assert_eq!(u16::from_be_bytes(req[96..98].try_into().unwrap()), 6881); // port
    }

    #[test]
    fn num_want_defaults_to_minus_one() {
        let mut p = sample_params();
        p.numwant = None;
        let req = build_announce_request(1, 2, 3, &p);
        assert_eq!(i32::from_be_bytes(req[92..96].try_into().unwrap()), -1);
    }

    #[test]
    fn parses_announce_response_with_peers() {
        let mut reply = Vec::new();
        reply.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
        reply.extend_from_slice(&0x55u32.to_be_bytes()); // txn
        reply.extend_from_slice(&1800u32.to_be_bytes()); // interval
        reply.extend_from_slice(&3u32.to_be_bytes()); // leechers
        reply.extend_from_slice(&5u32.to_be_bytes()); // seeders
        reply.extend_from_slice(&[1, 2, 3, 4, 0x1a, 0xe1]); // 1.2.3.4:6881

        let resp = parse_announce_response(&reply, 0x55).unwrap();
        assert_eq!(resp.interval, 1800);
        assert_eq!(resp.seeders, Some(5));
        assert_eq!(resp.leechers, Some(3));
        assert_eq!(
            resp.peers,
            vec![SocketAddr::from((Ipv4Addr::new(1, 2, 3, 4), 6881))]
        );
    }

    #[test]
    fn surfaces_udp_error_action() {
        let mut reply = Vec::new();
        reply.extend_from_slice(&ACTION_ERROR.to_be_bytes());
        reply.extend_from_slice(&0x55u32.to_be_bytes());
        reply.extend_from_slice(b"bad request");

        let err = parse_announce_response(&reply, 0x55).unwrap_err();
        assert!(matches!(err, Error::TrackerFailure(m) if m == "bad request"));
    }
}
