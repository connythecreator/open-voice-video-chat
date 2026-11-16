//! Coverage for the default methods on the `Storage` trait.
//!
//! The real backend overrides almost every method, so without a deliberately
//! minimal backend the defaults that keep third-party storage working are
//! never executed. `Stub` implements only the required methods and leaves every
//! convenience method at its default.

use ovvc_core::error::CoreError;
use ovvc_core::model::{
    AuthorId, ChannelId, ChannelKind, ChannelMeta, ChannelView, EventBody, EventId,
    EventKind, GuildId, GuildMeta, MemberInfo, MemberProfile, SignedEvent, EVENT_VERSION,
};
use ovvc_core::{Identity, Storage, SyncCursor};

const GUILD: GuildId = GuildId([0x11; 32]);
const OTHER_GUILD: GuildId = GuildId([0x12; 32]);

struct Stub {
    guilds: Vec<GuildMeta>,
    channels: Vec<ChannelMeta>,
    members: Vec<MemberInfo>,
    events: Vec<SignedEvent>,
}

impl Default for Stub {
    fn default() -> Self {
        let author = Identity::from_seed([7u8; 32]).author_id();
        Self {
            guilds: vec![
                GuildMeta {
                    id: GUILD,
                    name: "alpha".to_string(),
                    founder: author,
                    created_at: 1,
                },
                GuildMeta {
                    id: OTHER_GUILD,
                    name: "beta".to_string(),
                    founder: author,
                    created_at: 2,
                },
            ],
            channels: Vec::new(),
            members: Vec::new(),
            events: Vec::new(),
        }
    }
}

fn message(seed: u8, lamport: u64, body: &str) -> SignedEvent {
    let identity = Identity::from_seed([seed; 32]);
    identity
        .sign_body(EventBody {
            version: EVENT_VERSION,
            guild_id: GUILD,
            channel_id: Some(ChannelId([2u8; 16])),
            author: identity.author_id(),
            seq: lamport,
            lamport,
            timestamp: lamport,
            prev: None,
            kind: EventKind::Message {
                body: body.to_string(),
            },
        })
        .unwrap()
}

impl Storage for Stub {
    fn create_guild(
        &mut self,
        _name: &str,
        _identity: &Identity,
    ) -> ovvc_core::Result<ovvc_core::CreatedGuild> {
        Err(CoreError::InvalidInput("stub create_guild".to_string()))
    }

    fn list_guilds(&self) -> ovvc_core::Result<Vec<GuildMeta>> {
        Ok(self.guilds.clone())
    }

    fn get_guild(&self, guild_id: GuildId) -> ovvc_core::Result<Option<GuildMeta>> {
        Ok(self.guilds.iter().find(|g| g.id == guild_id).cloned())
    }

    fn create_channel(
        &mut self,
        guild_id: GuildId,
        name: &str,
        identity: &Identity,
    ) -> ovvc_core::Result<ChannelMeta> {
        assert_eq!(guild_id, GUILD);
        assert_eq!(name, "new");
        assert_eq!(identity.author_id(), Identity::from_seed([9u8; 32]).author_id());
        Ok(ChannelMeta {
            id: ChannelId([3u8; 16]),
            guild_id,
            name: name.to_string(),
            created_by: identity.author_id(),
            created_at: 3,
        })
    }

    fn list_channels(&self, _guild_id: GuildId) -> ovvc_core::Result<Vec<ChannelMeta>> {
        Ok(self.channels.clone())
    }

    fn get_channel(
        &self,
        _guild_id: GuildId,
        channel_id: ChannelId,
    ) -> ovvc_core::Result<Option<ChannelMeta>> {
        Ok(self.channels.iter().find(|c| c.id == channel_id).cloned())
    }

    fn post_message(
        &mut self,
        _guild_id: GuildId,
        _channel_id: ChannelId,
        _body: &str,
        _identity: &Identity,
    ) -> ovvc_core::Result<SignedEvent> {
        Err(CoreError::InvalidInput("stub post_message".to_string()))
    }

    fn edit_message(
        &mut self,
        _guild_id: GuildId,
        _target: EventId,
        _body: &str,
        _identity: &Identity,
    ) -> ovvc_core::Result<SignedEvent> {
        Err(CoreError::InvalidInput("stub edit_message".to_string()))
    }

    fn delete_message(
        &mut self,
        _guild_id: GuildId,
        _target: EventId,
        _identity: &Identity,
    ) -> ovvc_core::Result<SignedEvent> {
        Err(CoreError::InvalidInput("stub delete_message".to_string()))
    }

    fn import_event(&mut self, event: SignedEvent) -> ovvc_core::Result<bool> {
        if self.events.iter().any(|e| e.id().unwrap() == event.id().unwrap()) {
            return Ok(false);
        }
        self.events.push(event);
        Ok(true)
    }

    fn has_event(&self, event_id: EventId) -> ovvc_core::Result<bool> {
        Ok(self.events.iter().any(|e| e.id().unwrap() == event_id))
    }

    fn events_after(
        &self,
        guild_id: GuildId,
        cursor: SyncCursor,
        limit: usize,
        _max_bytes: usize,
    ) -> ovvc_core::Result<ovvc_core::EventPage> {
        let mut events: Vec<_> = self
            .events
            .iter()
            .filter(|event| event.body.guild_id == guild_id)
            .filter(|event| cursor.is_before(event.body.lamport, event.id().unwrap()))
            .cloned()
            .collect();
        events.sort_by_key(|event| (event.body.lamport, event.id().unwrap()));
        let more = events.len() > limit;
        events.truncate(limit);
        Ok(ovvc_core::EventPage { events, more })
    }

    fn history(
        &self,
        _guild_id: GuildId,
        channel_id: ChannelId,
        limit: usize,
    ) -> ovvc_core::Result<Vec<SignedEvent>> {
        let mut events: Vec<_> = self
            .events
            .iter()
            .filter(|event| event.body.channel_id == Some(channel_id))
            .cloned()
            .collect();
        if events.len() > limit {
            events.drain(..events.len() - limit);
        }
        Ok(events)
    }

    fn history_before(
        &self,
        _guild_id: GuildId,
        channel_id: ChannelId,
        cursor: Option<SyncCursor>,
        limit: usize,
    ) -> ovvc_core::Result<ovvc_core::EventPage> {
        let mut events: Vec<_> = self
            .events
            .iter()
            .filter(|event| event.body.channel_id == Some(channel_id))
            .filter(|event| match cursor {
                Some(cursor) => cursor.is_strictly_before(event.body.lamport, event.id().unwrap()),
                None => true,
            })
            .cloned()
            .collect();
        events.sort_by_key(|event| (event.body.lamport, event.id().unwrap()));
        let more = events.len() > limit;
        if events.len() > limit {
            events.drain(..events.len() - limit);
        }
        Ok(ovvc_core::EventPage { events, more })
    }

    fn get_event(
        &self,
        _guild_id: GuildId,
        event_id: EventId,
    ) -> ovvc_core::Result<Option<SignedEvent>> {
        Ok(self.events.iter().find(|e| e.id().unwrap() == event_id).cloned())
    }

    fn guild_lamport(&self, _guild_id: GuildId) -> ovvc_core::Result<u64> {
        Ok(self.events.iter().map(|e| e.body.lamport).max().unwrap_or(0))
    }

    fn add_member(
        &mut self,
        guild_id: GuildId,
        author: AuthorId,
        display_name: &str,
    ) -> ovvc_core::Result<()> {
        self.members.push(MemberInfo {
            guild_id,
            author,
            display_name: display_name.to_string(),
            joined_at: 0,
        });
        Ok(())
    }

    fn list_members(&self, _guild_id: GuildId) -> ovvc_core::Result<Vec<MemberInfo>> {
        Ok(self.members.clone())
    }

    fn set_display_name(
        &mut self,
        _guild_id: GuildId,
        _display_name: &str,
        _identity: &Identity,
    ) -> ovvc_core::Result<Option<SignedEvent>> {
        Ok(None)
    }

    fn leave_guild(
        &mut self,
        _guild_id: GuildId,
        _identity: &Identity,
    ) -> ovvc_core::Result<Option<SignedEvent>> {
        Ok(None)
    }
}

fn assert_invalid(result: ovvc_core::Result<()>, needle: &str) {
    match result {
        Err(CoreError::InvalidInput(message) | CoreError::Backend(message)) => {
            assert!(
                message.contains(needle),
                "expected {needle:?} in {message:?}"
            );
        }
        other => panic!("expected an unsupported-feature error, got {other:?}"),
    }
}

#[test]
fn unsupported_defaults_report_a_useful_error() {
    let mut stub = Stub::default();
    let identity = Identity::from_seed([7u8; 32]);
    let guild = GUILD;
    let channel = ChannelId([2u8; 16]);
    let event = message(7, 1, "hello");
    let id = event.id().unwrap();

    assert_invalid(
        stub.create_guild_with_id(guild, "dm", "me", &identity)
            .map(|_| ()),
        "chosen id",
    );
    assert_invalid(
        stub.rename_channel(guild, channel, "renamed", &identity)
            .map(|_| ()),
        "renaming",
    );
    assert_invalid(
        stub.move_channel(guild, channel, None, &identity).map(|_| ()),
        "moving",
    );
    assert_invalid(
        stub.create_category(guild, "cat", &identity).map(|_| ()),
        "categories",
    );
    assert!(stub.list_categories(guild).unwrap().is_empty());

    assert_invalid(
        stub.create_channel_in(guild, "voice", ChannelKind::Voice, None, &identity)
            .map(|_| ()),
        "kinds or categories",
    );
    assert_invalid(
        stub.create_channel_in(guild, "child", ChannelKind::Text, Some(channel), &identity)
            .map(|_| ()),
        "kinds or categories",
    );

    assert_invalid(
        stub.add_reaction(guild, id, "👍", &identity).map(|_| ()),
        "reactions",
    );
    assert_invalid(
        stub.remove_reaction(guild, id, "👍", &identity).map(|_| ()),
        "reactions",
    );
    assert_invalid(
        stub.mark_read(guild, channel, id, &identity).map(|_| ()),
        "read markers",
    );
    assert!(stub.unread_counts(guild, identity.author_id()).unwrap().is_empty());

    assert_invalid(
        stub.append_member_joined(guild, "me", &identity).map(|_| ()),
        "event-backed membership",
    );
    assert_invalid(
        stub.set_profile(guild, &identity, "about", "they", &[])
            .map(|_| ()),
        "profiles",
    );
    assert_invalid(
        stub.member_profile(guild, identity.author_id()).map(|_| ()),
        "profiles",
    );
    assert_invalid(
        stub.announce_direct_message(guild, channel, identity.author_id(), OTHER_GUILD, &identity)
            .map(|_| ()),
        "direct messages",
    );

    assert_invalid(stub.put_blob(ovvc_core::AttachmentId([1u8; 32]), b"blob"), "blobs");
    assert_invalid(
        stub.get_blob(ovvc_core::AttachmentId([1u8; 32])).map(|_| ()),
        "blobs",
    );
    assert_invalid(
        stub.has_blob(ovvc_core::AttachmentId([1u8; 32])).map(|_| ()),
        "blobs",
    );
}

#[test]
fn post_message_full_routes_the_empty_case_to_post_message() {
    let mut stub = Stub::default();
    let identity = Identity::from_seed([7u8; 32]);
    let error = stub
        .post_message_full(GUILD, ChannelId([2u8; 16]), "hi", None, &[], &identity)
        .unwrap_err();
    assert!(matches!(error, CoreError::InvalidInput(ref m) if m == "stub post_message"));

    let error = stub
        .post_message_full(
            GUILD,
            ChannelId([2u8; 16]),
            "hi",
            None,
            &[identity.author_id()],
            &identity,
        )
        .unwrap_err();
    assert!(matches!(error, CoreError::InvalidInput(ref m) if m.contains("replies or attachments")));
}

#[test]
fn create_channel_in_delegates_plain_text_channels() {
    let mut stub = Stub::default();
    let identity = Identity::from_seed([9u8; 32]);
    let (channel, event) = stub
        .create_channel_in(GUILD, "new", ChannelKind::Text, None, &identity)
        .unwrap();
    assert_eq!(channel.name, "new");
    assert_eq!(channel.guild_id, GUILD);
    assert!(event.is_none(), "the default backend records no channel event");
}

#[test]
fn list_channel_views_defaults_to_plain_text_channels() {
    let mut stub = Stub::default();
    let author = Identity::from_seed([7u8; 32]).author_id();
    stub.channels = vec![ChannelMeta {
        id: ChannelId([4u8; 16]),
        guild_id: GUILD,
        name: "general".to_string(),
        created_by: author,
        created_at: 5,
    }];
    let views: Vec<ChannelView> = stub.list_channel_views(GUILD).unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].name, "general");
    assert_eq!(views[0].kind, ChannelKind::Text);
    assert_eq!(views[0].parent, None);
    assert_eq!(views[0].created_at, 5);
}

#[test]
fn search_default_folds_each_page_and_returns_newest_first() {
    let mut stub = Stub::default();
    for (seed, lamport, body) in [
        (7u8, 1u64, "Needle one"),
        (8, 2, "irrelevant"),
        (9, 3, "needle two"),
    ] {
        stub.import_event(message(seed, lamport, body)).unwrap();
    }

    let hits = stub.search(GUILD, "  NEEDLE ", 10).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].body, "needle two");
    assert_eq!(hits[1].body, "Needle one");

    let limited = stub.search(GUILD, "needle", 1).unwrap();
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].body, "needle two");

    assert!(stub.search(GUILD, "   ", 10).unwrap().is_empty());
    assert!(stub.search(GUILD, "needle", 0).unwrap().is_empty());
    assert!(stub.search(OTHER_GUILD, "needle", 10).unwrap().is_empty());
}

#[test]
fn find_guild_resolves_exact_name_hex_and_prefix() {
    let stub = Stub::default();

    assert_eq!(stub.find_guild("alpha").unwrap().id, GUILD);
    assert_eq!(stub.find_guild("  beta ").unwrap().id, OTHER_GUILD);
    assert_eq!(stub.find_guild(&GUILD.to_hex()).unwrap().id, GUILD);
    assert_eq!(stub.find_guild("1111").unwrap().id, GUILD);

    let missing = stub.find_guild("gamma").unwrap_err();
    assert!(matches!(missing, CoreError::NotFound(ref m) if m.contains("gamma")));
}

#[test]
fn find_guild_rejects_an_ambiguous_prefix() {
    let mut stub = Stub::default();
    let founder = Identity::from_seed([7u8; 32]).author_id();
    let mut first = [0u8; 32];
    first[0] = 0x21;
    first[1] = 0x01;
    let mut second = first;
    second[1] = 0x02;
    stub.guilds = vec![
        GuildMeta {
            id: GuildId(first),
            name: "one".to_string(),
            founder,
            created_at: 1,
        },
        GuildMeta {
            id: GuildId(second),
            name: "two".to_string(),
            founder,
            created_at: 2,
        },
    ];
    let ambiguous = stub.find_guild("21").unwrap_err();
    assert!(matches!(ambiguous, CoreError::InvalidInput(ref m) if m.contains("ambiguous")));
}

#[test]
fn find_channel_prefers_names_then_prefixes() {
    let mut stub = Stub::default();
    let author = Identity::from_seed([7u8; 32]).author_id();
    stub.channels = vec![
        ChannelMeta {
            id: ChannelId([0x31; 16]),
            guild_id: GUILD,
            name: "general".to_string(),
            created_by: author,
            created_at: 1,
        },
        ChannelMeta {
            id: ChannelId([0x32; 16]),
            guild_id: GUILD,
            name: "random".to_string(),
            created_by: author,
            created_at: 2,
        },
    ];
    assert_eq!(stub.find_channel(GUILD, "#general").unwrap().name, "general");
    assert_eq!(stub.find_channel(GUILD, "32").unwrap().name, "random");

    let missing = stub.find_channel(GUILD, "nope").unwrap_err();
    assert!(matches!(missing, CoreError::NotFound(ref m) if m.contains("nope")));
}

#[test]
fn sync_cursor_orders_by_lamport_then_event_id() {
    let first = message(7, 5, "a");
    let second = message(8, 5, "b");
    let (first_id, second_id) = (first.id().unwrap(), second.id().unwrap());
    let (low, high) = if first_id < second_id {
        (first_id, second_id)
    } else {
        (second_id, first_id)
    };

    let start = SyncCursor::start();
    assert!(start.is_before(0, low));
    assert!(!start.is_strictly_before(0, low));

    let at_low = SyncCursor {
        lamport: 5,
        event_id: Some(low),
    };
    assert!(at_low.is_before(5, high));
    assert!(!at_low.is_before(5, low));
    assert!(!at_low.is_before(4, high));
    assert!(!at_low.is_strictly_before(5, high));
    assert!(at_low.is_strictly_before(4, high));
    assert!(!at_low.is_strictly_before(5, low));

    let after = SyncCursor::after(&first).unwrap();
    assert_eq!(after, SyncCursor::at(&first).unwrap());
    assert_eq!(after.lamport, 5);
}

#[test]
fn stub_default_profiles_and_membership_round_trip() {
    let mut stub = Stub::default();
    let identity = Identity::from_seed([7u8; 32]);
    stub.add_member(GUILD, identity.author_id(), "me").unwrap();
    let members = stub.list_members(GUILD).unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].display_name, "me");
    assert!(stub.set_display_name(GUILD, "you", &identity).unwrap().is_none());
    assert!(stub.leave_guild(GUILD, &identity).unwrap().is_none());

    // The derived profile is an unsupported default, not a stub failure.
    let profile: ovvc_core::Result<MemberProfile> = stub.member_profile(GUILD, identity.author_id());
    assert!(profile.is_err());
}
