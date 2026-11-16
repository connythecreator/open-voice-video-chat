use std::collections::HashSet;
use std::fs;
use std::io::Write;

use ovvc_core::error::CoreError;
use ovvc_core::model::now_unix;
use ovvc_core::{
    ChannelId, ChannelKind, EventBody, EventId, EventKind, GuildId, Identity, MAX_PROFILE_ABOUT_BYTES,
    MAX_PROFILE_LINKS, SignedEvent, Storage, SyncCursor, direct_guild_id,
};
use ovvc_store::Store;
use tempfile::tempdir;

fn identity(seed: u8) -> Identity {
    Identity::from_seed([seed; 32])
}

#[test]
fn signs_verifies_and_rejects_tampering() {
    let alice = identity(1);
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let created = store.create_guild("test", &alice).unwrap();
    let event = store
        .post_message(created.guild.id, created.general.id, "hello", &alice)
        .unwrap();

    assert!(event.verify().is_ok());
    let mut tampered = event.clone();
    tampered.body.kind = EventKind::Message {
        body: "evil".to_string(),
    };
    assert!(tampered.verify().is_err());
}

#[test]
fn guild_channel_and_message_roundtrip() {
    let alice = identity(2);
    let dir = tempdir().unwrap();
    let created = {
        let mut store = Store::open(dir.path()).unwrap();
        let created = store.create_guild("  Test Guild  ", &alice).unwrap();
        assert_eq!(created.guild.name, "Test Guild");
        assert_eq!(created.general.name, "general");
        assert_eq!(store.list_guilds().unwrap().len(), 1);
        assert_eq!(store.list_channels(created.guild.id).unwrap().len(), 1);

        let event = store
            .post_message(created.guild.id, created.general.id, "hello world", &alice)
            .unwrap();
        assert_eq!(event.body.seq, 2);
        assert_eq!(event.body.lamport, 3);
        assert_eq!(store.list_members(created.guild.id).unwrap().len(), 1);
        created
    };

    let store = Store::open(dir.path()).unwrap();
    let history = store
        .history(created.guild.id, created.general.id, 50)
        .unwrap();
    let messages: Vec<_> = history
        .iter()
        .filter(|event| matches!(event.body.kind, EventKind::Message { .. }))
        .collect();
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].body.kind,
        EventKind::Message {
            body: "hello world".to_string()
        }
    );
    assert!(messages[0].verify().is_ok());
}

#[test]
fn multiple_authors_order_and_verify() {
    let alice = identity(3);
    let bob = identity(4);
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let created = store.create_guild("people", &alice).unwrap();
    store
        .add_member(created.guild.id, bob.author_id(), "bob")
        .unwrap();

    let first = store
        .post_message(created.guild.id, created.general.id, "from alice", &alice)
        .unwrap();
    let second = store
        .post_message(created.guild.id, created.general.id, "from bob", &bob)
        .unwrap();
    let third = store
        .post_message(created.guild.id, created.general.id, "alice again", &alice)
        .unwrap();

    assert_eq!(first.body.lamport + 1, second.body.lamport);
    assert_eq!(second.body.lamport + 1, third.body.lamport);

    let messages: Vec<_> = store
        .history(created.guild.id, created.general.id, 50)
        .unwrap()
        .into_iter()
        .filter(|event| matches!(event.body.kind, EventKind::Message { .. }))
        .collect();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].body.author, alice.author_id());
    assert_eq!(messages[1].body.author, bob.author_id());
    assert_eq!(messages[2].body.author, alice.author_id());
    for message in messages {
        assert!(message.verify().is_ok());
    }
}

#[test]
fn lookup_by_name_short_id_and_channel_name() {
    let alice = identity(5);
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let created = store.create_guild("lookup", &alice).unwrap();
    let random = store
        .create_channel(created.guild.id, "random", &alice)
        .unwrap();

    assert_eq!(store.find_guild("lookup").unwrap().id, created.guild.id);
    assert_eq!(
        store.find_guild(&created.guild.id.short()).unwrap().id,
        created.guild.id
    );
    assert_eq!(
        store.find_channel(created.guild.id, "random").unwrap().id,
        random.id
    );
    assert_eq!(
        store
            .find_channel(created.guild.id, &random.id.short())
            .unwrap()
            .id,
        random.id
    );
}

#[test]
fn rejects_invalid_messages() {
    let alice = identity(6);
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let created = store.create_guild("validation", &alice).unwrap();

    assert!(
        store
            .post_message(created.guild.id, created.general.id, "   ", &alice)
            .is_err()
    );
    let too_long = "x".repeat(16 * 1024 + 1);
    assert!(
        store
            .post_message(created.guild.id, created.general.id, &too_long, &alice)
            .is_err()
    );
}

#[test]
fn rebuilds_indexes_from_event_logs() {
    let alice = identity(7);
    let dir = tempdir().unwrap();
    let created = {
        let mut store = Store::open(dir.path()).unwrap();
        let created = store.create_guild("recovery", &alice).unwrap();
        store
            .post_message(created.guild.id, created.general.id, "persisted", &alice)
            .unwrap();
        created
    };

    fs::remove_file(dir.path().join("index.redb")).unwrap();
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.list_guilds().unwrap().len(), 1);
    assert_eq!(store.list_channels(created.guild.id).unwrap().len(), 1);
    let messages: Vec<_> = store
        .history(created.guild.id, created.general.id, 50)
        .unwrap()
        .into_iter()
        .filter(|event| matches!(event.body.kind, EventKind::Message { .. }))
        .collect();
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].body.kind,
        EventKind::Message {
            body: "persisted".to_string()
        }
    );
}

#[test]
fn history_limit_returns_latest_messages() {
    let alice = identity(8);
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let created = store.create_guild("limits", &alice).unwrap();
    for index in 0..10 {
        store
            .post_message(
                created.guild.id,
                created.general.id,
                &format!("message {index}"),
                &alice,
            )
            .unwrap();
    }

    let messages: Vec<_> = store
        .history(created.guild.id, created.general.id, 3)
        .unwrap()
        .into_iter()
        .filter(|event| matches!(event.body.kind, EventKind::Message { .. }))
        .collect();
    assert_eq!(messages.len(), 3);
    assert_eq!(
        messages[0].body.kind,
        EventKind::Message {
            body: "message 7".to_string()
        }
    );
    assert_eq!(
        messages[2].body.kind,
        EventKind::Message {
            body: "message 9".to_string()
        }
    );
}

#[test]
fn event_serialization_roundtrip() {
    let alice = identity(9);
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let created = store.create_guild("serialization", &alice).unwrap();
    let event = store
        .post_message(created.guild.id, created.general.id, "roundtrip", &alice)
        .unwrap();
    let bytes = event.to_bytes().unwrap();
    let decoded = ovvc_core::SignedEvent::from_bytes(&bytes).unwrap();
    assert_eq!(event, decoded);
    assert_eq!(event.id().unwrap(), decoded.id().unwrap());
}

#[test]
fn invite_roundtrip_and_tamper_detection() {
    let alice = identity(13);
    let guild_id = ovvc_core::GuildId([42u8; 32]);
    let invite = ovvc_core::Invite::new(
        guild_id,
        "Invite Guild".to_string(),
        &alice,
        vec!["/ip4/127.0.0.1/tcp/7000/p2p/12D3KooWExample".to_string()],
        None,
    )
    .unwrap();

    invite.verify().unwrap();
    let encoded = invite.encode().unwrap();
    let decoded = ovvc_core::Invite::decode(&encoded).unwrap();
    assert_eq!(decoded, invite);
    decoded.verify().unwrap();

    let mut tampered = decoded;
    tampered.body.guild_name = "Evil Guild".to_string();
    assert!(tampered.verify().is_err());
}

#[test]
fn expired_invites_are_rejected() {
    let alice = identity(14);
    let guild_id = ovvc_core::GuildId([43u8; 32]);
    let now = ovvc_core::model::now_unix();
    let invite = ovvc_core::Invite::new(
        guild_id,
        "Old Guild".to_string(),
        &alice,
        vec!["/ip4/127.0.0.1/tcp/7000".to_string()],
        Some(now.saturating_sub(1)),
    )
    .unwrap();

    assert!(invite.verify().is_err());
}

/// Two authors' events can share a Lamport value. Paging by Lamport alone
/// drops or repeats one of them; the `(lamport, event_id)` cursor must return
/// every event exactly once even with a page size of one.
#[test]
fn events_after_pages_across_a_lamport_tie() {
    let alice = identity(20);
    let bob = identity(21);
    let alice_dir = tempdir().unwrap();
    let bob_dir = tempdir().unwrap();

    let mut alice_store = Store::open(alice_dir.path()).unwrap();
    let created = alice_store.create_guild("ties", &alice).unwrap();

    // Give Bob the guild and channel through the same import path the network
    // uses, without needing a transport.
    let seed = alice_store
        .events_after(created.guild.id, SyncCursor::start(), 100, usize::MAX)
        .unwrap()
        .events;
    let mut bob_store = Store::open(bob_dir.path()).unwrap();
    for event in seed {
        assert!(bob_store.import_event(event).unwrap());
    }

    let a = alice_store
        .post_message(created.guild.id, created.general.id, "from alice", &alice)
        .unwrap();
    let b = bob_store
        .post_message(created.guild.id, created.general.id, "from bob", &bob)
        .unwrap();
    assert_eq!(
        a.body.lamport, b.body.lamport,
        "fixture must produce a Lamport tie"
    );

    alice_store.import_event(b).unwrap();
    bob_store.import_event(a).unwrap();

    let expected = 4; // guild, channel, and the two tied messages
    for store in [&alice_store, &bob_store] {
        let mut cursor = SyncCursor::start();
        let mut seen = Vec::new();
        loop {
            let page = store
                .events_after(created.guild.id, cursor, 1, usize::MAX)
                .unwrap();
            assert!(page.events.len() <= 1, "page size was not honoured");
            if let Some(last) = page.events.last() {
                seen.push(last.id().unwrap());
                cursor = SyncCursor::after(last).unwrap();
            }
            if !page.more {
                break;
            }
            assert!(
                !page.events.is_empty(),
                "a page that reports `more` must make progress"
            );
        }
        assert_eq!(seen.len(), expected, "paging lost or repeated events");
        let unique: HashSet<EventId> = seen.iter().copied().collect();
        assert_eq!(unique.len(), expected, "paging returned duplicates");
    }
}

/// Backwards paging must also survive Lamport ties. A bare Lamport cursor
/// would either repeat or skip the tied pair when the page boundary lands in
/// the middle of it.
#[test]
fn history_before_pages_backwards_across_a_lamport_tie() {
    let alice = identity(22);
    let bob = identity(23);
    let alice_dir = tempdir().unwrap();
    let bob_dir = tempdir().unwrap();

    let mut alice_store = Store::open(alice_dir.path()).unwrap();
    let created = alice_store.create_guild("reverse ties", &alice).unwrap();
    let seed = alice_store
        .events_after(created.guild.id, SyncCursor::start(), 100, usize::MAX)
        .unwrap()
        .events;
    let mut bob_store = Store::open(bob_dir.path()).unwrap();
    for event in seed {
        bob_store.import_event(event).unwrap();
    }

    let a = alice_store
        .post_message(created.guild.id, created.general.id, "from alice", &alice)
        .unwrap();
    let b = bob_store
        .post_message(created.guild.id, created.general.id, "from bob", &bob)
        .unwrap();
    alice_store.import_event(b).unwrap();
    bob_store.import_event(a).unwrap();

    let full: Vec<EventId> = alice_store
        .history(created.guild.id, created.general.id, 100)
        .unwrap()
        .iter()
        .map(|event| event.id().unwrap())
        .collect();
    assert!(full.len() >= 2);

    let newest = alice_store
        .history_before(created.guild.id, created.general.id, None, 1)
        .unwrap();
    assert_eq!(newest.events.len(), 1);
    assert!(newest.more);
    assert_eq!(newest.events[0].id().unwrap(), *full.last().unwrap());

    let mut cursor = None;
    let mut seen = Vec::new();
    loop {
        let page = alice_store
            .history_before(created.guild.id, created.general.id, cursor, 1)
            .unwrap();
        assert!(page.events.len() <= 1, "page size was not honoured");
        if let Some(event) = page.events.first() {
            seen.insert(0, event.id().unwrap());
            cursor = Some(SyncCursor::at(event).unwrap());
        }
        if !page.more {
            break;
        }
        assert!(
            !page.events.is_empty(),
            "a page that reports `more` must make progress"
        );
    }
    assert_eq!(
        seen, full,
        "backwards paging lost, repeated or reordered events"
    );
}

/// A crash can leave a length prefix and a partial payload at the tail of the
/// last segment. Reopening must truncate the partial record so later appends
/// are readable; otherwise every subsequent message is silently lost.
#[test]
fn torn_tail_is_repaired_and_later_appends_survive() {
    let alice = identity(22);
    let dir = tempdir().unwrap();
    let created = {
        let mut store = Store::open(dir.path()).unwrap();
        let created = store.create_guild("torn", &alice).unwrap();
        store
            .post_message(created.guild.id, created.general.id, "before crash", &alice)
            .unwrap();
        created
    };

    let segment = dir
        .path()
        .join("guilds")
        .join(created.guild.id.to_hex())
        .join("events")
        .join("00000001.seg");
    let clean_len = fs::metadata(&segment).unwrap().len();
    {
        let mut file = fs::OpenOptions::new().append(true).open(&segment).unwrap();
        // Length prefix says 64 bytes; only 5 follow. This is exactly what a
        // power loss during `write_all` can leave behind.
        file.write_all(&64u32.to_le_bytes()).unwrap();
        file.write_all(b"short").unwrap();
        file.sync_all().unwrap();
    }
    assert!(fs::metadata(&segment).unwrap().len() > clean_len);

    let mut store = Store::open(dir.path()).unwrap();
    let bodies: Vec<String> = store
        .history(created.guild.id, created.general.id, 50)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.body.kind {
            EventKind::Message { body } => Some(body),
            _ => None,
        })
        .collect();
    assert_eq!(bodies, vec!["before crash".to_string()]);
    assert_eq!(
        fs::metadata(&segment).unwrap().len(),
        clean_len,
        "the torn record must be truncated away"
    );

    // A new append after the repair must be durable and visible next open.
    store
        .post_message(created.guild.id, created.general.id, "after repair", &alice)
        .unwrap();
    drop(store);

    let store = Store::open(dir.path()).unwrap();
    let bodies: Vec<String> = store
        .history(created.guild.id, created.general.id, 50)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.body.kind {
            EventKind::Message { body } => Some(body),
            _ => None,
        })
        .collect();
    assert!(bodies.contains(&"before crash".to_string()));
    assert!(
        bodies.contains(&"after repair".to_string()),
        "append after repair was lost: {bodies:?}"
    );
}

/// A crash can leave a full-length but zero-filled record at the tail (the
/// filesystem commits the size before the data). It must be truncated like any
/// other torn record instead of being indexed as an event.
#[test]
fn zero_filled_tail_is_treated_as_torn() {
    let alice = identity(29);
    let dir = tempdir().unwrap();
    let created = {
        let mut store = Store::open(dir.path()).unwrap();
        let created = store.create_guild("zeroed", &alice).unwrap();
        store
            .post_message(created.guild.id, created.general.id, "kept", &alice)
            .unwrap();
        created
    };

    let segment = dir
        .path()
        .join("guilds")
        .join(created.guild.id.to_hex())
        .join("events")
        .join("00000001.seg");
    let clean_len = fs::metadata(&segment).unwrap().len();
    {
        let mut file = fs::OpenOptions::new().append(true).open(&segment).unwrap();
        file.write_all(&64u32.to_le_bytes()).unwrap();
        file.write_all(&[0u8; 64]).unwrap();
        file.sync_all().unwrap();
    }

    let store = Store::open(dir.path()).unwrap();
    let bodies: Vec<String> = store
        .history(created.guild.id, created.general.id, 50)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.body.kind {
            EventKind::Message { body } => Some(body),
            _ => None,
        })
        .collect();
    assert_eq!(bodies, vec!["kept".to_string()]);
    assert_eq!(
        fs::metadata(&segment).unwrap().len(),
        clean_len,
        "the zero-filled record must be truncated away"
    );
}

/// A lost guild directory must not leave index rows alive just because
/// another guild happens to have a record at the same location. Locations are
/// unique only within one guild's log, so a row kept by collision would make
/// every later read fail on a missing segment file.
#[test]
fn a_lost_guild_does_not_keep_its_index_rows_alive() {
    let alice = identity(30);
    let dir = tempdir().unwrap();
    let (kept_guild, lost_guild, lost_event) = {
        let mut store = Store::open(dir.path()).unwrap();
        let kept_guild = store.create_guild("guild-a", &alice).unwrap();
        let lost_guild = store.create_guild("guild-b", &alice).unwrap();
        // Equal-length names and bodies make the two logs collide on record
        // locations, which is what made a location-only validity check wrong.
        let lost_event = store
            .post_message(lost_guild.guild.id, lost_guild.general.id, "gone", &alice)
            .unwrap();
        store
            .post_message(kept_guild.guild.id, kept_guild.general.id, "kept", &alice)
            .unwrap();
        (kept_guild, lost_guild, lost_event)
    };

    fs::remove_dir_all(
        dir.path()
            .join("guilds")
            .join(lost_guild.guild.id.to_hex())
            .join("events"),
    )
    .unwrap();

    let store = Store::open(dir.path()).unwrap();
    assert!(!store.has_event(lost_event.id().unwrap()).unwrap());
    assert!(
        store
            .history(lost_guild.guild.id, lost_guild.general.id, 50)
            .unwrap()
            .is_empty()
    );
    let kept: Vec<String> = store
        .history(kept_guild.guild.id, kept_guild.general.id, 50)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.body.kind {
            EventKind::Message { body } => Some(body),
            _ => None,
        })
        .collect();
    assert_eq!(kept, vec!["kept".to_string()]);
}

/// A tail record can decode structurally and still fail signature
/// verification (bit rot, or a payload persisted with a corrupted signature).
/// It must be treated as torn; otherwise recovery fails and the store can
/// never be opened again.
#[test]
fn a_corrupt_signature_at_the_tail_is_truncated_on_open() {
    let alice = identity(31);
    let dir = tempdir().unwrap();
    let created = {
        let mut store = Store::open(dir.path()).unwrap();
        let created = store.create_guild("bitrot", &alice).unwrap();
        store
            .post_message(created.guild.id, created.general.id, "kept", &alice)
            .unwrap();
        store
            .post_message(created.guild.id, created.general.id, "corrupt", &alice)
            .unwrap();
        created
    };

    let segment = dir
        .path()
        .join("guilds")
        .join(created.guild.id.to_hex())
        .join("events")
        .join("00000001.seg");
    let mut bytes = fs::read(&segment).unwrap();
    // The postcard payload ends with the signature, so the final byte is part
    // of it. Flipping it leaves a decodable, unverifiable record.
    *bytes.last_mut().unwrap() ^= 0xff;
    fs::write(&segment, &bytes).unwrap();

    let store = Store::open(dir.path()).unwrap();
    let bodies: Vec<String> = store
        .history(created.guild.id, created.general.id, 50)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.body.kind {
            EventKind::Message { body } => Some(body),
            _ => None,
        })
        .collect();
    assert_eq!(bodies, vec!["kept".to_string()]);
}

/// Imported events are as untrusted as any other network input. They must be
/// rejected when they reference a channel the guild does not have, when they
/// come from the far future, or when they claim an impossible sequence.
#[test]
fn imported_events_are_validated() {
    let alice = identity(23);
    let mallory = identity(24);
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let created = store.create_guild("validated", &alice).unwrap();
    let lamport = store.guild_lamport(created.guild.id).unwrap() + 1;

    let base = EventBody {
        version: 1,
        guild_id: created.guild.id,
        channel_id: Some(created.general.id),
        author: mallory.author_id(),
        seq: 1,
        lamport,
        timestamp: now_unix(),
        // A non-first event always names its predecessor.
        prev: Some(EventId([7u8; 32])),
        kind: EventKind::Message {
            body: "hello".to_string(),
        },
    };

    let unknown_channel = EventBody {
        channel_id: Some(ovvc_core::ChannelId([9u8; 16])),
        ..base.clone()
    };
    assert!(
        store
            .import_event(mallory.sign_body(unknown_channel).unwrap())
            .is_err(),
        "an event for an unknown channel must be rejected"
    );

    let future = EventBody {
        timestamp: now_unix() + 7 * 24 * 3600,
        ..base.clone()
    };
    assert!(
        store
            .import_event(mallory.sign_body(future).unwrap())
            .is_err(),
        "an event from the far future must be rejected"
    );

    let zero_lamport = EventBody {
        lamport: 0,
        ..base.clone()
    };
    assert!(
        store
            .import_event(mallory.sign_body(zero_lamport).unwrap())
            .is_err(),
        "an event with a zero Lamport clock must be rejected"
    );

    // The same body, correctly signed, imports exactly once.
    let event: SignedEvent = mallory.sign_body(base).unwrap();
    assert!(store.import_event(event.clone()).unwrap());
    assert!(!store.import_event(event).unwrap());
}

/// The first event a store ever sees for a guild defines its clock. It must be
/// the guild's creation with a sane Lamport value; otherwise one peer could
/// park the clock at `u64::MAX` and no local event could ever be ordered after
/// it.
#[test]
fn the_first_event_for_a_guild_must_be_its_creation() {
    let mallory = identity(27);
    let guild_id = ovvc_core::GuildId([44u8; 32]);
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();

    let message = EventBody {
        version: 1,
        guild_id,
        channel_id: Some(ovvc_core::ChannelId([1u8; 16])),
        author: mallory.author_id(),
        seq: 1,
        lamport: 1,
        timestamp: now_unix(),
        prev: Some(EventId([8u8; 32])),
        kind: EventKind::Message {
            body: "first!".to_string(),
        },
    };
    assert!(
        store
            .import_event(mallory.sign_body(message).unwrap())
            .is_err(),
        "a message cannot be the first event for a guild"
    );

    let poisoned = EventBody {
        version: 1,
        guild_id,
        channel_id: None,
        author: mallory.author_id(),
        seq: 0,
        lamport: u64::MAX / 2,
        timestamp: now_unix(),
        prev: None,
        kind: EventKind::GuildCreated {
            name: "Poisoned".to_string(),
        },
    };
    assert!(
        store
            .import_event(mallory.sign_body(poisoned).unwrap())
            .is_err(),
        "the first event cannot claim an absurd Lamport clock"
    );

    let created = EventBody {
        version: 1,
        guild_id,
        channel_id: None,
        author: mallory.author_id(),
        seq: 0,
        lamport: 1,
        timestamp: now_unix(),
        prev: None,
        kind: EventKind::GuildCreated {
            name: "Real".to_string(),
        },
    };
    assert!(
        store
            .import_event(mallory.sign_body(created).unwrap())
            .unwrap()
    );
    assert_eq!(store.guild_lamport(guild_id).unwrap(), 1);
}

/// Creating a channel returns the signed event that created it, so the client
/// can broadcast it and online peers see the channel immediately.
#[test]
fn create_channel_with_event_returns_a_signed_channel_event() {
    let alice = identity(28);
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let created = store.create_guild("channels", &alice).unwrap();

    let (channel, event) = store
        .create_channel_with_event(created.guild.id, "random", &alice)
        .unwrap();
    let event = event.expect("the concrete store records a channel event");
    event.verify().unwrap();
    assert_eq!(event.body.channel_id, Some(channel.id));
    assert_eq!(
        event.body.kind,
        EventKind::ChannelCreated {
            name: "random".to_string()
        }
    );
}

/// Renaming is a normal event: it must be signed, broadcastable, applied to
/// the roster, and a no-op when nothing changed.
#[test]
fn display_name_change_is_event_backed_and_idempotent() {
    let alice = identity(25);
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let created = store.create_guild("names", &alice).unwrap();

    let event = store
        .set_display_name(created.guild.id, "Alice", &alice)
        .unwrap()
        .expect("a changed name emits an event");
    assert_eq!(
        event.body.kind,
        EventKind::MemberRenamed {
            display_name: "Alice".to_string()
        }
    );
    event.verify().unwrap();

    assert!(
        store
            .set_display_name(created.guild.id, "Alice", &alice)
            .unwrap()
            .is_none(),
        "setting the same name again must not append an event"
    );

    let member = store
        .list_members(created.guild.id)
        .unwrap()
        .into_iter()
        .find(|member| member.author == alice.author_id())
        .expect("alice is a member");
    assert_eq!(member.display_name, "Alice");
}

fn guild_events_dir(root: &std::path::Path, guild_id: ovvc_core::GuildId) -> std::path::PathBuf {
    root.join("guilds").join(guild_id.to_hex()).join("events")
}

const SEGMENT_MAGIC: &[u8; 8] = b"OVVCSG01";
const RECORD_CHECKSUM_FLAG: u32 = 0x8000_0000;

/// One record in a segment file, with the offsets a test needs to corrupt or
/// split it without duplicating the on-disk framing rules everywhere.
struct SegmentRecord {
    /// Offset of the 4-byte record header.
    header: usize,
    /// Offset of the first payload byte.
    payload_start: usize,
    len: usize,
    checksummed: bool,
}

fn segment_records(bytes: &[u8]) -> Vec<SegmentRecord> {
    assert_eq!(&bytes[..8], SEGMENT_MAGIC, "test fixture lost its header");
    let mut records = Vec::new();
    let mut pos = 8;
    while pos < bytes.len() {
        assert!(pos + 4 <= bytes.len(), "truncated test fixture");
        let raw = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
        let checksummed = raw & RECORD_CHECKSUM_FLAG != 0;
        let len = (raw & !RECORD_CHECKSUM_FLAG) as usize;
        let payload_start = pos + 4;
        let record_end = payload_start + len + if checksummed { 4 } else { 0 };
        assert!(record_end <= bytes.len(), "truncated test fixture");
        records.push(SegmentRecord {
            header: pos,
            payload_start,
            len,
            checksummed,
        });
        pos = record_end;
    }
    records
}

fn message_bodies(store: &Store, guild_id: GuildId, channel_id: ChannelId) -> Vec<String> {
    store
        .history(guild_id, channel_id, 100)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.body.kind {
            EventKind::Message { body } => Some(body),
            _ => None,
        })
        .collect()
}

/// A record on disk gets the same validation as a peer import. A validly signed
/// event with an absurd Lamport clock must not be able to poison the guild
/// clock just because it was written into the segment file.
#[test]
fn recovery_rejects_records_that_would_fail_import() {
    let alice = identity(40);
    let mallory = identity(41);
    let dir = tempdir().unwrap();
    let created = {
        let mut store = Store::open(dir.path()).unwrap();
        store.create_guild("recovery-validation", &alice).unwrap()
    };

    let poison = EventBody {
        version: 1,
        guild_id: created.guild.id,
        channel_id: None,
        author: mallory.author_id(),
        seq: 0,
        lamport: u64::MAX,
        timestamp: now_unix(),
        prev: None,
        kind: EventKind::GuildCreated {
            name: "pwned".to_string(),
        },
    };
    let poison = mallory.sign_body(poison).unwrap().to_bytes().unwrap();

    let segment = guild_events_dir(dir.path(), created.guild.id).join("00000001.seg");
    let mut bytes = fs::read(&segment).unwrap();
    bytes.extend_from_slice(&(poison.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&poison);
    fs::write(&segment, bytes).unwrap();

    assert!(
        Store::open(dir.path()).is_err(),
        "recovery must reject a log record that import_event would reject"
    );
}

/// Reads are canonicalised through `GuildId::to_hex()`. A directory with any
/// other spelling must be rejected at open instead of being silently
/// unreachable and then forked by the next post.
#[test]
fn non_canonical_guild_directory_is_rejected() {
    let alice = identity(42);
    let dir = tempdir().unwrap();
    let created = {
        let mut store = Store::open(dir.path()).unwrap();
        store.create_guild("canonical", &alice).unwrap()
    };
    let lower = dir.path().join("guilds").join(created.guild.id.to_hex());
    let upper = dir
        .path()
        .join("guilds")
        .join(created.guild.id.to_hex().to_uppercase());
    fs::rename(lower, upper).unwrap();

    assert!(Store::open(dir.path()).is_err());
}

/// Segment filenames are part of the addressing scheme. A renamed segment that
/// parses but is not the canonical `%08d.seg` would be scanned while appends
/// create a different file, forking the log.
#[test]
fn non_canonical_segment_filename_is_rejected() {
    let alice = identity(43);
    let dir = tempdir().unwrap();
    let created = {
        let mut store = Store::open(dir.path()).unwrap();
        store.create_guild("segments", &alice).unwrap()
    };
    let events = guild_events_dir(dir.path(), created.guild.id);
    fs::rename(events.join("00000001.seg"), events.join("1.seg")).unwrap();

    assert!(Store::open(dir.path()).is_err());
}

/// A sparse file can make a multi-megabyte record prefix fit inside the file.
/// The scanner must cap the allocation and repair the tail rather than trying
/// to reserve the declared length.
#[test]
fn oversized_declared_record_is_truncated_without_allocating() {
    let alice = identity(44);
    let dir = tempdir().unwrap();
    let created = {
        let mut store = Store::open(dir.path()).unwrap();
        store.create_guild("sparse", &alice).unwrap()
    };
    let segment = guild_events_dir(dir.path(), created.guild.id).join("00000001.seg");
    let original_len = fs::metadata(&segment).unwrap().len();
    let declared: u32 = 64 * 1024 * 1024;

    let file = fs::OpenOptions::new().append(true).open(&segment).unwrap();
    let mut file = file;
    file.write_all(&declared.to_le_bytes()).unwrap();
    file.set_len(original_len + 4 + u64::from(declared))
        .unwrap();
    drop(file);

    let store = Store::open(dir.path()).expect("a torn huge tail is repairable");
    assert_eq!(fs::metadata(&segment).unwrap().len(), original_len);
    assert_eq!(store.list_guilds().unwrap().len(), 1);
}

/// `get_event` is keyed by a global event id but reads from a guild-local
/// location. Every guild's first records share offsets, so without checking the
/// returned id and guild it can return another guild's event.
#[test]
fn get_event_does_not_return_another_guilds_event() {
    let alice = identity(45);
    let bob = identity(46);
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    // Equal-length names keep the two guilds' record layouts identical.
    let a = store.create_guild("aaaa", &alice).unwrap();
    let b = store.create_guild("bbbb", &bob).unwrap();

    let b_event = store
        .history(b.guild.id, b.general.id, 10)
        .unwrap()
        .into_iter()
        .next()
        .expect("b has a channel creation event");
    let b_event_id = b_event.id().unwrap();

    assert!(
        store.get_event(a.guild.id, b_event_id).unwrap().is_none(),
        "a guild must not resolve an event id that belongs to another guild"
    );
    let found = store
        .get_event(b.guild.id, b_event_id)
        .unwrap()
        .expect("the owning guild resolves its own event");
    assert_eq!(found.id().unwrap(), b_event_id);
}

/// Symlinked segments and guild directories are not part of the store format.
/// Following them would let a modified data directory truncate an outside file
/// or silently drop a guild from the recovered index.
#[cfg(unix)]
#[test]
fn symlinked_store_entries_are_rejected() {
    use std::os::unix::fs::symlink;

    let alice = identity(47);
    let dir = tempdir().unwrap();
    let created = {
        let mut store = Store::open(dir.path()).unwrap();
        store.create_guild("symlinks", &alice).unwrap()
    };

    let outside = dir.path().join("outside.bin");
    fs::write(&outside, b"not a segment").unwrap();
    let events = guild_events_dir(dir.path(), created.guild.id);
    symlink(&outside, events.join("00000002.seg")).unwrap();
    assert!(
        Store::open(dir.path()).is_err(),
        "a symlinked segment must be rejected"
    );
    assert_eq!(fs::read(&outside).unwrap(), b"not a segment");
}

#[cfg(unix)]
#[test]
fn symlinked_guild_directory_is_rejected() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let target = dir.path().join("elsewhere");
    fs::create_dir(&target).unwrap();
    let guilds = dir.path().join("guilds");
    fs::create_dir(&guilds).unwrap();
    let name = "11".repeat(32);
    symlink(&target, guilds.join(name)).unwrap();

    assert!(Store::open(dir.path()).is_err());
}

/// Event text is rendered by a terminal, so control and bidi-format
/// characters must not be storable; and a name's *raw* length is what matters,
/// not its length after trimming (otherwise a peer can store megabytes).
#[test]
fn imported_text_rejects_terminal_escapes_and_raw_oversize() {
    let alice = identity(41);
    let mallory = identity(42);
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let created = store.create_guild("clean", &alice).unwrap();
    let lamport = store.guild_lamport(created.guild.id).unwrap() + 1;

    let message = |text: &str| EventBody {
        version: 1,
        guild_id: created.guild.id,
        channel_id: Some(created.general.id),
        author: mallory.author_id(),
        seq: 1,
        lamport,
        timestamp: now_unix(),
        prev: Some(EventId([3u8; 32])),
        kind: EventKind::Message {
            body: text.to_string(),
        },
    };

    for text in ["\x1b]0;pwned\x07", "line one\nFAKE-LINE", "\u{202e}gnp.exe"] {
        assert!(
            store
                .import_event(mallory.sign_body(message(text)).unwrap())
                .is_err(),
            "message {text:?} must be rejected"
        );
    }

    let long_name = format!("{}{}", " ".repeat(50), "x".repeat(51));
    let oversized = EventBody {
        version: 1,
        guild_id: created.guild.id,
        channel_id: None,
        author: mallory.author_id(),
        seq: 1,
        lamport,
        timestamp: now_unix(),
        prev: Some(EventId([4u8; 32])),
        kind: EventKind::MemberJoined {
            display_name: long_name,
        },
    };
    assert!(
        store
            .import_event(mallory.sign_body(oversized).unwrap())
            .is_err(),
        "a name whose raw encoding exceeds the limit must be rejected"
    );

    // A valid name is stored trimmed, so surrounding whitespace cannot be used
    // to smuggle a different display value.
    let padded = EventBody {
        version: 1,
        guild_id: created.guild.id,
        channel_id: None,
        author: mallory.author_id(),
        seq: 1,
        lamport,
        timestamp: now_unix(),
        prev: Some(EventId([5u8; 32])),
        kind: EventKind::MemberJoined {
            display_name: "  bob  ".to_string(),
        },
    };
    store
        .import_event(mallory.sign_body(padded).unwrap())
        .unwrap();
    let member = store
        .list_members(created.guild.id)
        .unwrap()
        .into_iter()
        .find(|member| member.author == mallory.author_id())
        .unwrap();
    assert_eq!(member.display_name, "bob");
}

#[cfg(unix)]
#[test]
fn store_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let alice = identity(43);
    let dir = tempdir().unwrap();
    let created = {
        let mut store = Store::open(dir.path()).unwrap();
        let created = store.create_guild("private", &alice).unwrap();
        store
            .post_message(created.guild.id, created.general.id, "hello", &alice)
            .unwrap();
        created
    };

    let mode = |path: &std::path::Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(dir.path()), 0o700);
    assert_eq!(mode(&dir.path().join("index.redb")), 0o600);
    let events = guild_events_dir(dir.path(), created.guild.id);
    for entry in fs::read_dir(&events).unwrap() {
        assert_eq!(mode(&entry.unwrap().path()), 0o600);
    }
}

/// Two peers that receive the same events in different orders must agree on
/// guild, channel and member names. Metadata rows are owned by a
/// `(lamport, event_id)` tuple; a late, older event may not overwrite a newer
/// one.
#[test]
fn metadata_converges_regardless_of_arrival_order() {
    let alice = identity(51);
    let dir = tempdir().unwrap();
    let (guild, channel, events) = {
        let mut store = Store::open(dir.path()).unwrap();
        let created = store.create_guild("first", &alice).unwrap();
        store
            .set_display_name(created.guild.id, "old name", &alice)
            .unwrap();
        store
            .set_display_name(created.guild.id, "new name", &alice)
            .unwrap();
        let page = store
            .events_after(created.guild.id, SyncCursor::start(), 100, 1 << 20)
            .unwrap();
        (created.guild.id, created.general.id, page.events)
    };
    assert_eq!(events.len(), 4, "guild, channel, and two profile events");

    // A later rename by the founder, and an older one that arrives after it.
    let rename = alice
        .sign_body(EventBody {
            version: 1,
            guild_id: guild,
            channel_id: None,
            author: alice.author_id(),
            seq: 100,
            lamport: 100,
            timestamp: now_unix(),
            prev: Some(EventId([1u8; 32])),
            kind: EventKind::GuildCreated {
                name: "renamed".to_string(),
            },
        })
        .unwrap();
    let stale = alice
        .sign_body(EventBody {
            version: 1,
            guild_id: guild,
            channel_id: None,
            author: alice.author_id(),
            seq: 101,
            lamport: 50,
            timestamp: now_unix(),
            prev: Some(EventId([2u8; 32])),
            kind: EventKind::GuildCreated {
                name: "stale".to_string(),
            },
        })
        .unwrap();

    let mut orders = Vec::new();
    let mut first = events.clone();
    first.push(rename.clone());
    first.push(stale.clone());
    orders.push(first);

    let second = vec![
        events[0].clone(),
        rename.clone(),
        events[1].clone(),
        events[3].clone(),
        events[2].clone(),
        stale.clone(),
    ];
    orders.push(second);

    for (index, order) in orders.into_iter().enumerate() {
        let dir = tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        for event in order {
            store.import_event(event).unwrap();
        }
        assert_eq!(
            store.get_guild(guild).unwrap().unwrap().name,
            "renamed",
            "order {index}: the newest guild creation must win"
        );
        let channel_meta = store.get_channel(guild, channel).unwrap().unwrap();
        assert_eq!(channel_meta.name, "general", "order {index}");
        let member = store
            .list_members(guild)
            .unwrap()
            .into_iter()
            .find(|member| member.author == alice.author_id())
            .unwrap();
        assert_eq!(
            member.display_name, "new name",
            "order {index}: the newest profile event must win"
        );
    }
}

/// A leave removes the roster row and leaves a tombstone: a later-arriving
/// event from before the leave must not recreate the member, while a newer
/// join must.
#[test]
fn leaving_tombstones_the_roster_row_until_a_newer_join() {
    let alice = identity(52);
    let dir = tempdir().unwrap();
    let (guild, channel, mut events) = {
        let mut store = Store::open(dir.path()).unwrap();
        let created = store.create_guild("leavers", &alice).unwrap();
        store
            .set_display_name(created.guild.id, "Alice", &alice)
            .unwrap();
        let page = store
            .events_after(created.guild.id, SyncCursor::start(), 100, 1 << 20)
            .unwrap();
        (created.guild.id, created.general.id, page.events)
    };
    assert_eq!(events.len(), 3);

    let leave = alice
        .sign_body(EventBody {
            version: 1,
            guild_id: guild,
            channel_id: None,
            author: alice.author_id(),
            seq: 10,
            lamport: 20,
            timestamp: now_unix(),
            prev: Some(EventId([6u8; 32])),
            kind: EventKind::MemberLeft,
        })
        .unwrap();
    let old_message = alice
        .sign_body(EventBody {
            version: 1,
            guild_id: guild,
            channel_id: Some(channel),
            author: alice.author_id(),
            seq: 9,
            lamport: 5,
            timestamp: now_unix(),
            prev: Some(EventId([7u8; 32])),
            kind: EventKind::Message {
                body: "sent before leaving".to_string(),
            },
        })
        .unwrap();
    let rejoin = alice
        .sign_body(EventBody {
            version: 1,
            guild_id: guild,
            channel_id: None,
            author: alice.author_id(),
            seq: 30,
            lamport: 40,
            timestamp: now_unix(),
            prev: Some(EventId([8u8; 32])),
            kind: EventKind::MemberJoined {
                display_name: "Alice again".to_string(),
            },
        })
        .unwrap();

    events.push(leave);
    let second = tempdir().unwrap();
    let mut store = Store::open(second.path()).unwrap();
    for event in events {
        store.import_event(event).unwrap();
    }
    assert!(
        !store
            .list_members(guild)
            .unwrap()
            .iter()
            .any(|member| member.author == alice.author_id()),
        "a leave must remove the roster row"
    );

    store.import_event(old_message).unwrap();
    assert!(
        !store
            .list_members(guild)
            .unwrap()
            .iter()
            .any(|member| member.author == alice.author_id()),
        "an older message must not resurrect a member who left"
    );

    store.import_event(rejoin).unwrap();
    let member = store
        .list_members(guild)
        .unwrap()
        .into_iter()
        .find(|member| member.author == alice.author_id())
        .expect("a newer join re-adds the member");
    assert_eq!(member.display_name, "Alice again");
}

#[test]
fn edits_and_deletes_are_authorized_and_fold() {
    let alice = identity(31);
    let mallory = identity(32);
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let created = store.create_guild("edits", &alice).unwrap();
    let message = store
        .post_message(created.guild.id, created.general.id, "first draft", &alice)
        .unwrap();
    let target = message.id().unwrap();

    // Only the original author may modify a message.
    assert!(
        store
            .edit_message(created.guild.id, target, "hijacked", &mallory)
            .is_err()
    );
    assert!(
        store
            .delete_message(created.guild.id, target, &mallory)
            .is_err()
    );

    // Edits obey the same text rules as messages.
    assert!(
        store
            .edit_message(created.guild.id, target, "   ", &alice)
            .is_err()
    );
    assert!(
        store
            .edit_message(created.guild.id, target, "boom\u{1b}[2J", &alice)
            .is_err()
    );
    // Targeting an event that is not a message is rejected.
    assert!(
        store
            .edit_message(created.guild.id, EventId([0u8; 32]), "nope", &alice)
            .is_err()
    );

    let edit = store
        .edit_message(created.guild.id, target, "final draft", &alice)
        .unwrap();
    assert!(matches!(
        edit.body.kind,
        EventKind::MessageEdited { target: id, .. } if id == target
    ));

    let history = store
        .history(created.guild.id, created.general.id, 50)
        .unwrap();
    let entries = ovvc_core::timeline(&history).unwrap();
    let folded = entries
        .iter()
        .find_map(|entry| match entry {
            ovvc_core::TimelineEntry::Message {
                event,
                body,
                edited,
                ..
            } if event.id().unwrap() == target => Some((body.as_str(), *edited)),
            _ => None,
        })
        .expect("the message is still visible");
    assert_eq!(folded, ("final draft", true));

    store
        .delete_message(created.guild.id, target, &alice)
        .unwrap();
    let history = store
        .history(created.guild.id, created.general.id, 50)
        .unwrap();
    let entries = ovvc_core::timeline(&history).unwrap();
    assert!(entries.iter().any(|entry| matches!(
        entry,
        ovvc_core::TimelineEntry::Deleted { event } if event.id().unwrap() == target
    )));
}

#[test]
fn a_forged_edit_that_arrives_before_its_target_never_applies() {
    let alice = identity(33);
    let mallory = identity(34);
    let dir_a = tempdir().unwrap();
    let dir_b = tempdir().unwrap();

    let (created, message, events) = {
        let mut a = Store::open(dir_a.path()).unwrap();
        let created = a.create_guild("forgery", &alice).unwrap();
        let message = a
            .post_message(created.guild.id, created.general.id, "original", &alice)
            .unwrap();
        let events = a
            .events_after(created.guild.id, SyncCursor::start(), 100, usize::MAX)
            .unwrap()
            .events;
        (created, message, events)
    };

    let forged = mallory
        .sign_body(EventBody {
            version: 1,
            guild_id: created.guild.id,
            channel_id: Some(created.general.id),
            author: mallory.author_id(),
            seq: 1,
            lamport: message.body.lamport + 1,
            timestamp: now_unix(),
            prev: Some(EventId([5u8; 32])),
            kind: EventKind::MessageEdited {
                target: message.id().unwrap(),
                body: "pwned".to_string(),
            },
        })
        .unwrap();

    let mut b = Store::open(dir_b.path()).unwrap();
    for event in &events {
        if matches!(
            event.body.kind,
            EventKind::GuildCreated { .. } | EventKind::ChannelCreated { .. }
        ) {
            assert!(b.import_event(event.clone()).unwrap());
        }
    }
    // The edit arrives before the message it claims to edit. Nothing about the
    // target can be checked yet, so the event is stored as an orphan...
    assert!(b.import_event(forged).unwrap());
    // ...and the message arrives later.
    assert!(b.import_event(message).unwrap());

    let history = b.history(created.guild.id, created.general.id, 50).unwrap();
    let entries = ovvc_core::timeline(&history).unwrap();
    let body = entries
        .iter()
        .find_map(|entry| match entry {
            ovvc_core::TimelineEntry::Message { body, .. } => Some(body.as_str()),
            _ => None,
        })
        .expect("the message is visible");
    assert_eq!(body, "original", "a forged orphan edit must never apply");
}

#[test]
fn imported_modifications_must_match_the_target_author_and_channel() {
    let alice = identity(35);
    let mallory = identity(36);
    let dir_a = tempdir().unwrap();
    let dir_b = tempdir().unwrap();

    let (created, message, other_channel, events) = {
        let mut a = Store::open(dir_a.path()).unwrap();
        let created = a.create_guild("moderation", &alice).unwrap();
        let message = a
            .post_message(created.guild.id, created.general.id, "original", &alice)
            .unwrap();
        let other_channel = a.create_channel(created.guild.id, "other", &alice).unwrap();
        let events = a
            .events_after(created.guild.id, SyncCursor::start(), 100, usize::MAX)
            .unwrap()
            .events;
        (created, message, other_channel, events)
    };

    let mut b = Store::open(dir_b.path()).unwrap();
    for event in events {
        b.import_event(event).unwrap();
    }

    let forged = mallory
        .sign_body(EventBody {
            version: 1,
            guild_id: created.guild.id,
            channel_id: Some(message.body.channel_id.unwrap()),
            author: mallory.author_id(),
            seq: 0,
            lamport: b.guild_lamport(created.guild.id).unwrap() + 1,
            timestamp: now_unix(),
            prev: None,
            kind: EventKind::MessageEdited {
                target: message.id().unwrap(),
                body: "pwned".to_string(),
            },
        })
        .unwrap();
    assert!(
        b.import_event(forged).is_err(),
        "an edit by another author must be rejected once the target is known"
    );

    let wrong_channel = alice
        .sign_body(EventBody {
            version: 1,
            guild_id: created.guild.id,
            channel_id: Some(other_channel.id),
            author: alice.author_id(),
            seq: 3,
            lamport: b.guild_lamport(created.guild.id).unwrap() + 1,
            timestamp: now_unix(),
            prev: Some(message.id().unwrap()),
            kind: EventKind::MessageDeleted {
                target: message.id().unwrap(),
            },
        })
        .unwrap();
    assert!(
        b.import_event(wrong_channel).is_err(),
        "a modification indexed under a different channel must be rejected"
    );
}

#[test]
fn legacy_and_unknown_version_two_events_coexist_and_reopen() {
    let alice = identity(37);
    let mallory = identity(38);
    let guild_id = ovvc_core::GuildId([55u8; 32]);
    let channel_id = ovvc_core::ChannelId([66u8; 16]);
    let dir = tempdir().unwrap();

    // Events written by a pre-tag (version 1) binary.
    let creation = alice
        .sign_body(EventBody {
            version: 1,
            guild_id,
            channel_id: None,
            author: alice.author_id(),
            seq: 0,
            lamport: 1,
            timestamp: now_unix(),
            prev: None,
            kind: EventKind::GuildCreated {
                name: "legacy".to_string(),
            },
        })
        .unwrap();
    let channel = alice
        .sign_body(EventBody {
            version: 1,
            guild_id,
            channel_id: Some(channel_id),
            author: alice.author_id(),
            seq: 1,
            lamport: 2,
            timestamp: now_unix(),
            prev: Some(creation.id().unwrap()),
            kind: EventKind::ChannelCreated {
                name: "general".to_string(),
            },
        })
        .unwrap();
    // An event kind this binary does not know, written by a newer peer.
    let unknown = mallory
        .sign_body(EventBody {
            version: ovvc_core::model::EVENT_VERSION,
            guild_id,
            channel_id: Some(channel_id),
            author: mallory.author_id(),
            seq: 0,
            lamport: 3,
            timestamp: now_unix(),
            prev: None,
            kind: EventKind::Unknown {
                tag: 9001,
                payload: vec![7, 8, 9],
            },
        })
        .unwrap();

    {
        let mut store = Store::open(dir.path()).unwrap();
        store.import_event(creation).unwrap();
        store.import_event(channel).unwrap();
        store.import_event(unknown.clone()).unwrap();
        assert_eq!(
            store.get_guild(guild_id).unwrap().unwrap().name,
            "legacy",
            "a version-1 creation event still applies its metadata"
        );
        assert_eq!(store.guild_lamport(guild_id).unwrap(), 3);
    }

    // Reopening scans every record, including the mixed versions.
    let mut store = Store::open(dir.path()).unwrap();
    let stored = store
        .get_event(guild_id, unknown.id().unwrap())
        .unwrap()
        .expect("the unknown event is retained");
    assert_eq!(stored.body.kind, unknown.body.kind);
    assert!(stored.verify().is_ok());

    // Local writes continue on the current version alongside the legacy log.
    let message = store
        .post_message(guild_id, channel_id, "after the upgrade", &alice)
        .unwrap();
    assert_eq!(message.body.version, ovvc_core::model::EVENT_VERSION);
    drop(store);

    let store = Store::open(dir.path()).unwrap();
    let history = store.history(guild_id, channel_id, 50).unwrap();
    assert!(
        history
            .iter()
            .any(|event| event.id().unwrap() == message.id().unwrap())
    );
    assert!(
        history
            .iter()
            .any(|event| matches!(&event.body.kind, EventKind::Unknown { tag: 9001, .. }))
    );
}

#[test]
fn replies_and_mentions_are_stored_folded_and_deduplicated() {
    let alice = identity(51);
    let bob = identity(52);
    let carol = identity(53);
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let created = store.create_guild("threads", &alice).unwrap();
    let original = store
        .post_message(created.guild.id, created.general.id, "first", &alice)
        .unwrap();
    let target = original.id().unwrap();

    let reply = store
        .post_message_full(
            created.guild.id,
            created.general.id,
            "answer @carol",
            Some(target),
            &[carol.author_id(), carol.author_id()],
            &bob,
        )
        .unwrap();
    assert_eq!(
        reply.body.kind,
        EventKind::MessageV2 {
            body: "answer @carol".to_string(),
            reply_to: Some(target),
            // Posting removes duplicates while preserving order.
            mentions: vec![carol.author_id()],
        }
    );
    assert_eq!(reply.body.channel_id, Some(created.general.id));

    // A reply must point at a message in the same channel.
    let other = store
        .create_channel(created.guild.id, "other", &alice)
        .unwrap();
    let elsewhere = store
        .post_message(created.guild.id, other.id, "elsewhere", &alice)
        .unwrap();
    assert!(
        store
            .post_message_full(
                created.guild.id,
                created.general.id,
                "wrong channel",
                Some(elsewhere.id().unwrap()),
                &[],
                &bob,
            )
            .is_err()
    );
    // An id that names no message, or no event at all, is rejected locally.
    assert!(
        store
            .post_message_full(
                created.guild.id,
                created.general.id,
                "nothing",
                Some(EventId([0u8; 32])),
                &[],
                &bob,
            )
            .is_err()
    );
    let too_many: Vec<_> = (0..=ovvc_core::MAX_MENTIONS)
        .map(|index| identity((index % 200) as u8 + 100).author_id())
        .collect();
    assert!(
        store
            .post_message_full(
                created.guild.id,
                created.general.id,
                "everyone",
                None,
                &too_many,
                &bob,
            )
            .is_err()
    );

    // Edits apply to the reply like any other message.
    store
        .edit_message(
            created.guild.id,
            reply.id().unwrap(),
            "answer, edited",
            &bob,
        )
        .unwrap();

    let history = store
        .history(created.guild.id, created.general.id, 50)
        .unwrap();
    let entries = ovvc_core::timeline(&history).unwrap();
    let folded = entries
        .iter()
        .find_map(|entry| match entry {
            ovvc_core::TimelineEntry::Message {
                event,
                body,
                edited,
                reply_to,
                mentions,
                ..
            } if event.id().unwrap() == reply.id().unwrap() => {
                Some((body.as_str(), *edited, reply_to.as_ref(), *mentions))
            }
            _ => None,
        })
        .expect("the reply is visible");
    assert_eq!(folded.0, "answer, edited");
    assert!(folded.1);
    let reference = folded.2.expect("the reply resolves its target");
    assert_eq!(reference.body.as_deref(), Some("first"));
    assert_eq!(reference.event.id().unwrap(), target);
    assert_eq!(folded.3, [carol.author_id()]);
}

#[test]
fn imported_replies_are_checked_against_a_known_target() {
    let alice = identity(54);
    let dir_a = tempdir().unwrap();
    let dir_b = tempdir().unwrap();

    let (created, target, other, events) = {
        let mut a = Store::open(dir_a.path()).unwrap();
        let created = a.create_guild("replies", &alice).unwrap();
        let message = a
            .post_message(created.guild.id, created.general.id, "known", &alice)
            .unwrap();
        let other = a.create_channel(created.guild.id, "other", &alice).unwrap();
        let events = a
            .events_after(created.guild.id, SyncCursor::start(), 100, usize::MAX)
            .unwrap()
            .events;
        (created, message.id().unwrap(), other, events)
    };

    let mut b = Store::open(dir_b.path()).unwrap();
    for event in events {
        b.import_event(event).unwrap();
    }

    // A reply that names a real message but indexes itself under another
    // channel would put a message into the wrong timeline.
    let wrong_channel = identity(55)
        .sign_body(EventBody {
            version: ovvc_core::model::EVENT_VERSION,
            guild_id: created.guild.id,
            channel_id: Some(other.id),
            author: identity(55).author_id(),
            seq: 0,
            lamport: b.guild_lamport(created.guild.id).unwrap() + 1,
            timestamp: now_unix(),
            prev: None,
            kind: EventKind::MessageV2 {
                body: "wrong".to_string(),
                reply_to: Some(target),
                mentions: Vec::new(),
            },
        })
        .unwrap();
    assert!(b.import_event(wrong_channel).is_err());

    // An unknown target is accepted as an inert orphan; the reply may simply
    // have arrived before the message it answers.
    let orphan = identity(56)
        .sign_body(EventBody {
            version: ovvc_core::model::EVENT_VERSION,
            guild_id: created.guild.id,
            channel_id: Some(created.general.id),
            author: identity(56).author_id(),
            seq: 0,
            lamport: b.guild_lamport(created.guild.id).unwrap() + 1,
            timestamp: now_unix(),
            prev: None,
            kind: EventKind::MessageV2 {
                body: "early".to_string(),
                reply_to: Some(EventId([9u8; 32])),
                mentions: Vec::new(),
            },
        })
        .unwrap();
    assert!(b.import_event(orphan).unwrap());

    // The mention cap is enforced on import too.
    let too_many: Vec<_> = (0..=ovvc_core::MAX_MENTIONS)
        .map(|index| identity((index % 200) as u8 + 100).author_id())
        .collect();
    let flood = identity(57)
        .sign_body(EventBody {
            version: ovvc_core::model::EVENT_VERSION,
            guild_id: created.guild.id,
            channel_id: Some(created.general.id),
            author: identity(57).author_id(),
            seq: 0,
            lamport: b.guild_lamport(created.guild.id).unwrap() + 1,
            timestamp: now_unix(),
            prev: None,
            kind: EventKind::MessageV2 {
                body: "everyone".to_string(),
                reply_to: None,
                mentions: too_many,
            },
        })
        .unwrap();
    assert!(b.import_event(flood).is_err());
}

/// Segments written before the checksum change must keep opening. New appends
/// carry a checksum and coexist with the legacy records in the same segment.
#[test]
fn legacy_checksumless_records_still_load() {
    let alice = identity(60);
    let dir = tempdir().unwrap();
    let created = {
        let mut store = Store::open(dir.path()).unwrap();
        let created = store.create_guild("legacy-format", &alice).unwrap();
        store
            .post_message(created.guild.id, created.general.id, "first", &alice)
            .unwrap();
        store
            .post_message(created.guild.id, created.general.id, "second", &alice)
            .unwrap();
        created
    };

    let segment = guild_events_dir(dir.path(), created.guild.id).join("00000001.seg");
    let bytes = fs::read(&segment).unwrap();
    let records = segment_records(&bytes);
    assert!(
        records.iter().all(|record| record.checksummed),
        "fresh records must carry a checksum"
    );

    // Rewrite every record in the pre-checksum framing: `[u32 len][payload]`,
    // with no flag and no trailing CRC.
    let mut legacy = SEGMENT_MAGIC.to_vec();
    for record in &records {
        legacy.extend_from_slice(&(record.len as u32).to_le_bytes());
        legacy.extend_from_slice(&bytes[record.payload_start..record.payload_start + record.len]);
    }
    assert!(legacy.len() < bytes.len());
    fs::write(&segment, &legacy).unwrap();
    // Force the index to be rebuilt from the legacy log rather than trusting
    // locations left by the previous binary.
    fs::remove_file(dir.path().join("index.redb")).unwrap();

    let mut store = Store::open(dir.path()).unwrap();
    assert_eq!(
        message_bodies(&store, created.guild.id, created.general.id),
        vec!["first".to_string(), "second".to_string()]
    );

    // A new append uses the checksummed framing alongside the legacy records.
    store
        .post_message(created.guild.id, created.general.id, "third", &alice)
        .unwrap();
    drop(store);

    let store = Store::open(dir.path()).unwrap();
    assert_eq!(
        message_bodies(&store, created.guild.id, created.general.id),
        vec![
            "first".to_string(),
            "second".to_string(),
            "third".to_string()
        ]
    );
    let records = segment_records(&fs::read(&segment).unwrap());
    assert!(
        !records[0].checksummed,
        "the legacy record must be read, not rewritten"
    );
    assert!(
        records.last().unwrap().checksummed,
        "appends after the upgrade must be checksummed"
    );
}

/// A bit flip in the payload of the final full-length record is caught by the
/// checksum and repaired as a torn tail, preserving every earlier event.
#[test]
fn corrupted_payload_in_last_record_is_repaired() {
    let alice = identity(61);
    let dir = tempdir().unwrap();
    let created = {
        let mut store = Store::open(dir.path()).unwrap();
        let created = store.create_guild("checksum-tail", &alice).unwrap();
        store
            .post_message(created.guild.id, created.general.id, "kept", &alice)
            .unwrap();
        store
            .post_message(created.guild.id, created.general.id, "corrupt-tail", &alice)
            .unwrap();
        created
    };

    let segment = guild_events_dir(dir.path(), created.guild.id).join("00000001.seg");
    let bytes = fs::read(&segment).unwrap();
    let records = segment_records(&bytes);
    let last = records.last().unwrap();
    let mut corrupt = bytes.clone();
    corrupt[last.payload_start + 1] ^= 0xff;
    fs::write(&segment, &corrupt).unwrap();

    let store = Store::open(dir.path()).unwrap();
    assert_eq!(
        message_bodies(&store, created.guild.id, created.general.id),
        vec!["kept".to_string()],
        "the corrupted tail record must not be indexed"
    );
    assert_eq!(
        fs::metadata(&segment).unwrap().len(),
        last.header as u64,
        "the tail must be truncated to the end of the previous record"
    );
}

/// A bit flip in a middle record is real corruption. It must be reported, and
/// the good records that follow it must be left on disk untouched.
#[test]
fn corrupted_payload_in_middle_record_is_reported_and_later_records_kept() {
    let alice = identity(62);
    let dir = tempdir().unwrap();
    let created = {
        let mut store = Store::open(dir.path()).unwrap();
        let created = store.create_guild("checksum-middle", &alice).unwrap();
        for body in ["one", "two", "three"] {
            store
                .post_message(created.guild.id, created.general.id, body, &alice)
                .unwrap();
        }
        created
    };

    let segment = guild_events_dir(dir.path(), created.guild.id).join("00000001.seg");
    let bytes = fs::read(&segment).unwrap();
    let records = segment_records(&bytes);
    // Record 0 is the guild creation, 1 the channel, 2 the first message.
    let middle = &records[2];
    let mut corrupt = bytes.clone();
    corrupt[middle.payload_start + 1] ^= 0xff;
    fs::write(&segment, &corrupt).unwrap();
    let corrupted_len = fs::metadata(&segment).unwrap().len();

    let error = match Store::open(dir.path()) {
        Ok(_) => panic!("interior corruption must not be silently skipped"),
        Err(error) => error,
    };
    assert!(
        matches!(error, CoreError::CorruptLog { .. }),
        "expected CorruptLog, got {error}"
    );
    assert_eq!(
        fs::metadata(&segment).unwrap().len(),
        corrupted_len,
        "interior corruption must not truncate the records after it"
    );
}

/// A record persisted only partially at the tail must be truncated to the last
/// complete record, not make the store impossible to open.
#[test]
fn truncated_final_record_is_repaired() {
    let alice = identity(63);
    let dir = tempdir().unwrap();
    let created = {
        let mut store = Store::open(dir.path()).unwrap();
        let created = store.create_guild("truncated-tail", &alice).unwrap();
        store
            .post_message(created.guild.id, created.general.id, "kept", &alice)
            .unwrap();
        store
            .post_message(created.guild.id, created.general.id, "cut-off", &alice)
            .unwrap();
        created
    };

    let segment = guild_events_dir(dir.path(), created.guild.id).join("00000001.seg");
    let bytes = fs::read(&segment).unwrap();
    let records = segment_records(&bytes);
    let last = records.last().unwrap();
    // The final record loses its last two bytes, as if the process died inside
    // the checksum write.
    fs::write(&segment, &bytes[..bytes.len() - 2]).unwrap();

    let store = Store::open(dir.path()).unwrap();
    assert_eq!(
        message_bodies(&store, created.guild.id, created.general.id),
        vec!["kept".to_string()]
    );
    assert_eq!(
        fs::metadata(&segment).unwrap().len(),
        last.header as u64,
        "the partial record must be truncated away"
    );
}

/// A guild directory that disappeared entirely is not visited by the
/// per-guild prune, so its guild-scoped rows must still be swept out before a
/// read can follow a dangling location into a missing segment.
#[test]
fn a_removed_guild_directory_is_pruned_from_the_index() {
    let alice = identity(65);
    let dir = tempdir().unwrap();
    let (kept, lost, lost_event) = {
        let mut store = Store::open(dir.path()).unwrap();
        let kept = store.create_guild("kept-guild", &alice).unwrap();
        let lost = store.create_guild("lost-guild", &alice).unwrap();
        let lost_event = store
            .post_message(lost.guild.id, lost.general.id, "gone", &alice)
            .unwrap();
        (kept, lost, lost_event)
    };

    fs::remove_dir_all(dir.path().join("guilds").join(lost.guild.id.to_hex())).unwrap();

    let store = Store::open(dir.path()).unwrap();
    assert!(!store.has_event(lost_event.id().unwrap()).unwrap());
    assert!(
        store
            .history(lost.guild.id, lost.general.id, 50)
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.list_channels(kept.guild.id).unwrap().len(), 1);
}

/// Streaming recovery must index a store with several guilds and several
/// segments correctly, including after the index has been invalidated, and a
/// second open must be a no-op.
#[test]
fn recovery_indexes_multi_guild_multi_segment_idempotently() {
    let alice = identity(64);
    let dir = tempdir().unwrap();
    let (a, b) = {
        let mut store = Store::open(dir.path()).unwrap();
        let a = store.create_guild("multi-a", &alice).unwrap();
        let b = store.create_guild("multi-b", &alice).unwrap();
        for index in 0..3 {
            store
                .post_message(a.guild.id, a.general.id, &format!("a{index}"), &alice)
                .unwrap();
            store
                .post_message(b.guild.id, b.general.id, &format!("b{index}"), &alice)
                .unwrap();
        }
        (a, b)
    };

    // Split each guild's single segment into two at a record boundary. The
    // index still points at the old locations, so recovery must prune the
    // moved rows and re-index those records under segment 2.
    for guild_id in [a.guild.id, b.guild.id] {
        let events = guild_events_dir(dir.path(), guild_id);
        let bytes = fs::read(events.join("00000001.seg")).unwrap();
        let records = segment_records(&bytes);
        let split = records.len() / 2;
        assert!(split > 0 && split < records.len());
        let boundary = records[split].header;

        let mut first = SEGMENT_MAGIC.to_vec();
        first.extend_from_slice(&bytes[8..boundary]);
        let mut second = SEGMENT_MAGIC.to_vec();
        second.extend_from_slice(&bytes[boundary..]);
        fs::write(events.join("00000001.seg"), first).unwrap();
        fs::write(events.join("00000002.seg"), second).unwrap();
    }

    let expected = 5; // guild creation, channel creation, three messages
    for pass in 0..2 {
        let store = Store::open(dir.path()).unwrap();
        for (index, guild_id) in [a.guild.id, b.guild.id].into_iter().enumerate() {
            let page = store
                .events_after(guild_id, SyncCursor::start(), 100, usize::MAX)
                .unwrap();
            assert_eq!(page.events.len(), expected, "pass {pass}, guild {index}");
            assert!(!page.more, "pass {pass}, guild {index}: paging lost events");
            let unique: HashSet<EventId> = page
                .events
                .iter()
                .map(|event| event.id().unwrap())
                .collect();
            assert_eq!(
                unique.len(),
                expected,
                "pass {pass}, guild {index}: duplicate index rows"
            );
        }
        assert_eq!(
            message_bodies(&store, a.guild.id, a.general.id),
            vec!["a0".to_string(), "a1".to_string(), "a2".to_string()]
        );
        assert_eq!(
            message_bodies(&store, b.guild.id, b.general.id),
            vec!["b0".to_string(), "b1".to_string(), "b2".to_string()]
        );
    }

    // Both segments survive the idempotent reopen with a valid header.
    for guild_id in [a.guild.id, b.guild.id] {
        let events = guild_events_dir(dir.path(), guild_id);
        for segment in ["00000001.seg", "00000002.seg"] {
            let bytes = fs::read(events.join(segment)).unwrap();
            assert_eq!(&bytes[..8], SEGMENT_MAGIC);
        }
    }
}

#[test]
fn blobs_are_content_addressed_and_persist() {
    let dir = tempdir().unwrap();
    let bytes = b"the exact attachment bytes";
    let id = ovvc_core::hash_bytes(bytes);
    {
        let mut store = Store::open(dir.path()).unwrap();
        assert_eq!(store.get_blob(id).unwrap(), None);
        assert!(!store.has_blob(id).unwrap());
        store.put_blob(id, bytes).unwrap();
        assert_eq!(store.get_blob(id).unwrap().as_deref(), Some(&bytes[..]));
        assert!(store.has_blob(id).unwrap());
    }
    // Blobs live in the index, not in a guild log, so they reopen with it.
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.get_blob(id).unwrap().as_deref(), Some(&bytes[..]));
}

#[test]
fn a_blob_must_match_its_hash_and_the_size_limit() {
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let bytes = b"real bytes";
    let id = ovvc_core::hash_bytes(bytes);
    let other = ovvc_core::hash_bytes(b"other bytes");

    assert!(store.put_blob(other, bytes).is_err());
    assert_eq!(store.get_blob(id).unwrap(), None, "nothing was written");

    let oversized = vec![0u8; ovvc_core::MAX_ATTACHMENT_BYTES as usize + 1];
    let oversized_id = ovvc_core::hash_bytes(&oversized);
    assert!(store.put_blob(oversized_id, &oversized).is_err());
    assert_eq!(store.get_blob(oversized_id).unwrap(), None);
}

#[test]
fn attachments_are_announced_with_metadata_and_sync() {
    let alice = identity(61);
    let bob = identity(62);
    let alice_dir = tempdir().unwrap();
    let bob_dir = tempdir().unwrap();
    let mut alice_store = Store::open(alice_dir.path()).unwrap();
    let mut bob_store = Store::open(bob_dir.path()).unwrap();
    let created = alice_store.create_guild("files", &alice).unwrap();
    let guild_id = created.guild.id;
    let channel_id = created.general.id;

    let bytes = b"the contents of a small note";
    let note = ovvc_core::Attachment {
        hash: ovvc_core::hash_bytes(bytes),
        size: bytes.len() as u64,
        mime: "text/plain".to_string(),
        name: "note.txt".to_string(),
    };
    let image = ovvc_core::Attachment {
        hash: ovvc_core::hash_bytes(b"pretend png"),
        size: 11,
        mime: "image/png".to_string(),
        name: "pixel.png".to_string(),
    };
    let event = alice_store
        .post_message_rich(
            guild_id,
            channel_id,
            "two files",
            None,
            &[bob.author_id()],
            &[note.clone(), image.clone()],
            &alice,
        )
        .unwrap();
    assert_eq!(
        event.body.kind,
        EventKind::MessageV3 {
            body: "two files".to_string(),
            reply_to: None,
            mentions: vec![bob.author_id()],
            attachments: vec![note.clone(), image.clone()],
        }
    );
    assert_eq!(
        event.body.kind.attachments(),
        &[note.clone(), image.clone()]
    );
    assert!(event.body.kind.is_message());
    assert_eq!(event.body.kind.mentions(), [bob.author_id()]);

    // Metadata validation happens on import too, so a peer cannot inject an
    // attachment list a local post would have refused.
    // A fresh peer learns the guild from its creation event onward, so replay
    // the whole stream in order rather than dropping one message into an empty
    // store.
    let page = alice_store
        .events_after(guild_id, ovvc_core::SyncCursor::start(), 100, usize::MAX)
        .unwrap();
    for peer_event in page.events {
        bob_store.import_event(peer_event).unwrap();
    }
    let synced = bob_store.history(guild_id, channel_id, 50).unwrap();
    let folded = ovvc_core::timeline(&synced).unwrap();
    let announced = folded.iter().any(|entry| {
        matches!(
            entry,
            ovvc_core::TimelineEntry::Message { body, .. } if body == "two files"
        )
    });
    assert!(announced, "the message syncs to the peer");

    // A peer may sign whatever it likes, so the import path runs the same
    // metadata checks. Re-signing the valid event with the same file listed
    // twice must be refused rather than stored.
    let forged = alice
        .sign_body(EventBody {
            kind: EventKind::MessageV3 {
                body: event.body.kind.message_body().unwrap().to_string(),
                reply_to: None,
                mentions: vec![bob.author_id()],
                attachments: vec![note.clone(), note.clone()],
            },
            ..event.body.clone()
        })
        .unwrap();
    assert!(
        bob_store.import_event(forged).is_err(),
        "an imported message with duplicate attachments must be rejected"
    );

    for bad in [
        ovvc_core::Attachment {
            hash: ovvc_core::AttachmentId([0u8; 32]),
            ..note.clone()
        },
        ovvc_core::Attachment {
            size: ovvc_core::MAX_ATTACHMENT_BYTES + 1,
            ..note.clone()
        },
        ovvc_core::Attachment {
            name: String::new(),
            ..note.clone()
        },
        ovvc_core::Attachment {
            mime: "not a mime type".to_string(),
            ..note.clone()
        },
    ] {
        assert!(
            alice_store
                .post_message_rich(guild_id, channel_id, "bad", None, &[], &[bad], &alice)
                .is_err(),
            "invalid attachment metadata must be rejected locally"
        );
    }
    // The same file twice in one message is a mistake, not two attachments.
    assert!(
        alice_store
            .post_message_rich(
                guild_id,
                channel_id,
                "twice",
                None,
                &[],
                &[note.clone(), note.clone()],
                &alice,
            )
            .is_err()
    );
    let too_many: Vec<_> = (0..=ovvc_core::MAX_ATTACHMENTS_PER_MESSAGE)
        .map(|index| ovvc_core::Attachment {
            hash: ovvc_core::hash_bytes(&index.to_le_bytes()),
            size: 8,
            mime: "application/octet-stream".to_string(),
            name: format!("file-{index}.bin"),
        })
        .collect();
    assert!(
        alice_store
            .post_message_rich(guild_id, channel_id, "many", None, &[], &too_many, &alice)
            .is_err()
    );

    // A file with no caption is ordinary chat, not an invalid message.
    let captionless = alice_store
        .post_message_rich(
            guild_id,
            channel_id,
            "",
            None,
            &[],
            std::slice::from_ref(&note),
            &alice,
        )
        .unwrap();
    assert_eq!(captionless.body.kind.message_body(), Some(""));
    // Without a file, an empty body is still refused.
    assert!(
        alice_store
            .post_message_rich(guild_id, channel_id, "", None, &[], &[], &alice)
            .is_err()
    );

    // A message with neither a file nor a reply still uses the compact kind.
    let plain = alice_store
        .post_message_rich(guild_id, channel_id, "plain", None, &[], &[], &alice)
        .unwrap();
    assert_eq!(
        plain.body.kind,
        EventKind::Message {
            body: "plain".to_string()
        }
    );
}

#[test]
fn categories_and_channel_layout_survive_any_import_order() {
    let alice = identity(61);
    let dir = tempdir().unwrap();
    let (guild, category, voice, events) = {
        let mut store = Store::open(dir.path()).unwrap();
        let created = store.create_guild("org", &alice).unwrap();
        let guild = created.guild.id;
        let category = store.create_category(guild, "projects", &alice).unwrap().0;
        let voice = store
            .create_channel_in(
                guild,
                "standup",
                ChannelKind::Voice,
                Some(category.id),
                &alice,
            )
            .unwrap()
            .0;
        store
            .rename_channel(guild, voice.id, "daily-standup", &alice)
            .unwrap();
        store.move_channel(guild, voice.id, None, &alice).unwrap();
        let page = store
            .events_after(guild, SyncCursor::start(), 100, 1 << 20)
            .unwrap();
        (guild, category, voice, page.events)
    };
    assert_eq!(
        events.len(),
        6,
        "guild, general, category, channel, rename, move"
    );

    // A second store receives the tail events in the opposite order. The
    // layout must still resolve to the same channel: kind from creation, the
    // latest name, and the root parent from the move.
    let other_dir = tempdir().unwrap();
    let mut importer = Store::open(other_dir.path()).unwrap();
    importer.import_event(events[0].clone()).unwrap();
    importer.import_event(events[1].clone()).unwrap();
    importer.import_event(events[2].clone()).unwrap();
    importer.import_event(events[3].clone()).unwrap();
    importer.import_event(events[5].clone()).unwrap();
    importer.import_event(events[4].clone()).unwrap();

    let categories = importer.list_categories(guild).unwrap();
    assert_eq!(categories, vec![category.clone()]);
    let views = importer.list_channel_views(guild).unwrap();
    let daily = views
        .iter()
        .find(|view| view.id == voice.id)
        .expect("the voice channel is listed");
    assert_eq!(daily.kind, ChannelKind::Voice);
    assert_eq!(daily.name, "daily-standup");
    assert_eq!(daily.parent, None);
    // Root channels sort before categories. `general` and the moved channel
    // are both at the root, so they come first, ordered by name.
    assert_eq!(views[0].parent, None);
    assert_eq!(views[1].parent, None);
    assert!(views[0].name <= views[1].name);

    // The original store agrees.
    let original = Store::open(dir.path()).unwrap();
    assert_eq!(original.list_channel_views(guild).unwrap(), views);
}

/// A profile is an ordinary signed event, and the resolved profile is a fold
/// of the guild log: latest `(lamport, event_id)` wins, display name still
/// comes from `MemberRenamed`, and counts are derived rather than stored.
#[test]
fn profile_is_event_backed_and_derived_activity_is_folded() {
    let alice = identity(70);
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let created = store.create_guild("profiles", &alice).unwrap();
    store.set_display_name(created.guild.id, "Alice", &alice).unwrap();
    store
        .post_message(created.guild.id, created.general.id, "one", &alice)
        .unwrap();

    let links = vec!["https://example.com/about".to_string()];
    let event = store
        .set_profile(
            created.guild.id,
            &alice,
            "building a peer-to-peer chat",
            "she/her",
            &links,
        )
        .unwrap();
    assert_eq!(
        event.body.kind,
        EventKind::MemberProfileUpdated {
            about: "building a peer-to-peer chat".to_string(),
            pronouns: "she/her".to_string(),
            links: links.clone(),
        }
    );
    event.verify().unwrap();
    assert_eq!(event.body.channel_id, None);

    let profile = store
        .member_profile(created.guild.id, alice.author_id())
        .unwrap();
    assert_eq!(profile.display_name, "Alice");
    assert_eq!(profile.about, "building a peer-to-peer chat");
    assert_eq!(profile.pronouns, "she/her");
    assert_eq!(profile.links, links);
    assert_eq!(profile.updated_at, event.body.timestamp);
    assert_eq!(profile.message_count, 1, "one visible message by Alice");
    assert!(profile.last_activity >= profile.updated_at);
    assert!(profile.joined_at <= profile.last_activity);

    // A later write replaces the fields, not the display name.
    let second = store
        .set_profile(created.guild.id, &alice, "second about", "", &[])
        .unwrap();
    let resolved = store
        .member_profile(created.guild.id, alice.author_id())
        .unwrap();
    assert_eq!(resolved.about, "second about");
    assert_eq!(resolved.pronouns, "");
    assert!(resolved.links.is_empty());
    assert_eq!(resolved.updated_at, second.body.timestamp);
    assert_eq!(resolved.display_name, "Alice");

    // The fold survives a reopen, so no profile table is needed.
    drop(store);
    let reopened = Store::open(dir.path()).unwrap();
    let resolved = reopened
        .member_profile(created.guild.id, alice.author_id())
        .unwrap();
    assert_eq!(resolved.about, "second about");

    // A profile needs a real member.
    let bob = identity(71);
    assert!(matches!(
        reopened.member_profile(created.guild.id, bob.author_id()),
        Err(CoreError::NotFound(_))
    ));
}

/// The fields have hard limits, and links are restricted to http(s), so a
/// profile cannot be used as an unbounded side channel.
#[test]
fn profile_fields_are_validated_before_appending() {
    let alice = identity(72);
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let guild = store.create_guild("profile limits", &alice).unwrap().guild.id;

    let too_long = "x".repeat(MAX_PROFILE_ABOUT_BYTES + 1);
    assert!(matches!(
        store.set_profile(guild, &alice, &too_long, "", &[]),
        Err(CoreError::InvalidInput(_))
    ));
    assert!(matches!(
        store.set_profile(guild, &alice, "", "line\nbreak", &[]),
        Err(CoreError::InvalidInput(_))
    ));
    let too_many: Vec<String> = (0..=MAX_PROFILE_LINKS).map(|_| "https://a".to_string()).collect();
    assert!(matches!(
        store.set_profile(guild, &alice, "", "", &too_many),
        Err(CoreError::InvalidInput(_))
    ));
    for bad in ["ftp://example.com", "example.com", "https://", "https://a b"] {
        assert!(
            store
                .set_profile(guild, &alice, "", "", &[bad.to_string()])
                .is_err(),
            "{bad:?} must not be accepted as a profile link"
        );
    }

    // Import applies the same rules, not only the local write path.
    let forged = alice
        .sign_body(EventBody {
            version: 1,
            guild_id: guild,
            channel_id: None,
            author: alice.author_id(),
            seq: 100,
            lamport: 100,
            timestamp: now_unix(),
            prev: Some(EventId([3u8; 32])),
            kind: EventKind::MemberProfileUpdated {
                about: "x".repeat(MAX_PROFILE_ABOUT_BYTES + 1),
                pronouns: String::new(),
                links: Vec::new(),
            },
        })
        .unwrap();
    assert!(store.import_event(forged).is_err());
}

/// With two profile events at the same Lamport clock, the event id is the
/// deterministic tie-break, so two stores that receive them in opposite
/// orders resolve the same profile.
#[test]
fn profile_tie_break_converges_regardless_of_arrival_order() {
    let alice = identity(73);
    let bob = identity(74);
    let source_dir = tempdir().unwrap();
    let (guild, creation) = {
        let mut store = Store::open(source_dir.path()).unwrap();
        let created = store.create_guild("profile ties", &alice).unwrap();
        store
            .append_member_joined(created.guild.id, "Bob", &bob)
            .unwrap();
        let page = store
            .events_after(created.guild.id, SyncCursor::start(), 100, 1 << 20)
            .unwrap();
        (created.guild.id, page.events)
    };

    // Two signed profile updates at the same Lamport clock. Their bodies (and
    // therefore ids) differ, so the id breaks the tie deterministically.
    let profile = |seq: u64, about: &str| {
        alice
            .sign_body(EventBody {
                version: 1,
                guild_id: guild,
                channel_id: None,
                author: alice.author_id(),
                seq,
                lamport: 10,
                timestamp: now_unix(),
                prev: Some(EventId([4u8; 32])),
                kind: EventKind::MemberProfileUpdated {
                    about: about.to_string(),
                    pronouns: String::new(),
                    links: Vec::new(),
                },
            })
            .unwrap()
    };
    let first = profile(100, "first");
    let second = profile(101, "second");
    assert_ne!(first.id().unwrap(), second.id().unwrap());
    let expected = if first.id().unwrap() > second.id().unwrap() {
        "first"
    } else {
        "second"
    };

    let mut winner = None;
    for order in [false, true] {
        let dir = tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        for event in &creation {
            store.import_event(event.clone()).unwrap();
        }
        let pair = if order {
            [&second, &first]
        } else {
            [&first, &second]
        };
        for event in pair {
            store.import_event(event.clone()).unwrap();
        }
        let resolved = store.member_profile(guild, alice.author_id()).unwrap();
        assert_eq!(
            resolved.about, expected,
            "order {order}: the greater (lamport, event id) must win"
        );
        match &winner {
            None => winner = Some(resolved.about),
            Some(previous) => assert_eq!(
                previous, &resolved.about,
                "order {order} resolved a different profile"
            ),
        }
    }
    assert_eq!(winner.as_deref(), Some(expected));
}

/// A direct conversation is an ordinary guild under an id both keys derive.
/// The founder is recorded through `MemberJoined`, the opener announces the
/// pair in a shared channel, and both the local call and the peer import
/// reject an announcement whose guild id does not match the two authors.
#[test]
fn direct_guild_creation_and_announcement_are_validated() {
    let alice = identity(75);
    let bob = identity(76);
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let context = store.create_guild("shared", &alice).unwrap();
    store
        .append_member_joined(context.guild.id, "Bob", &bob)
        .unwrap()
        .expect("bob was not on the roster yet");

    let dm_id = direct_guild_id(&alice.author_id(), &bob.author_id());
    let dm = store
        .create_guild_with_id(dm_id, "Bob", "Alice", &alice)
        .unwrap();
    assert_eq!(dm.guild.id, dm_id);
    assert_eq!(dm.general.name, "general");
    let members = store.list_members(dm_id).unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].author, alice.author_id());
    assert_eq!(members[0].display_name, "Alice");
    assert!(
        store
            .create_guild_with_id(dm_id, "again", "Alice", &alice)
            .is_err(),
        "a chosen id must not overwrite an existing guild"
    );
    assert_eq!(
        store
            .append_member_joined(dm_id, "Alice", &alice)
            .unwrap(),
        None,
        "joining twice is idempotent"
    );

    let event = store
        .announce_direct_message(
            context.guild.id,
            context.general.id,
            bob.author_id(),
            dm_id,
            &alice,
        )
        .unwrap();
    assert_eq!(
        event.body.kind,
        EventKind::DirectMessageOpened {
            peer: bob.author_id(),
            dm: dm_id,
        }
    );
    assert_eq!(event.body.channel_id, Some(context.general.id));
    event.verify().unwrap();

    let wrong = direct_guild_id(&alice.author_id(), &alice.author_id());
    assert!(
        store
            .announce_direct_message(
                context.guild.id,
                context.general.id,
                bob.author_id(),
                wrong,
                &alice,
            )
            .is_err(),
        "the announcement must name the pair's derived id"
    );
    assert!(
        store
            .announce_direct_message(
                context.guild.id,
                context.general.id,
                alice.author_id(),
                dm_id,
                &alice,
            )
            .is_err(),
        "a conversation needs another member"
    );
    let carol = identity(77);
    assert!(
        store
            .announce_direct_message(
                context.guild.id,
                context.general.id,
                carol.author_id(),
                direct_guild_id(&alice.author_id(), &carol.author_id()),
                &alice,
            )
            .is_err(),
        "the peer must be on the shared guild's roster"
    );

    // The recipient's store accepts the real announcement and rejects a
    // correctly signed one that points at a guild id for a different pair.
    let recipient_dir = tempdir().unwrap();
    let mut recipient = Store::open(recipient_dir.path()).unwrap();
    for event in store
        .events_after(context.guild.id, SyncCursor::start(), 100, 1 << 20)
        .unwrap()
        .events
    {
        if matches!(event.body.kind, EventKind::DirectMessageOpened { .. }) {
            continue;
        }
        recipient.import_event(event).unwrap();
    }
    recipient.import_event(event.clone()).unwrap();
    assert_eq!(
        recipient
            .events_after(context.guild.id, SyncCursor::start(), 100, 1 << 20)
            .unwrap()
            .events
            .len(),
        store
            .events_after(context.guild.id, SyncCursor::start(), 100, 1 << 20)
            .unwrap()
            .events
            .len()
    );

    let forged = alice
        .sign_body(EventBody {
            version: 1,
            guild_id: context.guild.id,
            channel_id: Some(context.general.id),
            author: alice.author_id(),
            seq: 100,
            lamport: 100,
            timestamp: now_unix(),
            prev: Some(EventId([5u8; 32])),
            kind: EventKind::DirectMessageOpened {
                peer: bob.author_id(),
                dm: wrong,
            },
        })
        .unwrap();
    assert!(recipient.import_event(forged).is_err());
}

/// The derived id is the same whichever author is passed first, differs per
/// pair, and is pinned so a change in the derivation cannot pass silently.
/// That pin matters on the wire: two implementations must agree or they would
/// create two guilds for one conversation.
#[test]
fn direct_guild_id_is_stable() {
    let alice = identity(1).author_id();
    let bob = identity(2).author_id();
    assert_eq!(direct_guild_id(&alice, &bob), direct_guild_id(&bob, &alice));
    assert_ne!(direct_guild_id(&alice, &bob), direct_guild_id(&alice, &alice));
    assert_eq!(
        direct_guild_id(&alice, &bob).to_hex(),
        "b7dcbc6e7c704c0542a7ad26533eaa29fb707b6829ee98475b646f54179aac13"
    );
}
