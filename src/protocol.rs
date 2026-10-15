//! Wire messages and framing.
//!
//! A TCP stream carries length-delimited frames (u32 big-endian length prefix,
//! max [`MAX_FRAME`]). Each frame body is `crypto::Cipher::seal(postcard(Message))`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio_util::codec::LengthDelimitedCodec;
use uuid::Uuid;

/// v2: added `ClipKind::Html`.
pub const PROTOCOL_VERSION: u16 = 2;
pub const MAX_FRAME: usize = 32 * 1024 * 1024;
/// Text larger than this is never sent nor applied.
pub const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    Hello {
        device_id: String,
        device_name: String,
        protocol_version: u16,
    },
    Clip(Clip),
    Ping,
    Pong,
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
        ];
        for m in msgs {
            assert_eq!(Message::decode(&m.encode().unwrap()).unwrap(), m);
        }
        assert!(Message::decode(&[0xff, 0xff, 0xff]).is_err());
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
