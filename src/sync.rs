//! Sync core: decides what to send and what to apply, without echo loops.
//!
//! [`SyncCore`] is a pure state machine (no I/O) so the rules are unit-testable.
//! [`run`] is the actor that feeds it clipboard changes and remote clips.
//!
//! Loop prevention: `current` is the hash of what we believe the local clipboard holds,
//! updated both when we send a local copy and when we apply a remote clip. Writing a
//! remote clip triggers a local change notification; reading it back yields `current`,
//! so it is ignored instead of being re-broadcast. Remote clips are also de-duplicated
//! by `clip_id`, and a clip that originated here is never applied.

use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::clipboard::{ClipboardHandle, ReadOpts, ReadOutcome, Want};
use crate::protocol::{Clip, ClipKind, Hash, MAX_TEXT_BYTES, short_hash};

const SEEN_CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_text_bytes: usize,
    pub max_image_bytes: usize,
    pub sync_images: bool,
    pub sync_html: bool,
}

impl Limits {
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self {
            max_text_bytes: MAX_TEXT_BYTES,
            max_image_bytes: cfg.max_image_bytes(),
            sync_images: cfg.sync_images,
            sync_html: cfg.sync_html,
        }
    }

    /// The content to actually sync, or why it may not be synced. Rich text that is
    /// disabled or too large degrades to its plain-text alternative.
    fn fit(&self, kind: ClipKind) -> Result<ClipKind, String> {
        let text_too_large = |t: &str| {
            format!(
                "text too large ({} > {} bytes)",
                t.len(),
                self.max_text_bytes
            )
        };
        match kind {
            ClipKind::Text(t) if t.len() > self.max_text_bytes => Err(text_too_large(&t)),
            ClipKind::Html { text, .. } if text.len() > self.max_text_bytes => {
                Err(text_too_large(&text))
            }
            ClipKind::Html { html, text }
                if !self.sync_html || html.len() > self.max_text_bytes =>
            {
                Ok(ClipKind::Text(text))
            }
            ClipKind::Image(_) if !self.sync_images => Err("image sync disabled".into()),
            ClipKind::Image(png) if png.len() > self.max_image_bytes => Err(format!(
                "image too large ({} > {} bytes as PNG)",
                png.len(),
                self.max_image_bytes
            )),
            other => Ok(other),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum LocalAction {
    Send(Clip),
    Ignore(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum RemoteAction {
    Apply(ClipKind),
    Skip(String),
}

/// Bounded set of recently seen clip ids.
struct SeenIds {
    order: VecDeque<Uuid>,
    set: HashSet<Uuid>,
}

impl SeenIds {
    fn new() -> Self {
        Self {
            order: VecDeque::with_capacity(SEEN_CAPACITY),
            set: HashSet::with_capacity(SEEN_CAPACITY),
        }
    }

    /// Returns false if the id was already present.
    fn insert(&mut self, id: Uuid) -> bool {
        if !self.set.insert(id) {
            return false;
        }
        self.order.push_back(id);
        if self.order.len() > SEEN_CAPACITY
            && let Some(old) = self.order.pop_front()
        {
            self.set.remove(&old);
        }
        true
    }
}

pub struct SyncCore {
    device_id: String,
    limits: Limits,
    current: Option<Hash>,
    seen: SeenIds,
}

impl SyncCore {
    pub fn new(device_id: String, limits: Limits) -> Self {
        Self {
            device_id,
            limits,
            current: None,
            seen: SeenIds::new(),
        }
    }

    pub fn current(&self) -> Option<Hash> {
        self.current
    }

    /// Records the clipboard state at startup without sending it.
    pub fn set_current(&mut self, hash: Hash) {
        self.current = Some(hash);
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Formats worth reading from the clipboard under the current limits.
    pub fn want(&self) -> Want {
        Want {
            images: self.limits.sync_images,
            html: self.limits.sync_html,
        }
    }

    /// The local clipboard changed to `kind` (debounced, already read).
    pub fn on_local(&mut self, kind: ClipKind, hash: Hash) -> LocalAction {
        if self.current == Some(hash) {
            return LocalAction::Ignore("unchanged (own write or same content)".into());
        }
        self.current = Some(hash);
        let kind = match self.limits.fit(kind) {
            Ok(k) => k,
            Err(why) => return LocalAction::Ignore(why),
        };
        let clip = Clip {
            clip_id: Uuid::new_v4(),
            origin: self.device_id.clone(),
            kind,
            hash,
        };
        self.seen.insert(clip.clip_id);
        LocalAction::Send(clip)
    }

    /// A peer sent us a clip.
    pub fn on_remote(&mut self, clip: Clip) -> RemoteAction {
        if !self.seen.insert(clip.clip_id) {
            return RemoteAction::Skip("duplicate clip_id".into());
        }
        if clip.origin == self.device_id {
            return RemoteAction::Skip("originated here".into());
        }
        if self.current == Some(clip.hash) {
            return RemoteAction::Skip("already on clipboard".into());
        }
        match self.limits.fit(clip.kind) {
            Ok(kind) => RemoteAction::Apply(kind),
            Err(why) => RemoteAction::Skip(why),
        }
    }

    /// A remote clip was written; `written` is the hash the clipboard now reports.
    pub fn applied(&mut self, written: Hash) {
        self.current = Some(written);
    }
}

/// Counters for the status display.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SyncStats {
    pub sent: u64,
    pub applied: u64,
    pub last_event: Option<String>,
}

pub struct SyncTask {
    pub core: SyncCore,
    pub clipboard: ClipboardHandle,
    pub changes: watch::Receiver<u64>,
    pub incoming: mpsc::Receiver<Clip>,
    pub outgoing: mpsc::Sender<Clip>,
    pub paused: watch::Receiver<bool>,
    pub stats: watch::Sender<SyncStats>,
    pub debounce: Duration,
    pub cancel: CancellationToken,
}

fn describe(kind: &ClipKind) -> String {
    let n = kind.byte_len();
    let size = if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    };
    format!("{} {size}", kind.label())
}

pub async fn run(mut t: SyncTask) {
    // Learn what is on the clipboard now, so it is neither sent nor overwritten
    // by an identical remote clip.
    t.changes.mark_unchanged();
    let opts = ReadOpts {
        want: t.core.want(),
        known: None,
    };
    if let Ok(ReadOutcome::Clip { hash, .. }) = t.clipboard.read(opts).await {
        t.core.set_current(hash);
    }

    loop {
        tokio::select! {
            _ = t.cancel.cancelled() => break,
            changed = t.changes.changed() => {
                if changed.is_err() {
                    break;
                }
                // Debounce: wait until the clipboard has been quiet for `debounce`.
                loop {
                    match tokio::time::timeout(t.debounce, t.changes.changed()).await {
                        Ok(Ok(())) => continue,
                        Ok(Err(_)) => return,
                        Err(_) => break,
                    }
                }
                handle_local(&mut t).await;
            }
            Some(clip) = t.incoming.recv() => handle_remote(&mut t, clip).await,
        }
    }
}

async fn handle_local(t: &mut SyncTask) {
    if *t.paused.borrow() {
        return;
    }
    let opts = ReadOpts {
        want: t.core.want(),
        known: t.core.current(),
    };
    let (kind, hash) = match t.clipboard.read(opts).await {
        Ok(ReadOutcome::Clip { kind, hash }) => (kind, hash),
        Ok(ReadOutcome::Unchanged) => {
            tracing::debug!("clipboard change ignored: unchanged");
            return;
        }
        Ok(ReadOutcome::Empty) => {
            tracing::debug!("clipboard change ignored: empty or unsupported format");
            return;
        }
        Err(e) => {
            tracing::warn!("reading clipboard failed: {e:#}");
            return;
        }
    };
    let what = describe(&kind);
    match t.core.on_local(kind, hash) {
        LocalAction::Send(clip) => {
            tracing::info!("local copy: {what} [{}], sending", short_hash(&hash));
            if t.outgoing.send(clip).await.is_ok() {
                t.stats.send_modify(|s| {
                    s.sent += 1;
                    s.last_event = Some(format!("sent {what}"));
                });
            }
        }
        LocalAction::Ignore(why) => tracing::info!("local copy: {what} not sent: {why}"),
    }
}

async fn handle_remote(t: &mut SyncTask, clip: Clip) {
    if *t.paused.borrow() {
        tracing::debug!("paused, dropping clip from {}", clip.origin);
        return;
    }
    let what = describe(&clip.kind);
    let hash = clip.hash;
    match t.core.on_remote(clip) {
        RemoteAction::Apply(kind) => match t.clipboard.write(kind).await {
            Ok(written) => {
                t.core.applied(written);
                tracing::info!("applied remote {what} [{}]", short_hash(&hash));
                t.stats.send_modify(|s| {
                    s.applied += 1;
                    s.last_event = Some(format!("received {what}"));
                });
            }
            Err(e) => tracing::warn!("writing remote {what} to clipboard failed: {e:#}"),
        },
        RemoteAction::Skip(why) => tracing::debug!("remote {what} skipped: {why}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            max_text_bytes: 100,
            max_image_bytes: 50,
            sync_images: true,
            sync_html: true,
        }
    }

    fn h(n: u8) -> Hash {
        [n; 32]
    }

    fn text(s: &str) -> ClipKind {
        ClipKind::Text(s.into())
    }

    fn remote(origin: &str, kind: ClipKind, hash: Hash) -> Clip {
        Clip {
            clip_id: Uuid::new_v4(),
            origin: origin.into(),
            kind,
            hash,
        }
    }

    #[test]
    fn local_change_is_sent_once() {
        let mut c = SyncCore::new("me".into(), limits());
        let LocalAction::Send(clip) = c.on_local(text("a"), h(1)) else {
            panic!("should send");
        };
        assert_eq!(clip.origin, "me");
        assert_eq!(clip.hash, h(1));
        // Re-notification for the same content (Windows fires several per copy).
        assert!(matches!(
            c.on_local(text("a"), h(1)),
            LocalAction::Ignore(_)
        ));
    }

    #[test]
    fn applied_clip_is_not_echoed() {
        let mut c = SyncCore::new("me".into(), limits());
        let clip = remote("peer", text("x"), h(5));
        assert_eq!(c.on_remote(clip), RemoteAction::Apply(text("x")));
        c.applied(h(5));
        // Writing it fires a local change; it must not be re-sent.
        assert!(matches!(
            c.on_local(text("x"), h(5)),
            LocalAction::Ignore(_)
        ));
    }

    #[test]
    fn lossy_image_roundtrip_is_not_echoed() {
        let mut c = SyncCore::new("me".into(), limits());
        let img = ClipKind::Image(vec![1; 10]);
        assert!(matches!(
            c.on_remote(remote("peer", img.clone(), h(7))),
            RemoteAction::Apply(_)
        ));
        // The OS changed the pixels slightly; we record what it really holds.
        c.applied(h(8));
        assert!(matches!(c.on_local(img, h(8)), LocalAction::Ignore(_)));
    }

    #[test]
    fn duplicates_and_own_clips_skipped() {
        let mut c = SyncCore::new("me".into(), limits());
        let clip = remote("peer", text("x"), h(1));
        assert!(matches!(c.on_remote(clip.clone()), RemoteAction::Apply(_)));
        assert!(matches!(c.on_remote(clip), RemoteAction::Skip(_)));

        assert!(matches!(
            c.on_remote(remote("me", text("y"), h(2))),
            RemoteAction::Skip(_)
        ));

        // Our own broadcast coming back (by id) is also skipped.
        let LocalAction::Send(mine) = c.on_local(text("z"), h(3)) else {
            panic!()
        };
        let mut relayed = mine.clone();
        relayed.origin = "someone".into();
        assert!(matches!(c.on_remote(relayed), RemoteAction::Skip(_)));
    }

    #[test]
    fn remote_equal_to_current_skipped() {
        let mut c = SyncCore::new("me".into(), limits());
        c.set_current(h(9));
        assert!(matches!(
            c.on_remote(remote("peer", text("same"), h(9))),
            RemoteAction::Skip(_)
        ));
    }

    #[test]
    fn recopying_older_content_is_sent_again() {
        // A sends X, then applies Y from B, then the user copies X again on A:
        // that must go out, otherwise A and B stay out of sync.
        let mut c = SyncCore::new("me".into(), limits());
        assert!(matches!(c.on_local(text("X"), h(1)), LocalAction::Send(_)));
        assert!(matches!(
            c.on_remote(remote("b", text("Y"), h(2))),
            RemoteAction::Apply(_)
        ));
        c.applied(h(2));
        assert!(matches!(c.on_local(text("X"), h(1)), LocalAction::Send(_)));
    }

    #[test]
    fn size_limits() {
        let mut c = SyncCore::new("me".into(), limits());
        let big = "x".repeat(101);
        assert!(matches!(
            c.on_local(text(&big), h(1)),
            LocalAction::Ignore(_)
        ));
        assert!(matches!(
            c.on_local(ClipKind::Image(vec![0; 51]), h(2)),
            LocalAction::Ignore(_)
        ));
        assert!(matches!(
            c.on_remote(remote("p", text(&big), h(3))),
            RemoteAction::Skip(_)
        ));
        assert!(matches!(
            c.on_remote(remote("p", ClipKind::Image(vec![0; 51]), h(4))),
            RemoteAction::Skip(_)
        ));
        assert!(matches!(
            c.on_remote(remote("p", ClipKind::Image(vec![0; 50]), h(5))),
            RemoteAction::Apply(_)
        ));
    }

    #[test]
    fn images_disabled() {
        let mut c = SyncCore::new(
            "me".into(),
            Limits {
                sync_images: false,
                ..limits()
            },
        );
        assert!(matches!(
            c.on_local(ClipKind::Image(vec![0; 5]), h(1)),
            LocalAction::Ignore(_)
        ));
        assert!(matches!(
            c.on_remote(remote("p", ClipKind::Image(vec![0; 5]), h(2))),
            RemoteAction::Skip(_)
        ));
    }

    #[test]
    fn rich_text_degrades_to_plain() {
        let html = |h: &str, t: &str| ClipKind::Html {
            html: h.into(),
            text: t.into(),
        };
        let mut c = SyncCore::new("me".into(), limits());
        let LocalAction::Send(clip) = c.on_local(html("<b>a</b>", "a"), h(1)) else {
            panic!()
        };
        assert_eq!(clip.kind, html("<b>a</b>", "a"));

        // HTML over the limit: send the plain text instead of nothing.
        let big = "x".repeat(101);
        let LocalAction::Send(clip) = c.on_local(html(&big, "short"), h(2)) else {
            panic!()
        };
        assert_eq!(clip.kind, text("short"));

        // Plain text itself too large: nothing to send.
        assert!(matches!(
            c.on_local(html("<p>", &big), h(3)),
            LocalAction::Ignore(_)
        ));

        // Rich text disabled: remote HTML is applied as plain text.
        let mut plain = SyncCore::new(
            "me".into(),
            Limits {
                sync_html: false,
                ..limits()
            },
        );
        assert_eq!(
            plain.on_remote(remote("p", html("<i>b</i>", "b"), h(4))),
            RemoteAction::Apply(text("b"))
        );
    }

    #[test]
    fn seen_ids_are_bounded() {
        let mut s = SeenIds::new();
        let first = Uuid::new_v4();
        assert!(s.insert(first));
        assert!(!s.insert(first));
        for _ in 0..SEEN_CAPACITY {
            s.insert(Uuid::new_v4());
        }
        assert_eq!(s.order.len(), SEEN_CAPACITY);
        assert!(s.insert(first), "oldest id evicted");
    }
}
