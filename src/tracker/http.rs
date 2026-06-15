//! HTTP(S) tracker transport.
//!
//! The request is a plain GET whose query string carries the announce
//! parameters. The two binary fields — `info_hash` and `peer_id` — must be
//! percent-encoded byte-by-byte; ordinary form encoders mishandle non-UTF-8
//! bytes, so we build the query string by hand.

use std::time::Duration;

use percent_encoding::{percent_encode, AsciiSet, NON_ALPHANUMERIC};

use super::{parse_http_response, AnnounceParams, AnnounceResponse};
use crate::error::Result;

/// How long to wait for the whole HTTP announce to complete.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// The URI "unreserved" set (RFC 3986): `A-Z a-z 0-9 - . _ ~`. Every other
/// byte — including all bytes of a binary hash — is percent-encoded. We start
/// from "encode everything non-alphanumeric" and carve out the unreserved
/// punctuation.
const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Percent-encode raw bytes per the tracker convention.
fn encode(bytes: &[u8]) -> String {
    percent_encode(bytes, UNRESERVED).to_string()
}

/// Build the full announce URL, appending our query to any the tracker URL
/// already carries (e.g. a passkey).
fn build_url(base: &str, params: &AnnounceParams) -> String {
    let mut q = String::new();
    q.push_str("info_hash=");
    q.push_str(&encode(params.info_hash.as_bytes()));
    q.push_str("&peer_id=");
    q.push_str(&encode(params.peer_id.as_bytes()));
    q.push_str(&format!(
        "&port={}&uploaded={}&downloaded={}&left={}&compact={}",
        params.port,
        params.uploaded,
        params.downloaded,
        params.left,
        if params.compact { 1 } else { 0 },
    ));
    let event = params.event.as_http_str();
    if !event.is_empty() {
        q.push_str("&event=");
        q.push_str(event);
    }
    if let Some(n) = params.numwant {
        q.push_str(&format!("&numwant={n}"));
    }

    let separator = if base.contains('?') { '&' } else { '?' };
    format!("{base}{separator}{q}")
}

/// Perform an HTTP(S) announce.
pub async fn announce(base_url: &str, params: &AnnounceParams) -> Result<AnnounceResponse> {
    let url = build_url(base_url, params);
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()?;
    let body = client.get(&url).send().await?.error_for_status()?.bytes().await?;
    parse_http_response(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metainfo::InfoHash;
    use crate::peer::PeerId;
    use crate::tracker::Event;

    fn sample_params() -> AnnounceParams {
        AnnounceParams {
            // Bytes chosen to include a value (0x00) that *must* be encoded and
            // an alphanumeric one ('A' = 0x41) that must *not* be.
            info_hash: InfoHash([0x00, 0x41, 0xff, 0x2d, 0x7e, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            peer_id: PeerId(*b"-TN0001-ABCDEFGHIJKL"),
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: 1024,
            event: Event::Started,
            compact: true,
            numwant: Some(50),
        }
    }

    #[test]
    fn encodes_binary_fields_per_tracker_convention() {
        // 0x00 -> %00 (encoded), 0x41 'A' -> A (unreserved), 0xff -> %FF,
        // 0x2d '-' -> - (unreserved), 0x7e '~' -> ~ (unreserved)
        assert_eq!(encode(&[0x00, 0x41, 0xff, 0x2d, 0x7e]), "%00A%FF-~");
    }

    #[test]
    fn builds_query_and_chooses_separator() {
        let p = sample_params();

        let url = build_url("http://tracker.example/announce", &p);
        assert!(url.starts_with("http://tracker.example/announce?info_hash="));
        assert!(url.contains("&peer_id=-TN0001-ABCDEFGHIJKL"));
        assert!(url.contains("&port=6881"));
        assert!(url.contains("&left=1024"));
        assert!(url.contains("&compact=1"));
        assert!(url.contains("&event=started"));
        assert!(url.contains("&numwant=50"));

        // A URL that already has a query must be extended with '&'.
        let url2 = build_url("http://tracker.example/announce?passkey=abc", &p);
        assert!(url2.contains("?passkey=abc&info_hash="));
    }

    #[test]
    fn omits_event_param_when_none() {
        let mut p = sample_params();
        p.event = Event::None;
        let url = build_url("http://t/announce", &p);
        assert!(!url.contains("event="));
    }
}
