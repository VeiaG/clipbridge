//! Wire messages and framing.
//!
//! A TCP stream carries length-delimited frames (u32 big-endian length prefix,
//! max [`MAX_FRAME`]). Each frame body is `crypto::Cipher::seal(postcard(Message))`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio_util::codec::LengthDelimitedCodec;
use uuid::Uuid;

/// v2: added `ClipKind::Html`. v3: `Hello::listen_port` and file transfers.
pub const PROTOCOL_VERSION: u16 = 3;
pub const MAX_FRAME: usize = 32 * 1024 * 1024;
/// Text (and, separately, its HTML) larger than this is not synced. Both together
/// stay well under [`MAX_FRAME`].
pub const MAX_TEXT_BYTES: usize = 10 * 1024 * 1024;

pub type Hash = [u8; 32];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipKind {
    Text(String),
    /// PNG-encoded image. Raw RGBA never goes over the wire.
    Image(Vec<u8>),
    /// Rich text plus its plain-text alternative.
    Html {
        html: String,
        text: String,
    },
}

impl ClipKind {
    pub fn label(&self) -> &'static str {
        match self {
            ClipKind::Text(_) => "text",
            ClipKind::Image(_) => "image",
            ClipKind::Html { .. } => "rich text",
        }
    }

    pub fn byte_len(&self) -> usize {
        match self {
            ClipKind::Text(t) => t.len(),
            ClipKind::Image(png) => png.len(),
            ClipKind::Html { html, text } => html.len() + text.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clip {
    pub clip_id: Uuid,
    /// device_id of the machine where the copy happened.
    pub origin: String,
    pub kind: ClipKind,
    /// blake3 of the clipboard content (see `clipboard::hash_raw`).
    pub hash: Hash,
}

/// One item of a file transfer. Files are sent in this order; directories and empty
/// files carry no data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Relative, `/`-separated. The receiver sanitizes it (see `transfer`).
    pub path: String,
    pub size: u64,
    pub dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    Hello {
        device_id: String,
        device_name: String,
        protocol_version: u16,
        /// Appended last so older peers still decode the version and can warn.
        listen_port: u16,
    },
    Clip(Clip),
    Ping,
    Pong,
    // A transfer runs on its own connection: the dialer answers the receiver's
    // `Hello` with `TransferHello`, then `Offer`; the receiver answers `Accept` or
    // `Decline`; then `Chunk`s, `Done`, and the receiver confirms with `Received`.
    // Either side may send `Cancel` at any point.
    TransferHello {
        device_id: String,
        device_name: String,
        protocol_version: u16,
    },
    Offer(Vec<Entry>),
    Accept,
    Decline(String),
    Chunk(Vec<u8>),
    Done,
    Received,
    Cancel(String),
}

impl Message {
    pub fn encode(&self) -> Result<Vec<u8>> {
        postcard::to_allocvec(self).context("encode message")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        postcard::from_bytes(bytes).context("decode message")
    }
}

/// Framing codec shared by both ends.
pub fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME)
        .new_codec()
}

pub fn short_hash(h: &Hash) -> String {
    h[..4].iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use tokio_util::codec::{Decoder, Encoder};

    #[test]
    fn message_roundtrip() {
        let msgs = [
            Message::Hello {
                device_id: "a".into(),
                device_name: "PC".into(),
                protocol_version: PROTOCOL_VERSION,
                listen_port: 47821,
            },
            Message::Clip(Clip {
                clip_id: Uuid::new_v4(),
                origin: "a".into(),
                kind: ClipKind::Text("привіт 👋".into()),
                hash: [7; 32],
            }),
            Message::Clip(Clip {
                clip_id: Uuid::new_v4(),
                origin: "b".into(),
                kind: ClipKind::Image(vec![1, 2, 3]),
                hash: [9; 32],
            }),
            Message::Ping,
            Message::Pong,
            Message::Offer(vec![Entry {
                path: "dir/файл.txt".into(),
                size: 5,
                dir: false,
            }]),
            Message::Chunk(vec![0, 1, 255]),
            Message::Decline("no".into()),
        ];
        for m in msgs {
            assert_eq!(Message::decode(&m.encode().unwrap()).unwrap(), m);
        }
        assert!(Message::decode(&[0xff, 0xff, 0xff]).is_err());
    }

    /// A v2 peer must still read our version number to print a useful warning.
    #[test]
    fn old_hello_layout_reads_new_hello() {
        #[derive(Deserialize)]
        enum OldMessage {
            Hello {
                #[allow(dead_code)]
                device_id: String,
                #[allow(dead_code)]
                device_name: String,
                protocol_version: u16,
            },
        }
        let new = Message::Hello {
            device_id: "a".into(),
            device_name: "PC".into(),
            protocol_version: PROTOCOL_VERSION,
            listen_port: 1,
        };
        let OldMessage::Hello {
            protocol_version, ..
        } = postcard::from_bytes(&new.encode().unwrap()).unwrap();
        assert_eq!(protocol_version, PROTOCOL_VERSION);
    }

    #[test]
    fn framing_splits_and_limits() {
        let mut codec = codec();
        let mut buf = BytesMut::new();
        codec
            .encode(bytes::Bytes::from_static(b"one"), &mut buf)
            .unwrap();
        codec
            .encode(bytes::Bytes::from_static(b"second"), &mut buf)
            .unwrap();
        // Feed a partial frame first.
        let mut partial = buf.split_to(5);
        assert!(codec.decode(&mut partial).unwrap().is_none());
        partial.unsplit(buf);
        let mut buf = partial;
        assert_eq!(&codec.decode(&mut buf).unwrap().unwrap()[..], b"one");
        assert_eq!(&codec.decode(&mut buf).unwrap().unwrap()[..], b"second");
        assert!(codec.decode(&mut buf).unwrap().is_none());

        // A length prefix above the cap is rejected instead of buffered.
        let mut huge = BytesMut::new();
        huge.extend_from_slice(&((MAX_FRAME as u32) + 1).to_be_bytes());
        assert!(codec.decode(&mut huge).is_err());
    }
}
