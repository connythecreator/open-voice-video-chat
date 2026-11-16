//! Wire-format hardening: version gating and canonical decoding.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ovvc_core::error::CoreError;
use ovvc_core::model::{
    ChannelId, ChannelKind, EVENT_VERSION, EventBody, EventId, EventKind, GuildId, SignedEvent,
};
use ovvc_core::{Identity, Invite};

fn message(identity: &Identity, version: u8) -> SignedEvent {
    identity
        .sign_body(EventBody {
            version,
            guild_id: GuildId([1u8; 32]),
            channel_id: Some(ChannelId([2u8; 16])),
            author: identity.author_id(),
            seq: 1,
            lamport: 1,
            timestamp: 0,
            prev: Some(EventId([3u8; 32])),
            kind: EventKind::Message {
                body: "hello".to_string(),
            },
        })
        .unwrap()
}

#[test]
fn verify_rejects_unsupported_event_versions() {
    let identity = Identity::from_seed([1u8; 32]);
    assert!(message(&identity, EVENT_VERSION).verify().is_ok());
    // Version 1 is the pre-tag envelope. Stores written by older binaries still
    // have to open, so those events must keep verifying.
    assert!(message(&identity, 1).verify().is_ok());

    for version in [0u8, 3, 255] {
        let error = message(&identity, version).verify().unwrap_err();
        assert!(
            matches!(error, CoreError::InvalidInput(_)),
            "version {version} should be rejected, got {error:?}"
        );
    }
}

/// A version-2 event whose kind tag this binary does not know must decode,
/// re-encode byte-for-byte, and verify. That is what lets a newer peer add an
/// event kind without making this binary unable to open its own log.
#[test]
fn unknown_kind_tags_roundtrip_verbatim() {
    let identity = Identity::from_seed([7u8; 32]);
    let unknown = EventKind::Unknown {
        tag: 4242,
        payload: vec![0xAA, 0x01, 0x02, 0x03],
    };
    let event = identity
        .sign_body(EventBody {
            version: EVENT_VERSION,
            guild_id: GuildId([8u8; 32]),
            channel_id: Some(ChannelId([9u8; 16])),
            author: identity.author_id(),
            seq: 1,
            lamport: 1,
            timestamp: 0,
            prev: Some(EventId([10u8; 32])),
            kind: unknown.clone(),
        })
        .unwrap();

    let bytes = event.to_bytes().unwrap();
    let decoded = SignedEvent::from_bytes(&bytes).unwrap();
    assert_eq!(decoded.body.kind, unknown);
    assert_eq!(decoded.to_bytes().unwrap(), bytes);
    assert!(decoded.verify().is_ok());
}

/// The same logical event written by a pre-tag binary (version 1) is still
/// decodable, and its signature still covers the version-1 encoding.
#[test]
fn legacy_version_one_events_still_decode_and_verify() {
    let identity = Identity::from_seed([11u8; 32]);
    let event = message(&identity, 1);
    let bytes = event.to_bytes().unwrap();
    let decoded = SignedEvent::from_bytes(&bytes).unwrap();
    assert_eq!(decoded, event);
    assert!(decoded.verify().is_ok());
    assert_eq!(decoded.to_bytes().unwrap(), bytes);
}

#[test]
fn decoders_reject_trailing_bytes() {
    let identity = Identity::from_seed([2u8; 32]);
    let event = message(&identity, EVENT_VERSION);
    let mut padded = event.to_bytes().unwrap();
    padded.extend_from_slice(&[0, 1, 2]);
    assert!(SignedEvent::from_bytes(&padded).is_err());

    let invite = Invite::new(
        GuildId([4u8; 32]),
        "guild".to_string(),
        &identity,
        vec!["/ip4/127.0.0.1/tcp/4001".to_string()],
        None,
    )
    .unwrap();
    let mut raw = URL_SAFE_NO_PAD.decode(invite.encode().unwrap()).unwrap();
    raw.push(0);
    let padded = URL_SAFE_NO_PAD.encode(raw);
    assert!(Invite::decode(&padded).is_err());
}

/// postcard accepts a redundant continuation byte for varints. The same event
/// must not have a second accepted wire spelling.
#[test]
fn event_decoder_rejects_overlong_varints() {
    let identity = Identity::from_seed([3u8; 32]);
    let mut raw = message(&identity, EVENT_VERSION).to_bytes().unwrap();

    // version(1) || guild(32) || channel tag+id(17) || author(32) = 82.
    assert_eq!(raw[82], 1, "seq is expected to start at offset 82");
    raw.splice(82..83, [0x81, 0x00]);

    assert!(SignedEvent::from_bytes(&raw).is_err());
}

#[test]
fn invite_creation_refuses_oversized_codes() {
    let identity = Identity::from_seed([5u8; 32]);
    let oversized = Invite::new(
        GuildId([6u8; 32]),
        "x".repeat(9000),
        &identity,
        Vec::new(),
        None,
    );
    assert!(oversized.is_err());
}

/// Replies and mentions travel in a new tagged kind, so the existing `Message`
/// encoding is untouched and a plain message stays byte-identical.
#[test]
fn reply_and_mention_payloads_roundtrip_and_verify() {
    let identity = Identity::from_seed([12u8; 32]);
    let target = EventId([13u8; 32]);
    let mentioned = Identity::from_seed([14u8; 32]).author_id();
    let kind = EventKind::MessageV2 {
        body: "answer @bob".to_string(),
        reply_to: Some(target),
        mentions: vec![mentioned],
    };
    let event = identity
        .sign_body(EventBody {
            version: EVENT_VERSION,
            guild_id: GuildId([15u8; 32]),
            channel_id: Some(ChannelId([16u8; 16])),
            author: identity.author_id(),
            seq: 1,
            lamport: 1,
            timestamp: 0,
            prev: Some(EventId([17u8; 32])),
            kind: kind.clone(),
        })
        .unwrap();

    let bytes = event.to_bytes().unwrap();
    let decoded = SignedEvent::from_bytes(&bytes).unwrap();
    assert_eq!(decoded.body.kind, kind);
    assert_eq!(decoded.body.kind.reply_to(), Some(target));
    assert_eq!(decoded.body.kind.mentions(), [mentioned]);
    assert_eq!(decoded.to_bytes().unwrap(), bytes);
    assert!(decoded.verify().is_ok());

    // A plain message still encodes with its original kind, not the new one.
    let plain = EventBody {
        version: EVENT_VERSION,
        guild_id: GuildId([15u8; 32]),
        channel_id: Some(ChannelId([16u8; 16])),
        author: identity.author_id(),
        seq: 1,
        lamport: 1,
        timestamp: 0,
        prev: Some(EventId([17u8; 32])),
        kind: EventKind::Message {
            body: "answer @bob".to_string(),
        },
    };
    assert_ne!(plain.kind, kind);
    assert!(plain.kind.mentions().is_empty());
    assert_eq!(plain.kind.reply_to(), None);
}

#[test]
fn attachment_metadata_roundtrips_and_verifies() {
    let identity = Identity::from_seed([18u8; 32]);
    let mentioned = Identity::from_seed([19u8; 32]).author_id();
    let bytes = b"a small file";
    let attachment = ovvc_core::Attachment {
        hash: ovvc_core::hash_bytes(bytes),
        size: bytes.len() as u64,
        mime: "text/plain".to_string(),
        name: "note.txt".to_string(),
    };
    let kind = EventKind::MessageV3 {
        body: "see attached".to_string(),
        reply_to: Some(EventId([20u8; 32])),
        mentions: vec![mentioned],
        attachments: vec![attachment.clone()],
    };
    let event = identity
        .sign_body(EventBody {
            version: EVENT_VERSION,
            guild_id: GuildId([21u8; 32]),
            channel_id: Some(ChannelId([22u8; 16])),
            author: identity.author_id(),
            seq: 1,
            lamport: 1,
            timestamp: 0,
            prev: None,
            kind: kind.clone(),
        })
        .unwrap();

    let encoded = event.to_bytes().unwrap();
    let decoded = SignedEvent::from_bytes(&encoded).unwrap();
    assert_eq!(decoded.body.kind, kind);
    assert!(decoded.body.kind.is_message());
    assert_eq!(decoded.body.kind.message_body(), Some("see attached"));
    assert_eq!(decoded.body.kind.mentions(), [mentioned]);
    assert_eq!(decoded.body.kind.attachments(), [attachment]);
    assert_eq!(decoded.to_bytes().unwrap(), encoded);
    assert!(decoded.verify().is_ok());

    // Only a V3 message announces files; older kinds answer with an empty
    // slice rather than making every caller special-case them.
    let plain = EventKind::Message {
        body: "no files".to_string(),
    };
    assert!(plain.attachments().is_empty());
}

/// Channel organisation events must roundtrip byte-for-byte and verify, so a
/// peer running the previous binary can store them as `Unknown` and a peer
/// running this one applies them.
#[test]
fn channel_organisation_events_roundtrip_and_verify() {
    let identity = Identity::from_seed([21u8; 32]);
    let guild = GuildId([22u8; 32]);
    let channel = ChannelId([23u8; 16]);
    let category = ChannelId([24u8; 16]);
    let kinds = [
        EventKind::CategoryCreated {
            name: "projects".to_string(),
        },
        EventKind::ChannelCreatedV2 {
            name: "standup".to_string(),
            kind: ChannelKind::Voice,
            parent: Some(category),
        },
        EventKind::ChannelRenamed {
            target: channel,
            name: "daily-standup".to_string(),
        },
        EventKind::ChannelMoved {
            target: channel,
            parent: Some(category),
        },
    ];
    for (index, kind) in kinds.into_iter().enumerate() {
        let body = EventBody {
            version: EVENT_VERSION,
            guild_id: guild,
            channel_id: Some(channel),
            author: identity.author_id(),
            seq: index as u64,
            lamport: index as u64 + 1,
            timestamp: 0,
            prev: (index > 0).then_some(EventId([25u8; 32])),
            kind: kind.clone(),
        };
        let event = identity.sign_body(body).unwrap();
        let bytes = event.to_bytes().unwrap();
        let decoded = SignedEvent::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.body.kind, kind);
        assert_eq!(decoded.to_bytes().unwrap(), bytes);
        assert!(decoded.verify().is_ok());
    }

    assert_eq!(ChannelKind::from_name("voice"), Some(ChannelKind::Voice));
    assert_eq!(ChannelKind::from_name("Forum"), Some(ChannelKind::Forum));
    assert_eq!(ChannelKind::from_name("nope"), None);
    assert!(ChannelKind::Announcement.is_textual());
    assert!(!ChannelKind::Forum.is_textual());
}

/// The direct-message guild id has to be identical on both sides, so argument
/// order must not matter, and two different pairs must not collide in
/// practice. The pinned vector catches an accidental change to the domain
/// separator or the sort order, which would silently split existing DMs into
/// two guilds.
#[test]
fn direct_guild_id_is_order_independent_stable_and_pair_specific() {
    use ovvc_core::direct_guild_id;

    let alice = Identity::from_seed([1u8; 32]).author_id();
    let bob = Identity::from_seed([2u8; 32]).author_id();
    let carol = Identity::from_seed([3u8; 32]).author_id();

    assert_eq!(direct_guild_id(&alice, &bob), direct_guild_id(&bob, &alice));
    assert_ne!(direct_guild_id(&alice, &bob), direct_guild_id(&alice, &carol));
    assert_ne!(direct_guild_id(&alice, &bob), direct_guild_id(&bob, &carol));
    assert_ne!(direct_guild_id(&alice, &alice), direct_guild_id(&alice, &bob));

    assert_eq!(
        direct_guild_id(&alice, &bob).to_hex(),
        "b7dcbc6e7c704c0542a7ad26533eaa29fb707b6829ee98475b646f54179aac13"
    );
}

/// The two new event kinds use the tagged version-2 envelope, so they must
/// round-trip byte-for-byte and verify like every existing kind.
#[test]
fn profile_and_direct_message_events_roundtrip_and_verify() {
    let identity = Identity::from_seed([11u8; 32]);
    let peer = Identity::from_seed([12u8; 32]).author_id();
    let kinds = vec![
        EventKind::MemberProfileUpdated {
            about: "builds things".to_string(),
            pronouns: "they/them".to_string(),
            links: vec!["https://example.com".to_string()],
        },
        EventKind::DirectMessageOpened {
            peer,
            dm: direct_guild_id_for_test(&identity.author_id(), &peer),
        },
    ];

    for (index, kind) in kinds.into_iter().enumerate() {
        let event = identity
            .sign_body(EventBody {
                version: EVENT_VERSION,
                guild_id: GuildId([13u8; 32]),
                channel_id: Some(ChannelId([14u8; 16])),
                author: identity.author_id(),
                seq: index as u64,
                lamport: index as u64 + 1,
                timestamp: 0,
                prev: (index > 0).then_some(EventId([15u8; 32])),
                kind: kind.clone(),
            })
            .unwrap();
        let bytes = event.to_bytes().unwrap();
        let decoded = SignedEvent::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.body.kind, kind);
        assert_eq!(decoded.to_bytes().unwrap(), bytes);
        assert!(decoded.verify().is_ok());
    }
}

fn direct_guild_id_for_test(a: &ovvc_core::AuthorId, b: &ovvc_core::AuthorId) -> GuildId {
    ovvc_core::direct_guild_id(a, b)
}
