//! Deterministic channel timeline folding.
//!
//! Edits and deletes are separate events, so every renderer that shows a
//! channel needs the same folding rule or two peers will disagree about what a
//! message says. This module is that rule, shared by every frontend:
//!
//! * a modification only applies when its author is the author of the target
//!   message (role-based moderation arrives with roles in Phase 2, and will
//!   extend this check rather than bypass it);
//! * among the edits that apply, the greatest `(lamport, event_id)` wins, which
//!   is the same deterministic order the event log itself uses;
//! * a delete is terminal: once an authorized delete has been applied, a later
//!   edit cannot resurrect the message;
//! * modifications whose target is not in the supplied window (or is not a
//!   message at all) are ignored, so a partial history never invents content;
//! * a reply reference is only resolved when the referenced message is in the
//!   same window, and it shows that message's *folded* body. A reply to a
//!   deleted message renders as a tombstone reference rather than the original
//!   text.
//!
//! The output keeps the input order and replaces each message with its folded
//! state. Modification events themselves are not emitted.

use std::collections::HashMap;

use crate::error::Result;
use crate::model::{AuthorId, EventId, EventKind, SignedEvent};

/// A reply's reference to an earlier message in the same window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyRef<'a> {
    pub event: &'a SignedEvent,
    /// The referenced message's effective body, or `None` when it was deleted.
    pub body: Option<String>,
}

/// One emoji and the members who currently reacted with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaction {
    pub emoji: String,
    /// Authors who have a live reaction, sorted by id for a stable render.
    pub authors: Vec<AuthorId>,
}

/// One entry of a folded channel timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineEntry<'a> {
    /// A message that still has content to show, with its effective body.
    Message {
        event: &'a SignedEvent,
        body: String,
        edited: bool,
        /// Set when this message replies to another message in this window.
        reply_to: Option<ReplyRef<'a>>,
        /// Roster ids mentioned by this message, in the order the sender
        /// stored them. Names are resolved at render time, never stored.
        mentions: &'a [AuthorId],
        /// Live reactions, in the order their emoji first appeared.
        reactions: Vec<Reaction>,
    },
    /// A message its author deleted. Renderers show a tombstone instead.
    Deleted { event: &'a SignedEvent },
    /// Any other event (`MemberJoined`, `ChannelCreated`, …).
    Other(&'a SignedEvent),
}

struct MessageState<'a> {
    author: AuthorId,
    body: String,
    edited: bool,
    deleted: bool,
    reply_to: Option<EventId>,
    mentions: &'a [AuthorId],
}

#[derive(Default)]
struct ReactionState {
    order: Vec<String>,
    present: HashMap<(String, AuthorId), bool>,
}

impl ReactionState {
    fn set(&mut self, emoji: &str, author: AuthorId, added: bool) {
        if added && !self.order.iter().any(|existing| existing == emoji) {
            self.order.push(emoji.to_string());
        }
        self.present.insert((emoji.to_string(), author), added);
    }

    fn folded(&self) -> Vec<Reaction> {
        self.order
            .iter()
            .filter_map(|emoji| {
                let mut authors: Vec<AuthorId> = self
                    .present
                    .iter()
                    .filter(|((existing, _), live)| existing == emoji && **live)
                    .map(|((_, author), _)| *author)
                    .collect();
                authors.sort();
                if authors.is_empty() {
                    None
                } else {
                    Some(Reaction {
                        emoji: emoji.clone(),
                        authors,
                    })
                }
            })
            .collect()
    }
}

/// Fold `events` — a channel's history in `(lamport, event_id)` order — into
/// renderable entries with edits and deletes applied.
pub fn timeline<'a>(events: &'a [SignedEvent]) -> Result<Vec<TimelineEntry<'a>>> {
    let mut states: HashMap<EventId, MessageState<'a>> = HashMap::new();
    // Reply targets are looked up by id in the output pass; keeping the event
    // alongside its folded state avoids a linear scan of `events` per reply.
    let mut messages_by_id: HashMap<EventId, &'a SignedEvent> = HashMap::new();
    let mut modifications: Vec<(u64, EventId, &'a SignedEvent)> = Vec::new();
    let mut reaction_events: Vec<(u64, EventId, &'a SignedEvent)> = Vec::new();

    for event in events {
        let kind = &event.body.kind;
        if kind.is_message() {
            let id = event.id()?;
            let body = kind
                .message_body()
                .expect("is_message implies body")
                .to_owned();
            states.entry(id).or_insert_with(|| MessageState {
                author: event.body.author,
                body,
                edited: false,
                deleted: false,
                reply_to: kind.reply_to(),
                mentions: kind.mentions(),
            });
            messages_by_id.entry(id).or_insert(event);
        } else if matches!(
            kind,
            EventKind::MessageEdited { .. } | EventKind::MessageDeleted { .. }
        ) {
            modifications.push((event.body.lamport, event.id()?, event));
        } else if kind.reaction().is_some() {
            reaction_events.push((event.body.lamport, event.id()?, event));
        }
    }

    // Sorting makes the fold independent of the order events were received in;
    // the log's own order is a total order, so this is stable.
    modifications.sort_by_key(|(lamport, id, _)| (*lamport, *id));
    for (_, _, event) in modifications {
        match &event.body.kind {
            EventKind::MessageEdited { target, body } => {
                if let Some(state) = states.get_mut(target)
                    && state.author == event.body.author
                    && !state.deleted
                {
                    state.body = body.clone();
                    state.edited = true;
                }
            }
            EventKind::MessageDeleted { target } => {
                if let Some(state) = states.get_mut(target)
                    && state.author == event.body.author
                {
                    state.deleted = true;
                }
            }
            _ => {}
        }
    }

    let mut out = Vec::with_capacity(events.len());
    // Reactions fold the same way edits do: in `(lamport, event_id)` order, so
    // an add followed by a remove leaves no reaction no matter which order the
    // events arrived in.
    reaction_events.sort_by_key(|(lamport, id, _)| (*lamport, *id));
    let mut reactions: HashMap<EventId, ReactionState> = HashMap::new();
    for (_, _, event) in &reaction_events {
        let Some((target, emoji, added)) = event.body.kind.reaction() else {
            continue;
        };
        if !states.contains_key(&target) {
            // A reaction to a message outside this window cannot be shown, but
            // keeping it would make a later page disagree with this one.
            continue;
        }
        reactions
            .entry(target)
            .or_default()
            .set(emoji, event.body.author, added);
    }
    for event in events {
        let kind = &event.body.kind;
        if kind.is_message() {
            let id = event.id()?;
            let Some(state) = states.get(&id) else {
                continue;
            };
            if state.deleted {
                out.push(TimelineEntry::Deleted { event });
                continue;
            }
            let reply_to = state.reply_to.and_then(|target| {
                // Targets outside the window (older than the page) do not
                // resolve, which renders the reply as a plain message.
                messages_by_id.get(&target).map(|&target_event| ReplyRef {
                    event: target_event,
                    body: states
                        .get(&target)
                        .filter(|target_state| !target_state.deleted)
                        .map(|target_state| target_state.body.clone()),
                })
            });
            out.push(TimelineEntry::Message {
                event,
                body: state.body.clone(),
                edited: state.edited,
                reply_to,
                mentions: state.mentions,
                reactions: reactions
                    .get(&id)
                    .map(ReactionState::folded)
                    .unwrap_or_default(),
            });
        } else if matches!(
            kind,
            EventKind::MessageEdited { .. } | EventKind::MessageDeleted { .. }
        ) {
            // Folded away.
        } else if kind.reaction().is_some() {
            // Folded into the message they name.
        } else {
            out.push(TimelineEntry::Other(event));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use crate::model::{ChannelId, EVENT_VERSION, EventBody, GuildId, now_unix};

    fn event_with(identity: &Identity, seq: u64, lamport: u64, kind: EventKind) -> SignedEvent {
        identity
            .sign_body(EventBody {
                version: EVENT_VERSION,
                guild_id: GuildId([7; 32]),
                channel_id: Some(ChannelId([9; 16])),
                author: identity.author_id(),
                seq,
                lamport,
                timestamp: now_unix(),
                prev: None,
                kind,
            })
            .unwrap()
    }

    fn message(identity: &Identity, seq: u64, lamport: u64, body: &str) -> SignedEvent {
        event_with(
            identity,
            seq,
            lamport,
            EventKind::Message {
                body: body.to_string(),
            },
        )
    }

    fn reply(
        identity: &Identity,
        seq: u64,
        lamport: u64,
        body: &str,
        target: EventId,
        mentions: Vec<AuthorId>,
    ) -> SignedEvent {
        event_with(
            identity,
            seq,
            lamport,
            EventKind::MessageV2 {
                body: body.to_string(),
                reply_to: Some(target),
                mentions,
            },
        )
    }

    fn edit(
        identity: &Identity,
        seq: u64,
        lamport: u64,
        target: EventId,
        body: &str,
    ) -> SignedEvent {
        event_with(
            identity,
            seq,
            lamport,
            EventKind::MessageEdited {
                target,
                body: body.to_string(),
            },
        )
    }

    fn delete(identity: &Identity, seq: u64, lamport: u64, target: EventId) -> SignedEvent {
        event_with(identity, seq, lamport, EventKind::MessageDeleted { target })
    }

    fn react(
        identity: &Identity,
        seq: u64,
        lamport: u64,
        target: EventId,
        emoji: &str,
        added: bool,
    ) -> SignedEvent {
        let kind = if added {
            EventKind::ReactionAdded {
                target,
                emoji: emoji.to_string(),
            }
        } else {
            EventKind::ReactionRemoved {
                target,
                emoji: emoji.to_string(),
            }
        };
        event_with(identity, seq, lamport, kind)
    }

    fn body_of<'a>(entries: &'a [TimelineEntry<'a>]) -> Vec<Option<&'a str>> {
        entries
            .iter()
            .map(|entry| match entry {
                TimelineEntry::Message { body, .. } => Some(body.as_str()),
                TimelineEntry::Deleted { .. } | TimelineEntry::Other(_) => None,
            })
            .collect()
    }

    #[test]
    fn edits_apply_and_keep_order() {
        let alice = Identity::generate();
        let first = message(&alice, 0, 1, "hello");
        let target = first.id().unwrap();
        let second = edit(&alice, 1, 2, target, "hello, world");
        let events = [first, second];
        let entries = timeline(&events).unwrap();
        // The modification event itself is folded away, not rendered.
        assert_eq!(body_of(&entries), vec![Some("hello, world")]);
        assert!(matches!(
            entries[0],
            TimelineEntry::Message { edited: true, .. }
        ));
    }

    #[test]
    fn unauthorized_edits_are_ignored() {
        let alice = Identity::generate();
        let mallory = Identity::generate();
        let original = message(&alice, 0, 1, "original");
        let target = original.id().unwrap();
        let forged = edit(&mallory, 0, 2, target, "forged");
        let events = [original, forged];
        let entries = timeline(&events).unwrap();
        assert_eq!(body_of(&entries), vec![Some("original")]);
        assert!(matches!(
            entries[0],
            TimelineEntry::Message { edited: false, .. }
        ));
    }

    #[test]
    fn latest_edit_wins_regardless_of_input_order() {
        let alice = Identity::generate();
        let original = message(&alice, 0, 1, "v1");
        let target = original.id().unwrap();
        let earlier = edit(&alice, 1, 2, target, "v2");
        let later = edit(&alice, 2, 3, target, "v3");
        let forward_events = [original.clone(), earlier.clone(), later.clone()];
        let forward = timeline(&forward_events).unwrap();
        let shuffled_events = [later, original, earlier];
        let shuffled = timeline(&shuffled_events).unwrap();
        assert_eq!(body_of(&forward), vec![Some("v3")]);
        assert_eq!(body_of(&forward), body_of(&shuffled));
    }

    #[test]
    fn delete_is_terminal_even_before_a_later_edit_arrives() {
        let alice = Identity::generate();
        let original = message(&alice, 0, 1, "to be deleted");
        let target = original.id().unwrap();
        let removed = delete(&alice, 1, 2, target);
        let revisionist = edit(&alice, 2, 3, target, "back from the dead");
        let events = [original, removed, revisionist];
        let entries = timeline(&events).unwrap();
        assert_eq!(body_of(&entries), vec![None]);
        assert!(matches!(entries[0], TimelineEntry::Deleted { .. }));
    }

    #[test]
    fn orphaned_modifications_are_ignored() {
        let alice = Identity::generate();
        let target = EventId([1; 32]);
        let orphan = edit(&alice, 0, 5, target, "nothing to edit");
        let events = [orphan];
        let entries = timeline(&events).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn replies_resolve_to_the_folded_target_body() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let original = message(&alice, 0, 1, "original");
        let target = original.id().unwrap();
        let revised = edit(&alice, 1, 2, target, "revised");
        let answer = reply(&bob, 0, 3, "answering", target, Vec::new());
        let events = [original, revised, answer];
        let entries = timeline(&events).unwrap();
        assert_eq!(body_of(&entries), vec![Some("revised"), Some("answering")]);
        let TimelineEntry::Message {
            reply_to: Some(reply_ref),
            ..
        } = &entries[1]
        else {
            panic!("expected a resolved reply, got {:?}", entries[1]);
        };
        // The reference shows the edited body, not the stale original.
        assert_eq!(reply_ref.body.as_deref(), Some("revised"));
        assert_eq!(reply_ref.event.id().unwrap(), target);
    }

    #[test]
    fn replies_to_deleted_messages_keep_only_the_tombstone() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let original = message(&alice, 0, 1, "gone soon");
        let target = original.id().unwrap();
        let removed = delete(&alice, 1, 2, target);
        let answer = reply(&bob, 0, 3, "too late", target, Vec::new());
        let events = [original, removed, answer];
        let entries = timeline(&events).unwrap();
        let TimelineEntry::Message {
            reply_to: Some(reply_ref),
            ..
        } = &entries[1]
        else {
            panic!("expected a resolved reply, got {:?}", entries[1]);
        };
        assert_eq!(reply_ref.body, None);
    }

    #[test]
    fn replies_outside_the_window_are_plain_messages() {
        let bob = Identity::generate();
        let target = EventId([2; 32]);
        let answer = reply(&bob, 0, 3, "who knows", target, Vec::new());
        let events = [answer];
        let entries = timeline(&events).unwrap();
        let TimelineEntry::Message { reply_to, .. } = &entries[0] else {
            panic!("expected a message");
        };
        assert!(reply_to.is_none());
    }

    #[test]
    fn mentions_survive_folding_with_their_ids() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let carol = Identity::generate();
        let original = message(&alice, 0, 1, "hello");
        let target = original.id().unwrap();
        let answer = reply(
            &bob,
            0,
            2,
            "hi @carol",
            target,
            vec![carol.author_id(), carol.author_id()],
        );
        let events = [original, answer];
        let entries = timeline(&events).unwrap();
        let TimelineEntry::Message { mentions, .. } = &entries[1] else {
            panic!("expected a message");
        };
        assert_eq!(*mentions, [carol.author_id(), carol.author_id()]);
    }

    #[test]
    fn reactions_aggregate_per_emoji_and_author() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let original = message(&alice, 0, 1, "hello");
        let target = original.id().unwrap();
        let alice_heart = react(&alice, 1, 2, target, "❤", true);
        let bob_heart = react(&bob, 0, 3, target, "❤", true);
        let bob_wave = react(&bob, 1, 4, target, "👋", true);
        let events = [original, alice_heart, bob_heart, bob_wave];
        let entries = timeline(&events).unwrap();
        let TimelineEntry::Message { reactions, .. } = &entries[0] else {
            panic!("expected a message");
        };
        assert_eq!(reactions.len(), 2);
        assert_eq!(reactions[0].emoji, "❤");
        let mut both = vec![alice.author_id(), bob.author_id()];
        both.sort();
        assert_eq!(reactions[0].authors, both);
        assert_eq!(reactions[1].emoji, "👋");
        assert_eq!(reactions[1].authors, vec![bob.author_id()]);
    }

    #[test]
    fn removed_reactions_disappear_regardless_of_input_order() {
        let alice = Identity::generate();
        let original = message(&alice, 0, 1, "hello");
        let target = original.id().unwrap();
        let added = react(&alice, 1, 2, target, "👍", true);
        let removed = react(&alice, 2, 3, target, "👍", false);
        let forward = [original.clone(), added.clone(), removed.clone()];
        let shuffled = [removed, original, added];
        for events in [forward.as_slice(), shuffled.as_slice()] {
            let entries = timeline(events).unwrap();
            let TimelineEntry::Message { reactions, .. } = &entries[0] else {
                panic!("expected a message");
            };
            assert!(reactions.is_empty());
        }
    }

    #[test]
    fn reactions_to_deleted_messages_are_not_shown() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let original = message(&alice, 0, 1, "hello");
        let target = original.id().unwrap();
        let removed = delete(&alice, 1, 2, target);
        let reaction = react(&bob, 0, 3, target, "👍", true);
        let events = [original, removed, reaction];
        let entries = timeline(&events).unwrap();
        assert!(matches!(entries[0], TimelineEntry::Deleted { .. }));
    }
}
