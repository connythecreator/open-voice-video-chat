//! Pure history, read-marker and unread helpers.
//!
//! The terminal frontend programs against `ovvc_remote::Frontend`, whose
//! surface does not include the store's `unread_counts`, so the read marker is
//! recovered from the signed `ReadUpTo` events that are already part of a
//! channel's history. Keeping the arithmetic here makes it testable without a
//! terminal.

use std::collections::HashSet;

use ovvc_client::{AuthorId, EventId, SignedEvent, SyncCursor, TimelineEntry, timeline};

/// The oldest event in the loaded window, or `None` when nothing is loaded.
///
/// `history_before` takes an exclusive upper bound, so paging from this cursor
/// can never return the event it names again.
pub fn oldest_cursor(events: &[SignedEvent]) -> Option<SyncCursor> {
    events.first().and_then(|event| SyncCursor::at(event).ok())
}

/// Prepend an older page, dropping events that are already loaded.
///
/// `page` is expected oldest first (the order `history_before` returns), and
/// the existing window is kept in the same order after it. Returns how many
/// events were actually added.
pub fn prepend_page(existing: &mut Vec<SignedEvent>, page: &[SignedEvent]) -> usize {
    if page.is_empty() {
        return 0;
    }
    let loaded: HashSet<EventId> = existing
        .iter()
        .filter_map(|event| event.id().ok())
        .collect();
    let mut older: Vec<SignedEvent> = page
        .iter()
        .filter(|event| event.id().ok().is_some_and(|id| !loaded.contains(&id)))
        .cloned()
        .collect();
    let added = older.len();
    older.append(existing);
    *existing = older;
    added
}

/// Install a fresh newest page without discarding older pages already loaded.
///
/// The newest page can shift forward as messages arrive, so only events
/// strictly older than the page's first event and absent from it are kept.
pub fn merge_newest_page(existing: &mut Vec<SignedEvent>, page: &[SignedEvent]) {
    let Some(first) = page.first() else {
        return;
    };
    let Ok(bound) = SyncCursor::at(first) else {
        existing.clear();
        existing.extend_from_slice(page);
        return;
    };
    let page_ids: HashSet<EventId> = page.iter().filter_map(|event| event.id().ok()).collect();

    let mut merged: Vec<SignedEvent> = Vec::new();
    for event in existing.iter() {
        let Ok(id) = event.id() else {
            continue;
        };
        if !page_ids.contains(&id) && bound.is_strictly_before(event.body.lamport, id) {
            merged.push(event.clone());
        }
    }
    merged.extend(page.iter().cloned());
    *existing = merged;
}

/// The newest read cursor named by this identity's `ReadUpTo` events, merged
/// with a cursor remembered for the current session.
///
/// This mirrors the store's `READ_STATE` cursor: the marker carries the
/// `ReadUpTo` event's lamport but names the target event id.
pub fn read_cursor_for(
    events: &[SignedEvent],
    author: AuthorId,
    session: Option<SyncCursor>,
) -> SyncCursor {
    let mut cursor = session.unwrap_or_else(SyncCursor::start);
    for event in events {
        if event.body.author != author {
            continue;
        }
        if let ovvc_client::EventKind::ReadUpTo { event_id } = &event.body.kind {
            let candidate = SyncCursor {
                lamport: event.body.lamport,
                event_id: Some(*event_id),
            };
            if is_newer(&candidate, &cursor) {
                cursor = candidate;
            }
        }
    }
    cursor
}

fn is_newer(candidate: &SyncCursor, current: &SyncCursor) -> bool {
    (candidate.lamport, candidate.event_id) > (current.lamport, current.event_id)
}

/// Count messages from other authors after `cursor`, folded like the store's
/// unread counter so edits and deletes are not counted twice.
///
/// The running app no longer calls this, because unread badges come from the
/// daemon's persisted read markers, but the TUI's own tests still use it to
/// check the folding and cursor rules against a hand-built window.
#[cfg(test)]
pub fn unread_after(events: &[SignedEvent], author: AuthorId, cursor: SyncCursor) -> usize {
    timeline(events)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| match entry {
            TimelineEntry::Message { event, .. } if event.body.author != author => Some(event),
            _ => None,
        })
        .filter(|event| {
            event
                .id()
                .ok()
                .is_some_and(|id| cursor.is_before(event.body.lamport, id))
        })
        .count()
}

/// The first message from another author after `cursor`, used as the position
/// of the "new messages" divider.
pub fn first_unread(
    events: &[SignedEvent],
    author: AuthorId,
    cursor: SyncCursor,
) -> Option<EventId> {
    timeline(events)
        .unwrap_or_default()
        .into_iter()
        .find_map(|entry| match entry {
            TimelineEntry::Message { event, .. } if event.body.author != author => {
                let id = event.id().ok()?;
                cursor.is_before(event.body.lamport, id).then_some(id)
            }
            _ => None,
        })
}

/// Bracket-style unread badge, or `None` when there is nothing unread.
pub fn badge(unread: usize) -> Option<String> {
    (unread > 0).then(|| format!("[{unread}]"))
}

/// The `scroll_back` value that puts `line` at screen row `row` in a window of
/// `height` lines. Used after prepending history so the reader does not move.
pub fn anchored_scroll_back(total: usize, height: usize, line: usize, row: usize) -> usize {
    total.saturating_sub(line.saturating_add(height).saturating_sub(row))
}

#[cfg(test)]
mod tests {
    use super::{
        anchored_scroll_back, badge, first_unread, merge_newest_page, oldest_cursor, prepend_page,
        read_cursor_for, unread_after,
    };
    use ovvc_client::{
        AuthorId, ChannelId, EventBody, EventId, EventKind, GuildId, SignedEvent, SyncCursor,
    };

    /// The current wire version. `ovvc_client` does not re-export the constant,
    /// and the helpers only hash the body, so a literal is enough here.
    const EVENT_VERSION: u8 = 2;

    const ME: AuthorId = AuthorId([1; 32]);
    const OTHER: AuthorId = AuthorId([2; 32]);
    const GUILD: GuildId = GuildId([3; 32]);
    const CHANNEL: ChannelId = ChannelId([4; 16]);

    fn event(author: AuthorId, lamport: u64, kind: EventKind) -> SignedEvent {
        SignedEvent {
            body: EventBody {
                version: EVENT_VERSION,
                guild_id: GUILD,
                channel_id: Some(CHANNEL),
                author,
                seq: lamport,
                lamport,
                timestamp: lamport,
                prev: None,
                kind,
            },
            // The pure helpers only hash the body, so a dummy signature is
            // enough to exercise them without an identity.
            signature: vec![0; 64],
        }
    }

    fn message(author: AuthorId, lamport: u64, body: &str) -> SignedEvent {
        event(author, lamport, EventKind::Message { body: body.into() })
    }

    fn read_up_to(author: AuthorId, lamport: u64, target: EventId) -> SignedEvent {
        event(author, lamport, EventKind::ReadUpTo { event_id: target })
    }

    fn id(event: &SignedEvent) -> EventId {
        event.id().expect("event id")
    }

    #[test]
    fn oldest_cursor_names_the_first_loaded_event() {
        let first = message(OTHER, 1, "one");
        let second = message(OTHER, 2, "two");
        let events = vec![first.clone(), second];
        assert_eq!(
            oldest_cursor(&events),
            Some(SyncCursor::at(&first).unwrap())
        );
        assert_eq!(oldest_cursor(&[]), None);
    }

    #[test]
    fn prepending_older_pages_keeps_order_and_drops_duplicates() {
        let first = message(OTHER, 1, "one");
        let second = message(OTHER, 2, "two");
        let third = message(OTHER, 3, "three");
        let fourth = message(OTHER, 4, "four");

        let mut existing = vec![third.clone(), fourth.clone()];
        let added = prepend_page(
            &mut existing,
            &[first.clone(), second.clone(), third.clone()],
        );

        assert_eq!(added, 2, "the duplicate third must not be added again");
        let ids: Vec<EventId> = existing.iter().map(id).collect();
        assert_eq!(ids, vec![id(&first), id(&second), id(&third), id(&fourth)]);
    }

    #[test]
    fn merging_a_newest_page_keeps_older_pages_and_folds_duplicates() {
        let first = message(OTHER, 1, "one");
        let second = message(OTHER, 2, "two");
        let third = message(OTHER, 3, "three");
        let fourth = message(OTHER, 4, "four");

        // The reader had paged back to `first`/`second`; a refresh now returns
        // the newest window containing `third`, `fourth` and a fresh `fifth`.
        let mut existing = vec![first.clone(), second.clone(), third.clone(), fourth.clone()];
        let fifth = message(OTHER, 5, "five");
        merge_newest_page(
            &mut existing,
            &[third.clone(), fourth.clone(), fifth.clone()],
        );

        let ids: Vec<EventId> = existing.iter().map(id).collect();
        assert_eq!(
            ids,
            vec![id(&first), id(&second), id(&third), id(&fourth), id(&fifth)]
        );
    }

    #[test]
    fn read_cursor_prefers_the_newest_marker_or_the_session_cursor() {
        let one = message(OTHER, 1, "one");
        let two = message(OTHER, 2, "two");
        let marker = read_up_to(ME, 3, id(&two));
        let events = vec![one.clone(), two.clone(), marker, message(OTHER, 4, "four")];

        let cursor = read_cursor_for(&events, ME, None);
        assert_eq!(cursor.lamport, 3);
        assert_eq!(cursor.event_id, Some(id(&two)));

        let session = SyncCursor::at(&message(OTHER, 5, "five")).unwrap();
        assert_eq!(
            read_cursor_for(&events, ME, Some(session)),
            session,
            "a newer session cursor must win"
        );

        assert_eq!(
            read_cursor_for(&[], ME, None),
            SyncCursor::start(),
            "no marker means nothing has been read yet"
        );
    }

    #[test]
    fn divider_anchors_on_the_first_unread_message_and_survives_paging() {
        let one = message(OTHER, 1, "one");
        let two = message(OTHER, 2, "two");
        let marker = read_up_to(ME, 3, id(&two));
        let three = message(OTHER, 4, "three");
        let mine = message(ME, 5, "mine");
        let events = vec![
            one.clone(),
            two.clone(),
            marker,
            three.clone(),
            mine.clone(),
        ];

        let cursor = read_cursor_for(&events, ME, None);
        assert_eq!(first_unread(&events, ME, cursor), Some(id(&three)));

        // Prepending older history must not move the anchor.
        let older = message(OTHER, 0, "older");
        let mut paged = vec![older];
        paged.extend(events);
        let paged_cursor = read_cursor_for(&paged, ME, None);
        assert_eq!(first_unread(&paged, ME, paged_cursor), Some(id(&three)));
    }

    #[test]
    fn unread_count_ignores_own_messages_and_respects_the_marker() {
        let one = message(OTHER, 1, "one");
        let two = message(OTHER, 2, "two");
        let marker = read_up_to(ME, 3, id(&two));
        let three = message(OTHER, 4, "three");
        let mine = message(ME, 5, "mine");
        let events = vec![one, two, marker, three, mine];

        let cursor = read_cursor_for(&events, ME, None);
        assert_eq!(unread_after(&events, ME, cursor), 1);
        assert_eq!(
            unread_after(&events, ME, SyncCursor::start()),
            3,
            "before any marker, other authors' messages are unread"
        );
    }

    #[test]
    fn recovers_a_real_read_marker_from_channel_history() {
        let dir = tempfile::tempdir().expect("temp dir");
        let shared =
            ovvc_client::shared_storage(ovvc_client::Store::open(dir.path()).expect("store"));
        let me =
            ovvc_client::Client::with_storage(ovvc_client::Identity::generate(), shared.clone())
                .expect("reader");
        let other = ovvc_client::Client::with_storage(ovvc_client::Identity::generate(), shared)
            .expect("author");

        let guild = me.create_guild("g").expect("guild");
        let channel = me.default_channel(guild.id).expect("channel");
        let first = other
            .post_message(guild.id, channel.id, "first")
            .expect("message");
        me.mark_read(guild.id, channel.id, first.id().unwrap())
            .expect("marker");
        let second = other
            .post_message(guild.id, channel.id, "second")
            .expect("message");

        let page = me
            .history_before(guild.id, channel.id, None, 200)
            .expect("history");
        let cursor = read_cursor_for(&page.events, me.author_id(), None);
        assert_eq!(
            first_unread(&page.events, me.author_id(), cursor),
            second.id().ok(),
            "the marker is in real channel history"
        );
        assert_eq!(unread_after(&page.events, me.author_id(), cursor), 1);
    }

    #[test]
    fn badges_use_square_brackets_and_hide_zero() {
        assert_eq!(badge(0), None);
        assert_eq!(badge(1), Some("[1]".to_string()));
        assert_eq!(badge(12), Some("[12]".to_string()));
    }

    #[test]
    fn anchored_scroll_keeps_the_same_message_on_screen() {
        // The reader is 40 lines from the old bottom, with line 100 at row 3.
        let height = 20;
        let row = 3;
        let old_total = 200;
        let old_line = 100;
        let old_scroll = anchored_scroll_back(old_total, height, old_line, row);
        assert_eq!(old_scroll, 200 - (100 + 20 - 3));

        // Prepending 30 lines pushes the old message down by 30 but the same
        // bottom-relative offset must put it back on row 3.
        let new_total = old_total + 30;
        let new_line = old_line + 30;
        let new_scroll = anchored_scroll_back(new_total, height, new_line, row);
        assert_eq!(new_scroll, old_scroll);
    }
}
