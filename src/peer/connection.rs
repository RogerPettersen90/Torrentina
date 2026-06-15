//! A live connection to a single peer.
//!
//! [`PeerConnection`] performs the TCP connect + handshake, then wraps the
//! stream in a `Framed<TcpStream, PeerCodec>` so callers work purely in terms
//! of [`Message`] values via [`send`](PeerConnection::send) and
//! [`recv`](PeerConnection::recv).

use std::net::SocketAddr;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::codec::Framed;

use crate::error::{Error, Result};
use crate::metainfo::InfoHash;
use crate::peer::handshake::{Handshake, HANDSHAKE_LEN};
use crate::peer::message::{Message, PeerCodec};
use crate::peer::PeerId;

/// An established, post-handshake connection to one peer.
pub struct PeerConnection {
    /// The remote peer's ID, as reported in its handshake.
    pub remote_peer_id: PeerId,
    /// The remote address we're connected to.
    pub addr: SocketAddr,
    framed: Framed<TcpStream, PeerCodec>,
}

impl PeerConnection {
    /// Dial `addr`, perform the handshake for `info_hash` as `peer_id`, and
    /// return a ready-to-message connection.
    pub async fn connect(
        addr: SocketAddr,
        info_hash: InfoHash,
        peer_id: PeerId,
    ) -> Result<Self> {
        let mut stream = TcpStream::connect(addr).await?;
        let remote = handshake(&mut stream, info_hash, peer_id).await?;
        Ok(PeerConnection {
            remote_peer_id: remote.peer_id,
            addr,
            framed: Framed::new(stream, PeerCodec::new()),
        })
    }

    /// Send one message to the peer.
    pub async fn send(&mut self, msg: Message) -> Result<()> {
        self.framed.send(msg).await
    }

    /// Receive the next message, or `None` if the peer closed the connection.
    pub async fn recv(&mut self) -> Result<Option<Message>> {
        self.framed.next().await.transpose()
    }
}

/// Exchange handshakes over an established stream, returning the peer's.
///
/// We send ours first, read the peer's 68 bytes, parse it, and verify it
/// claims the same torrent. Standalone (not a method) so it can be reused for
/// inbound connections later.
pub async fn handshake(
    stream: &mut TcpStream,
    info_hash: InfoHash,
    peer_id: PeerId,
) -> Result<Handshake> {
    let ours = Handshake::new(info_hash, peer_id);
    stream.write_all(&ours.to_bytes()).await?;

    let mut buf = [0u8; HANDSHAKE_LEN];
    stream.read_exact(&mut buf).await?;
    let remote = Handshake::from_bytes(&buf)?;

    if remote.info_hash != info_hash {
        return Err(Error::PeerProtocol(format!(
            "peer info hash {} does not match ours {}",
            remote.info_hash, info_hash
        )));
    }
    Ok(remote)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::message::Block;
    use bytes::Bytes;
    use tokio::net::TcpListener;

    /// Full loopback test: spin up a listener that acts as a peer, then drive a
    /// real `PeerConnection` through handshake + a message exchange over TCP.
    #[tokio::test]
    async fn handshake_and_message_exchange_over_tcp() {
        let info_hash = InfoHash([0x11; 20]);
        let server_peer_id = PeerId(*b"-TN0001-SERVERSERVER");
        let client_peer_id = PeerId(*b"-TN0001-CLIENTCLIENT");

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server_addr = listener.local_addr().unwrap();

        // Server task: accept, complete handshake from the other side, then
        // talk via the same Framed codec.
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let remote = handshake(&mut stream, info_hash, server_peer_id)
                .await
                .unwrap();
            assert_eq!(remote.peer_id, client_peer_id);

            let mut framed = Framed::new(stream, PeerCodec::new());
            // Expect Interested, reply with Unchoke + a Piece.
            assert_eq!(framed.next().await.unwrap().unwrap(), Message::Interested);
            framed.send(Message::Unchoke).await.unwrap();
            framed
                .send(Message::Piece(Block {
                    index: 0,
                    begin: 0,
                    data: Bytes::from_static(b"hello"),
                }))
                .await
                .unwrap();
        });

        // Client side: the real public API.
        let mut conn = PeerConnection::connect(server_addr, info_hash, client_peer_id)
            .await
            .unwrap();
        assert_eq!(conn.remote_peer_id, server_peer_id);

        conn.send(Message::Interested).await.unwrap();
        assert_eq!(conn.recv().await.unwrap(), Some(Message::Unchoke));
        assert_eq!(
            conn.recv().await.unwrap(),
            Some(Message::Piece(Block {
                index: 0,
                begin: 0,
                data: Bytes::from_static(b"hello"),
            }))
        );

        server.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_mismatched_info_hash() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server_addr = listener.local_addr().unwrap();

        // Server hands back a handshake for a *different* torrent.
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = handshake(&mut stream, InfoHash([0x22; 20]), PeerId([0; 20])).await;
        });

        let result = PeerConnection::connect(server_addr, InfoHash([0x11; 20]), PeerId([0; 20])).await;
        assert!(matches!(result, Err(Error::PeerProtocol(_))));
    }
}
