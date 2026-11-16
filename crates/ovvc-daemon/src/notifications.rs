//! Mention and reply notification policy.
//!
//! The decision itself is a pure function so it can be unit-tested without a
//! store, a network or a notification daemon. [`spawn_notifier`] is the only
//! part that talks to DBus, and only when the daemon was built with the
//! `notify` feature and started with `--notify`.

use std::collections::{HashSet, VecDeque};

use ovvc_core::{AuthorId, EventId, MemberInfo, SignedEvent, has_mention_syntax, parse_mentions};
use ovvc_ipc::{DaemonEvent, NotificationReason};
use tokio::sync::broadcast;

/// How many event ids the dedupe window remembers.
///
/// A resync can re-import an event that was already seen, but the daemon never
/// keeps more than this many ids, so the set cannot grow without bound.
pub const DEDUPE_CAPACITY: usize = 512;

/// Decide whether `event` should produce a [`DaemonEvent::Notification`].
///
/// `reply_author` is the author of the event `event` replies to, when the
/// daemon could resolve it from the store. Mentioned authors win over replies
/// because a message can do both and the mention is the more direct signal.
pub fn notification_reason(
    event: &SignedEvent,
    local_author: AuthorId,
    local_display_name: &str,
    roster: &[MemberInfo],
    reply_author: Option<AuthorId>,
) -> Option<NotificationReason> {
    if event.body.author == local_author {
        return None;
    }
    let body = event.body.kind.message_body()?;
    if event.body.kind.mentions_author(local_author) {
        return Some(NotificationReason::Mention);
    }
    if mentions_local_in_text(
        body,
        event.body.guild_id,
        local_author,
        local_display_name,
        roster,
    ) {
        return Some(NotificationReason::Mention);
    }
    if event.body.kind.reply_to().is_some() && reply_author == Some(local_author) {
        return Some(NotificationReason::Reply);
    }
    None
}

/// Resolve `@name` text against the roster, adding the local member if the
/// roster is stale and does not include them yet.
fn mentions_local_in_text(
    body: &str,
    guild_id: ovvc_core::GuildId,
    local_author: AuthorId,
    local_display_name: &str,
    roster: &[MemberInfo],
) -> bool {
    if !has_mention_syntax(body) {
        return false;
    }
    let mut members = roster.to_vec();
    let local_name = local_display_name.trim();
    match members
        .iter_mut()
        .find(|member| member.author == local_author)
    {
        Some(member) => {
            if !local_name.is_empty() {
                member.display_name = local_name.to_string();
            }
        }
        None => members.push(MemberInfo {
            guild_id,
            author: local_author,
            display_name: local_name.to_string(),
            joined_at: 0,
        }),
    }
    parse_mentions(body, &members).contains(&local_author)
}

/// A bounded, first-seen set of event ids.
///
/// `first_time` returns `true` once per id. When the window is full the oldest
/// id is evicted, which is enough to stop a resync from notifying twice while
/// keeping memory flat.
#[derive(Debug, Default)]
pub struct NotificationFilter {
    seen: HashSet<EventId>,
    order: VecDeque<EventId>,
}

impl NotificationFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `event_id` and report whether it is new.
    pub fn first_time(&mut self, event_id: EventId) -> bool {
        if !self.seen.insert(event_id) {
            return false;
        }
        self.order.push_back(event_id);
        while self.order.len() > DEDUPE_CAPACITY {
            if let Some(evicted) = self.order.pop_front() {
                self.seen.remove(&evicted);
            }
        }
        true
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

/// Start the desktop notifier when the `notify` feature is built in.
///
/// The caller only invokes this after `--notify`, so a headless daemon never
/// touches DBus.
#[cfg(feature = "notify")]
pub fn start(
    events: broadcast::Receiver<DaemonEvent>,
    settings: std::sync::Arc<std::sync::Mutex<ovvc_core::NotificationSettings>>,
) -> Option<tokio::task::JoinHandle<()>> {
    Some(spawn_notifier(events, settings))
}

#[cfg(not(feature = "notify"))]
pub fn start(
    _events: broadcast::Receiver<DaemonEvent>,
    _settings: std::sync::Arc<std::sync::Mutex<ovvc_core::NotificationSettings>>,
) -> Option<tokio::task::JoinHandle<()>> {
    eprintln!("ovvc-daemon: --notify ignored; this build has no `notify` feature");
    None
}

/// Forward daemon notifications to the desktop, one blocking call at a time.
///
/// The task only ever runs when the caller passes `--notify`. Each notification
/// is handed to `spawn_blocking`, so a slow or missing notification service
/// cannot stall the daemon's event loop. The settings handle is consulted per
/// event, so turning desktop notifications off takes effect immediately rather
/// than at the next daemon restart.
#[cfg(feature = "notify")]
fn spawn_notifier(
    mut events: broadcast::Receiver<DaemonEvent>,
    settings: std::sync::Arc<std::sync::Mutex<ovvc_core::NotificationSettings>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event @ DaemonEvent::Notification { .. }) => {
                    let enabled = settings.lock().expect("settings mutex poisoned").desktop;
                    if !enabled {
                        continue;
                    }
                    // Detached on purpose: the daemon never joins a
                    // notification, it just fails soft if one cannot be shown.
                    drop(tokio::task::spawn_blocking(move || {
                        show_desktop_notification(&event);
                    }));
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    })
}

/// Show one notification, logging and swallowing every failure.
#[cfg(feature = "notify")]
fn show_desktop_notification(event: &DaemonEvent) {
    let DaemonEvent::Notification {
        author,
        body,
        reason,
        ..
    } = event
    else {
        return;
    };
    let summary = match reason {
        NotificationReason::Mention => "ovvc: you were mentioned",
        NotificationReason::Reply => "ovvc: you were replied to",
    };
    let result = notify_rust::Notification::new()
        .appname("ovvc")
        .summary(summary)
        .body(&format!("{author}: {body}"))
        .icon("utilities-terminal")
        .show();
    if let Err(error) = result {
        eprintln!("ovvc-daemon: desktop notification failed: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovvc_core::{EventBody, EventKind, GuildId, Identity};

    fn local() -> Identity {
        Identity::from_seed([7u8; 32])
    }

    fn other() -> Identity {
        Identity::from_seed([9u8; 32])
    }

    fn guild() -> GuildId {
        GuildId([1; 32])
    }

    fn member(identity: &Identity, name: &str) -> MemberInfo {
        MemberInfo {
            guild_id: guild(),
            author: identity.author_id(),
            display_name: name.to_string(),
            joined_at: 0,
        }
    }

    fn event(kind: EventKind, seq: u64) -> SignedEvent {
        event_from(&other(), kind, seq)
    }

    fn event_from(identity: &Identity, kind: EventKind, seq: u64) -> SignedEvent {
        identity
            .sign_body(EventBody {
                version: 2,
                guild_id: guild(),
                channel_id: Some(ovvc_core::ChannelId([2; 16])),
                author: identity.author_id(),
                seq,
                lamport: seq,
                timestamp: 1_700_000_000 + seq,
                prev: None,
                kind,
            })
            .unwrap()
    }

    fn message(body: &str) -> EventKind {
        EventKind::Message {
            body: body.to_string(),
        }
    }

    fn decide(event: &SignedEvent, roster: &[MemberInfo]) -> Option<NotificationReason> {
        notification_reason(event, local().author_id(), "Alice", roster, None)
    }

    #[test]
    fn own_messages_and_non_message_kinds_are_not_notifiable() {
        let roster = vec![member(&local(), "Alice")];
        let own = event_from(
            &local(),
            EventKind::MessageV2 {
                body: "@Alice hi".to_string(),
                reply_to: None,
                mentions: vec![local().author_id()],
            },
            1,
        );
        assert_eq!(decide(&own, &roster), None);
        let reaction = event(
            EventKind::ReactionAdded {
                target: EventId([3; 32]),
                emoji: "x".to_string(),
            },
            2,
        );
        assert_eq!(decide(&reaction, &roster), None);
    }

    #[test]
    fn message_v2_mentions_notify() {
        let roster = vec![member(&local(), "Alice")];
        let mentioned = event(
            EventKind::MessageV2 {
                body: "look at this".to_string(),
                reply_to: None,
                mentions: vec![local().author_id()],
            },
            1,
        );
        assert_eq!(
            decide(&mentioned, &roster),
            Some(NotificationReason::Mention)
        );
    }

    #[test]
    fn at_name_syntax_is_resolved_through_the_roster() {
        let roster = vec![member(&local(), "Alice")];
        let mentioned = event(message("@Alice can you check this?"), 1);
        assert_eq!(
            decide(&mentioned, &roster),
            Some(NotificationReason::Mention)
        );
        // A name that merely starts with the local name is not a mention.
        let not_mentioned = event(message("@Alicia can you check this?"), 2);
        assert_eq!(decide(&not_mentioned, &roster), None);
    }

    #[test]
    fn a_stale_roster_still_uses_the_local_display_name() {
        let roster: Vec<MemberInfo> = Vec::new();
        let mentioned = event(message("hey @Alice"), 1);
        assert_eq!(
            decide(&mentioned, &roster),
            Some(NotificationReason::Mention)
        );
    }

    #[test]
    fn replies_to_the_local_author_notify_and_others_do_not() {
        let roster = vec![member(&local(), "Alice"), member(&other(), "Bob")];
        let reply = event(
            EventKind::MessageV2 {
                body: "answering".to_string(),
                reply_to: Some(EventId([4; 32])),
                mentions: Vec::new(),
            },
            1,
        );
        assert_eq!(
            notification_reason(
                &reply,
                local().author_id(),
                "Alice",
                &roster,
                Some(local().author_id())
            ),
            Some(NotificationReason::Reply)
        );
        assert_eq!(
            notification_reason(
                &reply,
                local().author_id(),
                "Alice",
                &roster,
                Some(other().author_id())
            ),
            None
        );
    }

    #[test]
    fn mention_wins_over_reply() {
        let roster = vec![member(&local(), "Alice")];
        let both = event(
            EventKind::MessageV2 {
                body: "@Alice".to_string(),
                reply_to: Some(EventId([4; 32])),
                mentions: vec![local().author_id()],
            },
            1,
        );
        assert_eq!(
            notification_reason(
                &both,
                local().author_id(),
                "Alice",
                &roster,
                Some(local().author_id())
            ),
            Some(NotificationReason::Mention)
        );
    }

    #[test]
    fn dedupe_fires_once_per_event_id() {
        let mut filter = NotificationFilter::new();
        let id = EventId([5; 32]);
        assert!(filter.first_time(id));
        assert!(!filter.first_time(id));
        assert_eq!(filter.len(), 1);
        assert!(!filter.is_empty());
    }

    #[cfg(feature = "notify")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn desktop_notifications_reach_the_dbus_service() {
        use std::path::PathBuf;
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        use ovvc_core::{ChannelId, NotificationSettings};

        // The session bus and the mock `org.freedesktop.Notifications`
        // service are provided by `scripts/e2e-dbus-notifications.sh`. A plain
        // `cargo test` has neither, so skip instead of failing.
        let Ok(log) = std::env::var("OVVC_NOTIFY_LOG") else {
            eprintln!("skipping DBus notification test; run scripts/e2e-dbus-notifications.sh");
            return;
        };
        let log = PathBuf::from(log);

        let (tx, rx) = broadcast::channel(16);
        let settings = Arc::new(Mutex::new(NotificationSettings::default()));
        let task = spawn_notifier(rx, Arc::clone(&settings));

        fn note(author: AuthorId, body: &str, reason: NotificationReason) -> DaemonEvent {
            DaemonEvent::Notification {
                guild_id: guild(),
                channel_id: ChannelId([2; 16]),
                event_id: EventId([7; 32]),
                author,
                body: body.to_string(),
                reason,
            }
        }

        async fn wait_for(log: &std::path::Path, needles: &[&str]) {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                let text = std::fs::read_to_string(log).unwrap_or_default();
                if needles.iter().all(|needle| text.contains(needle)) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {needles:?}; log so far:\n{text}"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        let author = AuthorId([0xAB; 32]);
        tx.send(note(author, "first body", NotificationReason::Mention))
            .expect("send mention event");
        wait_for(
            &log,
            &["ovvc: you were mentioned", &format!("{author}: first body")],
        )
        .await;

        tx.send(note(author, "second body", NotificationReason::Reply))
            .expect("send reply event");
        wait_for(
            &log,
            &[
                "ovvc: you were replied to",
                &format!("{author}: second body"),
            ],
        )
        .await;

        // Turning the preference off is checked per event, so the next
        // notification must never reach DBus.
        let before = std::fs::read_to_string(&log).expect("read notification log");
        let notes = before.matches("\"method\": \"Notify\"").count();
        settings.lock().expect("settings mutex").desktop = false;
        tx.send(note(author, "muted body", NotificationReason::Mention))
            .expect("send muted event");
        tokio::time::sleep(Duration::from_millis(700)).await;
        let after = std::fs::read_to_string(&log).expect("read notification log");
        assert_eq!(
            after.matches("\"method\": \"Notify\"").count(),
            notes,
            "a disabled desktop notification still reached DBus:\n{after}"
        );
        assert!(!after.contains("muted body"));

        task.abort();
    }

    #[test]
    fn dedupe_window_is_bounded_and_evicts_the_oldest() {
        let mut filter = NotificationFilter::new();
        let first = EventId([0; 32]);
        assert!(filter.first_time(first));
        for index in 1..=DEDUPE_CAPACITY {
            let mut bytes = [0u8; 32];
            bytes[..8].copy_from_slice(&(index as u64).to_be_bytes());
            assert!(filter.first_time(EventId(bytes)));
        }
        assert_eq!(filter.len(), DEDUPE_CAPACITY);
        // The oldest id has fallen out of the window, so it may notify again.
        assert!(filter.first_time(first));
        assert_eq!(filter.len(), DEDUPE_CAPACITY);
    }
}
