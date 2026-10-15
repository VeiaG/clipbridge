//! Clipboard access.
//!
//! The OS clipboard sits behind [`ClipboardBackend`] so the sync core can be tested with
//! [`mock::MockClipboard`]. The backend is owned by a dedicated thread (an actor);
//! async code talks to it through the cloneable [`ClipboardHandle`]. PNG encoding and
//! decoding also happen on that thread so they never block the async runtime.
//!
//! Change notifications are a `watch::Receiver<u64>` counter bumped on every change.

use std::io::Cursor;
use std::sync::mpsc as std_mpsc;

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::{oneshot, watch};

use crate::protocol::{ClipKind, Hash};

/// Clipboard content as the OS sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawClip {
    Text(String),
    /// Rich text: HTML plus the plain-text alternative apps put next to it.
    Html {
        html: String,
        text: String,
    },
    Image {
        width: usize,
        height: usize,
        rgba: Vec<u8>,
    },
}

/// Which optional formats to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Want {
    pub images: bool,
    pub html: bool,
}

impl Want {
    pub const ALL: Want = Want {
        images: true,
        html: true,
    };
}

pub trait ClipboardBackend: Send + 'static {
    /// Current content. Text (with its HTML, if any) wins over an image.
    /// `Ok(None)` when the clipboard is empty or holds an unsupported format.
    fn read(&mut self, want: Want) -> Result<Option<RawClip>>;
    fn write(&mut self, clip: &RawClip) -> Result<()>;
}

/// Content hash. Images are hashed over their pixels (not PNG bytes), so the same
/// picture hashes the same no matter who encoded it.
pub fn hash_raw(raw: &RawClip) -> Hash {
    let mut h = blake3::Hasher::new();
    match raw {
        RawClip::Text(t) => {
            h.update(b"text\0");
            h.update(t.as_bytes());
        }
        RawClip::Html { html, text } => {
            h.update(b"html\0");
            h.update(&(text.len() as u64).to_le_bytes());
            h.update(text.as_bytes());
            h.update(html.as_bytes());
        }
        RawClip::Image {
            width,
            height,
            rgba,
        } => {
            h.update(b"image\0");
            h.update(&(*width as u64).to_le_bytes());
            h.update(&(*height as u64).to_le_bytes());
            h.update(rgba);
        }
    }
    *h.finalize().as_bytes()
}

// ---------------------------------------------------------------------------------
// PNG

const MAX_DIMENSION: u32 = 16384;

pub fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>> {
    use image::ImageEncoder;
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};

    if width == 0 || height == 0 || rgba.len() != width * height * 4 {
        bail!("bad RGBA buffer: {width}x{height}, {} bytes", rgba.len());
    }
    let (w, h) = (u32::try_from(width)?, u32::try_from(height)?);
    let mut out = Vec::new();
    PngEncoder::new_with_quality(&mut out, CompressionType::Fast, FilterType::Adaptive)
        .write_image(rgba, w, h, image::ExtendedColorType::Rgba8)
        .context("PNG encode")?;
    Ok(out)
}

/// Decodes a PNG into RGBA8, refusing absurd dimensions (decompression bombs).
pub fn decode_png(png: &[u8]) -> Result<(usize, usize, Vec<u8>)> {
    let mut reader = image::ImageReader::with_format(Cursor::new(png), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(1024 * 1024 * 1024);
    reader.limits(limits);
    let img = reader.decode().context("PNG decode")?.into_rgba8();
    let (w, h) = (img.width() as usize, img.height() as usize);
    Ok((w, h, img.into_raw()))
}

// ---------------------------------------------------------------------------------
// Actor thread

/// What the sync core gets back from a read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutcome {
    Empty,
    /// Content hash equals the `known` hash passed in (skips PNG encoding).
    Unchanged,
    Clip {
        kind: ClipKind,
        hash: Hash,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct ReadOpts {
    pub want: Want,
    pub known: Option<Hash>,
}

enum Request {
    Read(ReadOpts, oneshot::Sender<Result<ReadOutcome>>),
    Write(ClipKind, oneshot::Sender<Result<Hash>>),
}

#[derive(Clone)]
pub struct ClipboardHandle {
    tx: std_mpsc::Sender<Request>,
}

impl ClipboardHandle {
    /// Moves `backend` onto its own thread. The thread exits when all handles are dropped.
    pub fn spawn(backend: Box<dyn ClipboardBackend>) -> Result<Self> {
        let (tx, rx) = std_mpsc::channel();
        std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || clipboard_thread(backend, rx))
            .context("spawn clipboard thread")?;
        Ok(Self { tx })
    }

    pub async fn read(&self, opts: ReadOpts) -> Result<ReadOutcome> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Request::Read(opts, tx))
            .map_err(|_| anyhow!("clipboard thread gone"))?;
        rx.await.map_err(|_| anyhow!("clipboard thread gone"))?
    }

    /// Writes the content and returns the hash of what the clipboard now holds.
    pub async fn write(&self, kind: ClipKind) -> Result<Hash> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Request::Write(kind, tx))
            .map_err(|_| anyhow!("clipboard thread gone"))?;
        rx.await.map_err(|_| anyhow!("clipboard thread gone"))?
    }
}

fn clipboard_thread(mut backend: Box<dyn ClipboardBackend>, rx: std_mpsc::Receiver<Request>) {
    while let Ok(req) = rx.recv() {
        match req {
            Request::Read(opts, reply) => {
                let _ = reply.send(do_read(backend.as_mut(), opts));
            }
            Request::Write(kind, reply) => {
                let _ = reply.send(do_write(backend.as_mut(), kind));
            }
        }
    }
}

fn do_read(backend: &mut dyn ClipboardBackend, opts: ReadOpts) -> Result<ReadOutcome> {
    let Some(raw) = backend.read(opts.want)? else {
        return Ok(ReadOutcome::Empty);
    };
    let hash = hash_raw(&raw);
    if opts.known == Some(hash) {
        return Ok(ReadOutcome::Unchanged);
    }
    let kind = match raw {
        RawClip::Text(t) => ClipKind::Text(t),
        RawClip::Html { html, text } => ClipKind::Html { html, text },
        RawClip::Image {
            width,
            height,
            rgba,
        } => ClipKind::Image(encode_png(width, height, &rgba)?),
    };
    Ok(ReadOutcome::Clip { kind, hash })
}

fn do_write(backend: &mut dyn ClipboardBackend, kind: ClipKind) -> Result<Hash> {
    let raw = match kind {
        ClipKind::Text(t) => RawClip::Text(t),
        ClipKind::Html { html, text } => RawClip::Html { html, text },
        ClipKind::Image(png) => {
            let (width, height, rgba) = decode_png(&png)?;
            RawClip::Image {
                width,
                height,
                rgba,
            }
        }
    };
    backend.write(&raw)?;
    // The OS may not round-trip images and HTML exactly (premultiplied alpha, DIB
    // conversion, CF_HTML wrapping); report what the clipboard really holds so the
    // echo of our own write is recognized.
    if !matches!(raw, RawClip::Text(_))
        && let Ok(Some(back)) = backend.read(Want::ALL)
    {
        return Ok(hash_raw(&back));
    }
    Ok(hash_raw(&raw))
}

// ---------------------------------------------------------------------------------
// Real OS clipboard

pub struct ArboardBackend {
    cb: arboard::Clipboard,
}

impl ArboardBackend {
    pub fn new() -> Result<Self> {
        Ok(Self {
            cb: arboard::Clipboard::new().context("open system clipboard")?,
        })
    }
}

/// The clipboard is briefly locked by other apps all the time (especially on Windows).
fn with_retry<T>(mut f: impl FnMut() -> Result<T, arboard::Error>) -> Result<T, arboard::Error> {
    let mut attempt = 0;
    loop {
        match f() {
            // "Unknown" errors show up when another process is mid-write; retry those too.
            Err(arboard::Error::ClipboardOccupied | arboard::Error::Unknown { .. })
                if attempt < 5 =>
            {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(40 * attempt));
            }
            other => return other,
        }
    }
}

impl ClipboardBackend for ArboardBackend {
    fn read(&mut self, want: Want) -> Result<Option<RawClip>> {
        match with_retry(|| self.cb.get_text()) {
            Ok(text) if !text.is_empty() => {
                if want.html
                    && let Ok(html) = with_retry(|| self.cb.get().html())
                    && !html.trim().is_empty()
                {
                    return Ok(Some(RawClip::Html { html, text }));
                }
                return Ok(Some(RawClip::Text(text)));
            }
            Ok(_) | Err(arboard::Error::ContentNotAvailable) => {}
            Err(e) => return Err(anyhow!("read clipboard text: {e}")),
        }
        if !want.images {
            return Ok(None);
        }
        match with_retry(|| self.cb.get_image()) {
            Ok(img) => Ok(Some(RawClip::Image {
                width: img.width,
                height: img.height,
                rgba: img.bytes.into_owned(),
            })),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(e) => Err(anyhow!("read clipboard image: {e}")),
        }
    }

    fn write(&mut self, clip: &RawClip) -> Result<()> {
        match clip {
            RawClip::Text(t) => with_retry(|| self.cb.set_text(t.as_str()))
                .map_err(|e| anyhow!("write clipboard text: {e}")),
            RawClip::Html { html, text } => {
                with_retry(|| self.cb.set().html(html.as_str(), Some(text.as_str())))
                    .map_err(|e| anyhow!("write clipboard HTML: {e}"))
            }
            RawClip::Image {
                width,
                height,
                rgba,
            } => with_retry(|| {
                self.cb.set_image(arboard::ImageData {
                    width: *width,
                    height: *height,
                    bytes: std::borrow::Cow::Borrowed(rgba),
                })
            })
            .map_err(|e| anyhow!("write clipboard image: {e}")),
        }
    }
}

/// Watches the OS clipboard (native notifications on Windows, polling on macOS)
/// and bumps the returned counter on every change.
pub fn spawn_watcher() -> Result<watch::Receiver<u64>> {
    use clipboard_master::{CallbackResult, ClipboardHandler, Master};

    struct Handler(watch::Sender<u64>);
    impl ClipboardHandler for Handler {
        fn on_clipboard_change(&mut self) -> CallbackResult {
            self.0.send_modify(|n| *n = n.wrapping_add(1));
            CallbackResult::Next
        }
        fn on_clipboard_error(&mut self, error: std::io::Error) -> CallbackResult {
            tracing::warn!("clipboard watcher error: {error}");
            CallbackResult::Next
        }
    }

    let (tx, rx) = watch::channel(0u64);
    let (ready_tx, ready_rx) = std_mpsc::channel();
    std::thread::Builder::new()
        .name("clipboard-watch".into())
        .spawn(move || match Master::new(Handler(tx)) {
            Ok(mut master) => {
                let _ = ready_tx.send(Ok(()));
                if let Err(e) = master.run() {
                    tracing::error!("clipboard watcher stopped: {e}");
                }
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        })
        .context("spawn clipboard watcher")?;
    ready_rx
        .recv()
        .context("clipboard watcher thread died")?
        .context("start clipboard watcher")?;
    Ok(rx)
}

// ---------------------------------------------------------------------------------
// Test double

pub mod mock {
    //! In-memory clipboard. Writes bump the change counter exactly like the real OS
    //! does, so tests exercise echo-loop prevention.

    use std::sync::{Arc, Mutex, MutexGuard};

    use anyhow::Result;
    use tokio::sync::watch;

    use super::{ClipboardBackend, RawClip, Want};

    #[derive(Default)]
    struct State {
        content: Option<RawClip>,
        writes: Vec<RawClip>,
    }

    #[derive(Clone)]
    pub struct MockClipboard {
        state: Arc<Mutex<State>>,
        changes: watch::Sender<u64>,
    }

    impl MockClipboard {
        pub fn new() -> (Self, watch::Receiver<u64>) {
            let (changes, rx) = watch::channel(0);
            let mock = Self {
                state: Arc::default(),
                changes,
            };
            (mock, rx)
        }

        fn lock(&self) -> MutexGuard<'_, State> {
            self.state.lock().unwrap_or_else(|e| e.into_inner())
        }

        fn bump(&self) {
            self.changes.send_modify(|n| *n = n.wrapping_add(1));
        }

        /// Simulates the user copying something on this machine.
        pub fn copy(&self, raw: RawClip) {
            self.lock().content = Some(raw);
            self.bump();
        }

        pub fn content(&self) -> Option<RawClip> {
            self.lock().content.clone()
        }

        /// Everything written by the sync engine (i.e. clips applied from peers).
        pub fn writes(&self) -> Vec<RawClip> {
            self.lock().writes.clone()
        }
    }

    impl ClipboardBackend for MockClipboard {
        fn read(&mut self, want: Want) -> Result<Option<RawClip>> {
            Ok(match self.lock().content.clone() {
                Some(RawClip::Image { .. }) if !want.images => None,
                Some(RawClip::Html { text, .. }) if !want.html => Some(RawClip::Text(text)),
                other => other,
            })
        }

        fn write(&mut self, clip: &RawClip) -> Result<()> {
            {
                let mut s = self.lock();
                s.content = Some(clip.clone());
                s.writes.push(clip.clone());
            }
            self.bump();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::MockClipboard;
    use super::*;

    fn checker(w: usize, h: usize) -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| {
                let on = (i % w + i / w).is_multiple_of(2);
                [if on { 255 } else { 0 }, 128, (i % 251) as u8, 200]
            })
            .collect()
    }

    #[test]
    fn png_roundtrip_is_lossless() {
        let rgba = checker(37, 21);
        let png = encode_png(37, 21, &rgba).unwrap();
        assert_eq!(&png[1..4], b"PNG");
        let (w, h, back) = decode_png(&png).unwrap();
        assert_eq!((w, h), (37, 21));
        assert_eq!(back, rgba);
    }

    #[test]
    fn png_rejects_bad_input() {
        assert!(encode_png(2, 2, &[0; 15]).is_err());
        assert!(encode_png(0, 0, &[]).is_err());
        assert!(decode_png(b"not a png").is_err());
    }

    #[test]
    fn hash_distinguishes_kinds_and_dims() {
        let t = hash_raw(&RawClip::Text("abc".into()));
        assert_eq!(t, hash_raw(&RawClip::Text("abc".into())));
        assert_ne!(t, hash_raw(&RawClip::Text("abd".into())));
        let a = hash_raw(&RawClip::Image {
            width: 2,
            height: 1,
            rgba: vec![0; 8],
        });
        let b = hash_raw(&RawClip::Image {
            width: 1,
            height: 2,
            rgba: vec![0; 8],
        });
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn handle_reads_and_writes_through_actor() {
        let (mock, mut changes) = MockClipboard::new();
        let handle = ClipboardHandle::spawn(Box::new(mock.clone())).unwrap();
        let opts = ReadOpts {
            want: Want::ALL,
            known: None,
        };
        assert_eq!(handle.read(opts).await.unwrap(), ReadOutcome::Empty);

        mock.copy(RawClip::Text("hi".into()));
        assert!(changes.has_changed().unwrap());
        changes.mark_unchanged();
        let ReadOutcome::Clip { kind, hash } = handle.read(opts).await.unwrap() else {
            panic!("expected clip");
        };
        assert_eq!(kind, ClipKind::Text("hi".into()));
        let known = ReadOpts {
            known: Some(hash),
            ..opts
        };
        assert_eq!(handle.read(known).await.unwrap(), ReadOutcome::Unchanged);

        let rgba = checker(4, 3);
        let png = encode_png(4, 3, &rgba).unwrap();
        let written = handle.write(ClipKind::Image(png)).await.unwrap();
        assert!(changes.has_changed().unwrap(), "writes notify like the OS");
        let expected = RawClip::Image {
            width: 4,
            height: 3,
            rgba,
        };
        assert_eq!(written, hash_raw(&expected));
        assert_eq!(mock.content(), Some(expected));

        let no_images = ReadOpts {
            want: Want {
                images: false,
                html: true,
            },
            known: None,
        };
        assert_eq!(handle.read(no_images).await.unwrap(), ReadOutcome::Empty);

        // Rich text: HTML travels with its plain-text alternative.
        let html = ClipKind::Html {
            html: "<b>bold</b>".into(),
            text: "bold".into(),
        };
        handle.write(html.clone()).await.unwrap();
        let ReadOutcome::Clip { kind, .. } = handle.read(opts).await.unwrap() else {
            panic!("expected clip");
        };
        assert_eq!(kind, html);
        let plain_only = ReadOpts {
            want: Want {
                images: true,
                html: false,
            },
            known: None,
        };
        let ReadOutcome::Clip { kind, .. } = handle.read(plain_only).await.unwrap() else {
            panic!("expected clip");
        };
        assert_eq!(kind, ClipKind::Text("bold".into()));
    }

    #[test]
    fn html_hash_covers_both_parts() {
        let a = hash_raw(&RawClip::Html {
            html: "<b>x</b>".into(),
            text: "x".into(),
        });
        let b = hash_raw(&RawClip::Html {
            html: "<i>x</i>".into(),
            text: "x".into(),
        });
        assert_ne!(a, b);
        assert_ne!(a, hash_raw(&RawClip::Text("x".into())));
    }
}

/// Touches the real OS clipboard; run manually: cargo test real_clipboard -- --ignored
#[cfg(test)]
mod real_clipboard {
    use super::*;

    #[test]
    #[ignore]
    fn real_clipboard_image_roundtrip() {
        let mut a = ArboardBackend::new().unwrap();
        let mut b = ArboardBackend::new().unwrap();
        let rgba: Vec<u8> = (0..64 * 48 * 4).map(|i| (i % 256) as u8 | 0x80).collect();
        let img = RawClip::Image {
            width: 64,
            height: 48,
            rgba,
        };
        a.write(&img).unwrap();
        for i in 0..5 {
            match b.read(Want::ALL) {
                Ok(Some(RawClip::Image { width, height, .. })) => {
                    assert_eq!((width, height), (64, 48));
                    println!("read ok on attempt {i}");
                    return;
                }
                other => println!("attempt {i}: {other:?}"),
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("could not read back image");
    }
}

#[cfg(test)]
mod real_clipboard_html {
    use super::*;

    /// Run manually: cargo test real_clipboard_html -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_clipboard_html_roundtrip() {
        let mut a = ArboardBackend::new().unwrap();
        let mut b = ArboardBackend::new().unwrap();
        let clip = RawClip::Html {
            html: "<p>Hello <b>bold</b> world</p>".into(),
            text: "Hello bold world".into(),
        };
        a.write(&clip).unwrap();
        let back = b.read(Want::ALL).unwrap();
        println!("read back: {back:?}");
        let Some(RawClip::Html { html, text }) = back else {
            panic!("expected HTML back");
        };
        assert_eq!(text, "Hello bold world");
        assert!(html.contains("<b>bold</b>"));
    }
}
