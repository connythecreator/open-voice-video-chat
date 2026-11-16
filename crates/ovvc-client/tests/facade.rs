use ovvc_client::{
    Client, ClientConfig, ClientEvent, EventKind, Identity, Invite, NetworkOptions, StorageConfig,
    Store, SyncCursor, shared_storage,
};

#[test]
fn offline_lifecycle_through_the_client_api() {
    let dir = tempfile::tempdir().unwrap();
    let client = Client::open_local(dir.path()).unwrap();

    let guild = client.create_guild("frontends").unwrap();
    let channel = client.default_channel(guild.id).unwrap();
    assert_eq!(channel.name, "general");

    let first = client.post_message(guild.id, channel.id, "hello").unwrap();
    assert_eq!(first.body.author, client.author_id());

    client.post_message(guild.id, channel.id, "world").unwrap();

    let history: Vec<_> = client
        .history(guild.id, channel.id, 10)
        .unwrap()
        .into_iter()
        .filter(|event| matches!(event.body.kind, EventKind::Message { .. }))
        .collect();
    assert_eq!(history.len(), 2);
    assert!(history.iter().all(|event| event.verify().is_ok()));

    // Lookups by name and by id prefix go through the Storage defaults.
    assert_eq!(client.find_guild("frontends").unwrap().id, guild.id);
    assert_eq!(
        client.find_channel(guild.id, "general").unwrap().id,
        channel.id
    );
    assert_eq!(client.list_members(guild.id).unwrap().len(), 1);
}

#[test]
fn custom_storage_backend_is_pluggable() {
    let dir = tempfile::tempdir().unwrap();
    let backend = shared_storage(Store::open(dir.path()).unwrap());

    let client = Client::open(ClientConfig {
        identity: Identity::from_seed([7u8; 32]),
        storage: StorageConfig::Custom(backend.clone()),
    })
    .unwrap();

    let guild = client.create_guild("custom").unwrap();
    let channel = client.default_channel(guild.id).unwrap();
    client
        .post_message(guild.id, channel.id, "via trait object")
        .unwrap();

    // The same backend, viewed through the trait, sees the same events.
    let events = backend
        .lock()
        .unwrap()
        .history(guild.id, channel.id, 10)
        .unwrap();
    assert_eq!(events.len(), 2);
}

#[test]
fn invites_roundtrip_through_the_facade() {
    let dir = tempfile::tempdir().unwrap();
    let client = Client::open_local(dir.path()).unwrap();
    let guild = client.create_guild("invites").unwrap();

    let peer = "/ip4/127.0.0.1/tcp/4001/p2p/12D3KooWExampleExampleExampleExampleExample";
    let invite = client
        .create_invite(guild.id, vec![peer.to_string()], Some(3600))
        .unwrap();
    let decoded = Invite::decode(&invite.encode().unwrap()).unwrap();
    decoded.verify().unwrap();
    assert_eq!(decoded.body.guild_id, guild.id);
    assert_eq!(decoded.body.guild_name, "invites");
}

/// Joining a guild this client is already online in cannot emit a fresh
/// `Synced` (the node already exists), so it must succeed immediately instead
/// of waiting out the join timeout and tearing the healthy node down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn joining_an_already_connected_guild_keeps_the_node() {
    use std::time::{Duration, Instant};

    let alice_dir = tempfile::tempdir().unwrap();
    let bob_dir = tempfile::tempdir().unwrap();
    let alice = Client::open_local(alice_dir.path()).unwrap();
    let bob = Client::open_local(bob_dir.path()).unwrap();
    let guild = alice.create_guild("rejoin").unwrap();

    let mut events = alice.subscribe();
    alice
        .connect(
            guild.id,
            NetworkOptions {
                listen: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
                ..NetworkOptions::default()
            },
        )
        .await
        .unwrap();
    let listen = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(ClientEvent::Listening { guild_id, address }) if guild_id == guild.id => {
                    break address;
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("event stream closed")
                }
            }
        }
    })
    .await
    .expect("listener timeout");

    let peer_id = alice.peer_id(guild.id).unwrap();
    let invite = alice
        .create_invite(guild.id, vec![format!("{listen}/p2p/{peer_id}")], None)
        .unwrap();

    let joined = bob
        .join_guild(&invite, NetworkOptions::default())
        .await
        .unwrap();
    assert_eq!(joined.id, guild.id);
    assert!(bob.is_connected(guild.id));

    let started = Instant::now();
    let rejoined = bob
        .join_guild(&invite, NetworkOptions::default())
        .await
        .unwrap();
    assert_eq!(rejoined.id, guild.id);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(bob.is_connected(guild.id));
}

#[test]
fn edits_and_deletes_flow_through_the_facade() {
    use ovvc_client::{TimelineEntry, timeline};

    let dir = tempfile::tempdir().unwrap();
    let client = Client::open_local(dir.path()).unwrap();
    let guild = client.create_guild("editing").unwrap();
    let channel = client.default_channel(guild.id).unwrap();

    let message = client.post_message(guild.id, channel.id, "draft").unwrap();
    let target = message.id().unwrap();
    let edit = client
        .edit_message(guild.id, target, "final version")
        .unwrap();
    assert!(matches!(
        edit.body.kind,
        EventKind::MessageEdited { target: id, .. } if id == target
    ));

    let history = client.history(guild.id, channel.id, 50).unwrap();
    let entries = timeline(&history).unwrap();
    let body = entries
        .iter()
        .find_map(|entry| match entry {
            TimelineEntry::Message { body, edited, .. } => Some((body.as_str(), *edited)),
            _ => None,
        })
        .unwrap();
    assert_eq!(body, ("final version", true));

    let delete = client.delete_message(guild.id, target).unwrap();
    assert!(matches!(
        delete.body.kind,
        EventKind::MessageDeleted { target: id } if id == target
    ));

    let history = client.history(guild.id, channel.id, 50).unwrap();
    let entries = timeline(&history).unwrap();
    assert!(
        entries
            .iter()
            .any(|entry| matches!(entry, TimelineEntry::Deleted { .. }))
    );
}

/// Local writes are echoed on the client event stream so a frontend does not
/// have to poll storage after every action.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_edits_are_echoed_on_the_event_stream() {
    let dir = tempfile::tempdir().unwrap();
    let client = Client::open_local(dir.path()).unwrap();
    let guild = client.create_guild("echo").unwrap();
    let channel = client.default_channel(guild.id).unwrap();
    let message = client.post_message(guild.id, channel.id, "draft").unwrap();
    let target = message.id().unwrap();

    let mut events = client.subscribe();
    let edit = client.edit_message(guild.id, target, "final").unwrap();
    let echoed = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
        .await
        .expect("local echo timeout")
        .expect("event stream open");
    match echoed {
        ClientEvent::Event { guild_id, event } => {
            assert_eq!(guild_id, guild.id);
            assert_eq!(event.id().unwrap(), edit.id().unwrap());
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn replies_and_mentions_flow_through_the_facade() {
    use ovvc_client::{TimelineEntry, parse_mentions, timeline};

    let dir = tempfile::tempdir().unwrap();
    let client = Client::open_local(dir.path()).unwrap();
    let guild = client.create_guild("threads").unwrap();
    let channel = client.default_channel(guild.id).unwrap();
    let original = client
        .post_message(guild.id, channel.id, "question")
        .unwrap();
    let target = original.id().unwrap();

    // The parser resolves a name through the roster, and the facade stores the
    // resulting id rather than the text the sender typed.
    let roster = client.list_members(guild.id).unwrap();
    let name = roster
        .iter()
        .find(|member| member.author == client.author_id())
        .expect("the founder is on the roster")
        .display_name
        .clone();
    let mentions = parse_mentions(&format!("answer for @{name}"), &roster);
    assert_eq!(mentions, vec![client.author_id()]);

    let reply = client
        .post_message_full(
            guild.id,
            channel.id,
            &format!("answer for @{name}"),
            Some(target),
            &mentions,
        )
        .unwrap();
    assert_eq!(
        reply.body.kind,
        EventKind::MessageV2 {
            body: format!("answer for @{name}"),
            reply_to: Some(target),
            mentions: vec![client.author_id()],
        }
    );

    let history = client.history(guild.id, channel.id, 50).unwrap();
    let entries = timeline(&history).unwrap();
    let expected = format!("answer for @{name}");
    let (body, reference, mentioned) = entries
        .iter()
        .find_map(|entry| match entry {
            TimelineEntry::Message {
                body,
                reply_to,
                mentions,
                ..
            } if *body == expected => Some((body.as_str(), reply_to.as_ref(), *mentions)),
            _ => None,
        })
        .expect("the reply is in the timeline");
    assert_eq!(body, expected);
    let reference = reference.expect("the reply resolves its target");
    assert_eq!(reference.body.as_deref(), Some("question"));
    assert_eq!(mentioned, [client.author_id()]);
}

#[test]
fn direct_lookups_and_cursor_paging() {
    let dir = tempfile::tempdir().unwrap();
    let client = Client::open_local(dir.path()).unwrap();
    let guild = client.create_guild("lookups").unwrap();
    let channel = client.default_channel(guild.id).unwrap();
    client.post_message(guild.id, channel.id, "one").unwrap();
    client.post_message(guild.id, channel.id, "two").unwrap();

    assert_eq!(
        client
            .get_channel(guild.id, channel.id)
            .unwrap()
            .unwrap()
            .id,
        channel.id
    );
    assert!(
        client
            .get_channel(guild.id, ovvc_client::ChannelId([9u8; 16]))
            .unwrap()
            .is_none()
    );
    assert!(
        client
            .get_member(guild.id, client.author_id())
            .unwrap()
            .is_some()
    );
    assert!(
        client
            .get_member(guild.id, ovvc_client::AuthorId([3u8; 32]))
            .unwrap()
            .is_none()
    );

    let all = client.events_since(guild.id, None, 100).unwrap();
    assert!(
        all.events.len() >= 4,
        "guild metadata, channel, and messages"
    );
    let cursor = SyncCursor::after(all.events.first().unwrap()).unwrap();
    let page = client.events_since(guild.id, Some(cursor), 1).unwrap();
    assert_eq!(page.events.len(), 1);
    assert!(page.more);
    let tail = client.events_since(guild.id, Some(cursor), 100).unwrap();
    assert_eq!(
        tail.events.len(),
        all.events.len() - 1,
        "strictly after the cursor"
    );
    assert!(!tail.more);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_state_tracks_the_node_lifecycle() {
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    let client = Client::open_local(dir.path()).unwrap();
    let guild = client.create_guild("status").unwrap();

    let offline = client.connection_state(guild.id);
    assert!(!offline.connected);
    assert!(offline.peer_id.is_none());
    assert!(offline.listen_addresses.is_empty());
    assert!(client.connected_peers(guild.id).is_empty());

    client
        .connect(
            guild.id,
            NetworkOptions {
                listen: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
                ..NetworkOptions::default()
            },
        )
        .await
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let online = loop {
        let state = client.connection_state(guild.id);
        if !state.listen_addresses.is_empty() {
            break state;
        }
        assert!(Instant::now() < deadline, "no listen address appeared");
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert!(online.connected);
    assert_eq!(online.peer_id, client.peer_id(guild.id));
    assert_eq!(
        client.listen_addresses(guild.id),
        online.listen_addresses,
        "the convenience accessor reads the same snapshot"
    );

    client.shutdown();
    assert!(!client.is_connected(guild.id));
    assert!(!client.connection_state(guild.id).connected);
}

#[test]
fn blobs_roundtrip_through_the_client() {
    let dir = tempfile::tempdir().unwrap();
    let client = Client::open_local(dir.path()).unwrap();
    let bytes = b"the bytes a message would announce";
    let id = client.put_blob(bytes).unwrap();
    assert_eq!(id, ovvc_client::hash_bytes(bytes));
    assert!(client.has_blob(id).unwrap());
    assert_eq!(client.get_blob(id).unwrap().as_deref(), Some(&bytes[..]));

    let missing = ovvc_client::hash_bytes(b"never stored");
    assert!(!client.has_blob(missing).unwrap());
    assert_eq!(client.get_blob(missing).unwrap(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_blob_uses_the_local_copy_before_the_network() {
    let dir = tempfile::tempdir().unwrap();
    let client = Client::open_local(dir.path()).unwrap();
    let guild = client.create_guild("blob fetch").unwrap();
    let bytes = b"already here";
    let id = client.put_blob(bytes).unwrap();

    // No node is running, but a local blob is returned without one.
    assert_eq!(client.fetch_blob(guild.id, id).await.unwrap(), bytes);

    let missing = ovvc_client::hash_bytes(b"never here");
    let error = client.fetch_blob(guild.id, missing).await.unwrap_err();
    assert!(
        matches!(error, ovvc_client::ClientError::NotConnected(id) if id == guild.id),
        "an uncached blob without a node must report NotConnected, got {error:?}"
    );
}

#[test]
fn attachments_are_posted_through_the_client() {
    let dir = tempfile::tempdir().unwrap();
    let client = Client::open_local(dir.path()).unwrap();
    let guild = client.create_guild("attachments").unwrap();
    let channel = client.default_channel(guild.id).unwrap();

    let bytes = b"a picture of nothing in particular";
    let id = client.put_blob(bytes).unwrap();
    let attachment = ovvc_client::Attachment {
        hash: id,
        size: bytes.len() as u64,
        mime: "image/png".to_string(),
        name: "nothing.png".to_string(),
    };
    let event = client
        .post_message_rich(
            guild.id,
            channel.id,
            "look",
            None,
            &[],
            std::slice::from_ref(&attachment),
        )
        .unwrap();
    assert_eq!(
        event.body.kind,
        EventKind::MessageV3 {
            body: "look".to_string(),
            reply_to: None,
            mentions: Vec::new(),
            attachments: vec![attachment.clone()],
        }
    );
    assert_eq!(
        event.body.kind.attachments(),
        std::slice::from_ref(&attachment)
    );
    assert!(client.has_blob(id).unwrap());

    // A caller that never stored the bytes cannot announce them: peers would
    // ask this node first and be refused.
    let unknown = ovvc_client::Attachment {
        hash: ovvc_client::hash_bytes(b"never put"),
        size: 9,
        mime: "text/plain".to_string(),
        name: "missing.txt".to_string(),
    };
    let error = client
        .post_message_rich(
            guild.id,
            channel.id,
            "bad",
            None,
            &[],
            std::slice::from_ref(&unknown),
        )
        .unwrap_err();
    assert!(
        matches!(error, ovvc_client::ClientError::Core(_)),
        "an unservable attachment must be a core input error, got {error:?}"
    );

    // A size that does not match the stored bytes is just as unservable.
    let lying = ovvc_client::Attachment {
        size: 1,
        ..attachment
    };
    assert!(
        client
            .post_message_rich(
                guild.id,
                channel.id,
                "lie",
                None,
                &[],
                std::slice::from_ref(&lying),
            )
            .is_err()
    );
}

/// A peer that answers Hello for another guild must not be redialled on every
/// Identify and mDNS report. Before the quarantine and dial cooldown, two
/// guilds on one host connected, refused, disconnected and reconnected
/// hundreds of times a second, which burned a core and made the daemon look
/// busy while showing no peers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peers_in_different_guilds_do_not_dial_storm() {
    use std::time::Duration;

    let alice_dir = tempfile::tempdir().unwrap();
    let bob_dir = tempfile::tempdir().unwrap();
    let alice = Client::open_local(alice_dir.path()).unwrap();
    let bob = Client::open_local(bob_dir.path()).unwrap();
    let alpha = alice.create_guild("alpha").unwrap();
    let beta = bob.create_guild("beta").unwrap();

    let mut alice_events = alice.subscribe();
    alice
        .connect(
            alpha.id,
            NetworkOptions {
                listen: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
                ..NetworkOptions::default()
            },
        )
        .await
        .unwrap();
    let listen = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match alice_events.recv().await {
                Ok(ClientEvent::Listening { guild_id, address }) if guild_id == alpha.id => {
                    break address;
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("event stream closed")
                }
            }
        }
    })
    .await
    .expect("listener timeout");

    let peer = alice.peer_id(alpha.id).unwrap();
    let mut bob_events = bob.subscribe();
    bob.connect(
        beta.id,
        NetworkOptions {
            listen: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
            dial: vec![format!("{listen}/p2p/{peer}").parse().unwrap()],
            ..NetworkOptions::default()
        },
    )
    .await
    .unwrap();

    // Let the mismatch, the disconnect and any follow-up discovery happen.
    let started = std::time::Instant::now();
    let mut connections = 0u32;
    while started.elapsed() < Duration::from_secs(3) {
        match tokio::time::timeout(Duration::from_millis(250), bob_events.recv()).await {
            Ok(Ok(ClientEvent::PeerConnected { .. })) => connections += 1,
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                panic!("event stream closed")
            }
            Err(_) => {}
        }
    }
    eprintln!("guilds in different guilds: {connections} peer connections in three seconds");
    assert!(
        connections <= 8,
        "two guilds redialled each other {connections} times in three seconds"
    );

    // The node must still answer local work while all that is happening.
    let channel = bob.default_channel(beta.id).unwrap();
    bob.post_message(beta.id, channel.id, "still responsive")
        .unwrap();
}

/// Profiles are published through the facade, resolved back from the log, and
/// echoed on the event stream so an attached frontend refreshes without
/// polling. The display name stays on `set_display_name`.
#[test]
fn profiles_are_published_and_resolved_through_the_facade() {
    let dir = tempfile::tempdir().unwrap();
    let client = Client::open_local(dir.path()).unwrap();
    let guild = client.create_guild("profiles").unwrap();
    client.set_display_name(guild.id, "Alice").unwrap();

    let event = client
        .set_profile(
            guild.id,
            "peer-to-peer chat",
            "she/her",
            &["https://example.com".to_string()],
        )
        .unwrap();
    assert_eq!(
        event.body.kind,
        EventKind::MemberProfileUpdated {
            about: "peer-to-peer chat".to_string(),
            pronouns: "she/her".to_string(),
            links: vec!["https://example.com".to_string()],
        }
    );

    let profile = client
        .member_profile(guild.id, client.author_id())
        .unwrap();
    assert_eq!(profile.display_name, "Alice");
    assert_eq!(profile.about, "peer-to-peer chat");
    assert_eq!(profile.pronouns, "she/her");
    assert_eq!(profile.links, ["https://example.com"]);
    assert_eq!(profile.updated_at, event.body.timestamp);

    let outsider = Identity::from_seed([99u8; 32]).author_id();
    assert!(
        client.member_profile(guild.id, outsider).is_err(),
        "only a member has a profile row"
    );
    let too_long = "x".repeat(ovvc_client::MAX_PROFILE_ABOUT_BYTES + 1);
    assert!(client.set_profile(guild.id, &too_long, "", &[]).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profile_updates_are_echoed_on_the_event_stream() {
    let dir = tempfile::tempdir().unwrap();
    let client = Client::open_local(dir.path()).unwrap();
    let guild = client.create_guild("profile echo").unwrap();
    let mut events = client.subscribe();

    let update = client
        .set_profile(guild.id, "echoed", "they/them", &[])
        .unwrap();
    let echoed = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
        .await
        .expect("local echo timeout")
        .expect("event stream open");
    match echoed {
        ClientEvent::Event { guild_id, event } => {
            assert_eq!(guild_id, guild.id);
            assert_eq!(event.id().unwrap(), update.id().unwrap());
            assert!(matches!(
                event.body.kind,
                EventKind::MemberProfileUpdated { .. }
            ));
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

/// Two clients converge on one direct-message guild derived from their keys,
/// then exchange a message, an edit, and a delete. This exercises the whole
/// path: the announcement in the shared guild, the recipient joining the
/// opener's log, and ordinary message events inside the DM guild.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_messages_converge_over_two_nodes() {
    use std::time::{Duration, Instant};

    use ovvc_client::{DirectConversation, Multiaddr, TimelineEntry, direct_guild_id, timeline};

    use tokio::sync::broadcast::error::RecvError;

    async fn next_listen(
        events: &mut tokio::sync::broadcast::Receiver<ClientEvent>,
        guild_id: ovvc_client::GuildId,
    ) -> Multiaddr {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match events.recv().await {
                    Ok(ClientEvent::Listening {
                        guild_id: seen,
                        address,
                    }) if seen == guild_id => break address,
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => panic!("event stream closed"),
                }
            }
        })
        .await
        .expect("listener timeout")
    }

    async fn wait_until(
        mut condition: impl FnMut() -> bool,
        budget: Duration,
        what: &str,
    ) {
        let deadline = Instant::now() + budget;
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    let alice_dir = tempfile::tempdir().unwrap();
    let bob_dir = tempfile::tempdir().unwrap();
    let alice = Client::open_local(alice_dir.path()).unwrap();
    let bob = Client::open_local(bob_dir.path()).unwrap();
    let guild = alice.create_guild("shared").unwrap();
    alice.set_display_name(guild.id, "Alice").unwrap();

    // Alice is online; Bob joins through an invite.
    let mut alice_events = alice.subscribe();
    alice
        .connect(
            guild.id,
            NetworkOptions {
                listen: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
                ..NetworkOptions::default()
            },
        )
        .await
        .unwrap();
    let listen = next_listen(&mut alice_events, guild.id).await;
    let alice_peer = alice.peer_id(guild.id).unwrap();
    let invite = alice
        .create_invite(guild.id, vec![format!("{listen}/p2p/{alice_peer}")], None)
        .unwrap();
    bob.join_guild(
        &invite,
        NetworkOptions {
            listen: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
            ..NetworkOptions::default()
        },
    )
    .await
    .unwrap();
    bob.set_display_name(guild.id, "Bob").unwrap();
    wait_until(
        || {
            alice
                .list_members(guild.id)
                .map(|members| members.len() == 2)
                .unwrap_or(false)
        },
        Duration::from_secs(20),
        "both members on the shared roster",
    )
    .await;
    assert!(
        !alice
            .list_direct_messages()
            .unwrap()
            .iter()
            .any(|DirectConversation { guild, .. }| guild.name == "Bob"),
        "an ordinary guild is not a direct conversation"
    );

    // Both DM nodes are started here with explicit addresses. The clients
    // would start them anyway (the opener in `open_direct_message`, the
    // recipient when the announcement imports), but pinning the port keeps
    // this convergence test independent of mDNS on a busy test host. The
    // auto-start path is covered by the next test.
    let dm_id = direct_guild_id(&alice.author_id(), &bob.author_id());
    let mut alice_dm_events = alice.subscribe();
    alice
        .connect(
            dm_id,
            NetworkOptions {
                listen: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
                ..NetworkOptions::default()
            },
        )
        .await
        .unwrap();
    let dm_listen = next_listen(&mut alice_dm_events, dm_id).await;
    let alice_dm_peer = alice.peer_id(dm_id).unwrap();
    bob.connect(
        dm_id,
        NetworkOptions {
            listen: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
            dial: vec![format!("{dm_listen}/p2p/{alice_dm_peer}").parse().unwrap()],
            ..NetworkOptions::default()
        },
    )
    .await
    .unwrap();
    let bob_dm_peer = bob.peer_id(dm_id).unwrap();
    wait_until(
        || alice.connected_peers(dm_id).contains(&bob_dm_peer),
        Duration::from_secs(20),
        "the two DM nodes to connect",
    )
    .await;

    let dm = alice.open_direct_message(guild.id, bob.author_id()).await.unwrap();
    assert_eq!(dm.id, dm_id);
    assert!(
        alice.direct_peer(guild.id).unwrap().is_none(),
        "the shared guild is not a DM"
    );

    // The recipient learns the guild from the opener's broadcast, joins it,
    // and the join travels back.
    wait_until(
        || bob.get_guild(dm_id).map(|guild| guild.is_some()).unwrap_or(false),
        Duration::from_secs(60),
        "the recipient to receive the DM guild",
    )
    .await;
    wait_until(
        || {
            bob.list_members(dm_id)
                .map(|members| members.len() == 2)
                .unwrap_or(false)
        },
        Duration::from_secs(30),
        "the recipient to join the DM guild",
    )
    .await;
    wait_until(
        || {
            alice
                .list_members(dm_id)
                .map(|members| members.len() == 2)
                .unwrap_or(false)
        },
        Duration::from_secs(30),
        "the opener to see the recipient's join",
    )
    .await;

    let alice_dms = alice.list_direct_messages().unwrap();
    assert_eq!(alice_dms.len(), 1);
    assert_eq!(alice_dms[0].guild.id, dm_id);
    assert_eq!(alice_dms[0].peer.author, bob.author_id());
    assert_eq!(alice_dms[0].peer.display_name, "Bob");
    assert_eq!(
        alice.direct_peer(dm_id).unwrap(),
        Some(bob.author_id()),
        "the opener sees the pair"
    );
    let bob_dms = bob.list_direct_messages().unwrap();
    assert_eq!(bob_dms.len(), 1);
    assert_eq!(bob_dms[0].peer.author, alice.author_id());
    assert_eq!(bob.direct_peer(dm_id).unwrap(), Some(alice.author_id()));

    // Messages, edits and deletes inside the DM guild travel the normal path.
    let channel = alice.default_channel(dm_id).unwrap();
    wait_until(
        || {
            bob.get_channel(dm_id, channel.id)
                .map(|channel| channel.is_some())
                .unwrap_or(false)
        },
        Duration::from_secs(20),
        "the recipient to import the DM channel",
    )
    .await;
    let message = alice
        .post_message(dm_id, channel.id, "hello over the dm")
        .unwrap();
    let target = message.id().unwrap();
    wait_until(
        || {
            bob.history(dm_id, channel.id, 50)
                .map(|history| {
                    history.iter().any(|event| {
                        matches!(
                            &event.body.kind,
                            EventKind::Message { body } if body == "hello over the dm"
                        )
                    })
                })
                .unwrap_or(false)
        },
        Duration::from_secs(20),
        "the recipient to import the first message",
    )
    .await;

    bob.post_message(dm_id, channel.id, "hi alice")
        .unwrap();
    wait_until(
        || {
            alice
                .history(dm_id, channel.id, 50)
                .map(|history| {
                    history.iter().any(|event| {
                        matches!(
                            &event.body.kind,
                            EventKind::Message { body } if body == "hi alice"
                        )
                    })
                })
                .unwrap_or(false)
        },
        Duration::from_secs(20),
        "the opener to import the reply",
    )
    .await;

    alice
        .edit_message(dm_id, target, "hello, edited")
        .unwrap();
    wait_until(
        || {
            bob.history(dm_id, channel.id, 50)
                .map(|history| {
                    timeline(&history).map(|entries| {
                        entries.iter().any(|entry| {
                            matches!(
                                entry,
                                TimelineEntry::Message { body, edited: true, .. }
                                    if body.as_str() == "hello, edited"
                            )
                        })
                    })
                })
                .unwrap_or(Ok(false))
                .unwrap_or(false)
        },
        Duration::from_secs(20),
        "the edit to converge",
    )
    .await;

    alice.delete_message(dm_id, target).unwrap();
    wait_until(
        || {
            bob.history(dm_id, channel.id, 50)
                .map(|history| {
                    timeline(&history).map(|entries| {
                        entries
                            .iter()
                            .any(|entry| matches!(entry, TimelineEntry::Deleted { .. }))
                    })
                })
                .unwrap_or(Ok(false))
                .unwrap_or(false)
        },
        Duration::from_secs(20),
        "the delete to converge",
    )
    .await;

    alice.shutdown();
    bob.shutdown();
}

/// Starting the DM nodes is part of the protocol, not of a caller's setup:
/// the opener starts one inside `open_direct_message`, and the recipient
/// starts one when the imported announcement reaches its event loop. This
/// checks both without depending on the two ports finding each other, which
/// the convergence test above covers with explicit dial addresses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn opening_and_receiving_a_direct_message_starts_the_nodes() {
    use std::time::{Duration, Instant};

    use ovvc_client::{Multiaddr, direct_guild_id};
    use tokio::sync::broadcast::error::RecvError;

    async fn wait_until(mut condition: impl FnMut() -> bool, budget: Duration, what: &str) {
        let deadline = Instant::now() + budget;
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    let alice_dir = tempfile::tempdir().unwrap();
    let bob_dir = tempfile::tempdir().unwrap();
    let alice = Client::open_local(alice_dir.path()).unwrap();
    let bob = Client::open_local(bob_dir.path()).unwrap();
    let guild = alice.create_guild("dm autostart").unwrap();

    let mut alice_events = alice.subscribe();
    alice
        .connect(
            guild.id,
            NetworkOptions {
                listen: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
                ..NetworkOptions::default()
            },
        )
        .await
        .unwrap();
    let listen: Multiaddr = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match alice_events.recv().await {
                Ok(ClientEvent::Listening { guild_id, address }) if guild_id == guild.id => {
                    break address;
                }
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => panic!("event stream closed"),
            }
        }
    })
    .await
    .expect("listener timeout");
    let alice_peer = alice.peer_id(guild.id).unwrap();
    let invite = alice
        .create_invite(guild.id, vec![format!("{listen}/p2p/{alice_peer}")], None)
        .unwrap();
    bob.join_guild(
        &invite,
        NetworkOptions {
            listen: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
            ..NetworkOptions::default()
        },
    )
    .await
    .unwrap();
    // The join itself must put the new member on both rosters: a silent
    // joiner authors no other event, and without the explicit `MemberJoined`
    // nobody could see their profile or open a direct message with them.
    wait_until(
        || {
            alice
                .list_members(guild.id)
                .map(|members| members.iter().any(|member| member.author == bob.author_id()))
                .unwrap_or(false)
        },
        Duration::from_secs(20),
        "the joiner on the founder's roster before any rename",
    )
    .await;
    assert!(
        bob.list_members(guild.id)
            .unwrap()
            .iter()
            .any(|member| member.author == bob.author_id()),
        "the joiner must be on its own roster"
    );

    bob.set_display_name(guild.id, "Bob").unwrap();
    alice.set_display_name(guild.id, "Alice").unwrap();
    wait_until(
        || {
            alice
                .list_members(guild.id)
                .map(|members| members.len() == 2)
                .unwrap_or(false)
        },
        Duration::from_secs(20),
        "both members on the shared roster",
    )
    .await;

    let dm_id = direct_guild_id(&alice.author_id(), &bob.author_id());
    assert!(!alice.is_connected(dm_id) && !bob.is_connected(dm_id));
    let dm = alice.open_direct_message(guild.id, bob.author_id()).await.unwrap();
    assert_eq!(dm.id, dm_id);
    assert!(
        alice.is_connected(dm_id),
        "the opener must start the DM node before returning"
    );
    wait_until(
        || bob.is_connected(dm_id),
        Duration::from_secs(20),
        "the recipient to start the DM node from the announcement",
    )
    .await;

    alice.shutdown();
    bob.shutdown();
}

/// Build a shared guild and roster entirely offline: Alice creates it, Bob
/// imports Alice's events, Bob appends his own signed join, and Alice imports
/// that join. No node is started, so the DM tests below control exactly which
/// events each store has seen.
fn offline_shared_guild(alice: &Client, bob: &Client) -> ovvc_client::GuildMeta {
    let guild = alice.create_guild("shared offline").unwrap();
    alice.set_display_name(guild.id, "Alice").unwrap();
    let context_events = alice.events_since(guild.id, None, 100).unwrap().events;
    let join = {
        let bob_storage = bob.storage();
        let mut bob_store = bob_storage.lock().unwrap();
        for event in context_events {
            bob_store.import_event(event).unwrap();
        }
        bob_storage_append_join(&mut *bob_store, guild.id, bob)
    };
    alice.storage().lock().unwrap().import_event(join).unwrap();
    guild
}

fn bob_storage_append_join(
    store: &mut dyn ovvc_client::Storage,
    guild_id: ovvc_client::GuildId,
    bob: &Client,
) -> ovvc_client::SignedEvent {
    store
        .append_member_joined(guild_id, "Bob", bob.identity())
        .unwrap()
        .expect("a fresh guild must accept the first join")
}

/// The opener's sidebar must not lose a DM between `open_direct_message` and
/// the recipient's join: the DM guild has one member at that point, so the
/// roster check alone would hide it and only the announcement knows Bob.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dm_opened_before_the_peer_joins_is_still_listed() {
    use ovvc_client::direct_guild_id;

    let alice_dir = tempfile::tempdir().unwrap();
    let bob_dir = tempfile::tempdir().unwrap();
    let alice = Client::open_local(alice_dir.path()).unwrap();
    let bob = Client::open_local(bob_dir.path()).unwrap();
    let guild = offline_shared_guild(&alice, &bob);

    let dm_id = direct_guild_id(&alice.author_id(), &bob.author_id());
    let dm = alice
        .open_direct_message(guild.id, bob.author_id())
        .await
        .unwrap();
    assert_eq!(dm.id, dm_id);
    assert_eq!(
        alice.list_members(dm_id).unwrap().len(),
        1,
        "the peer has not joined yet, so the roster cannot carry the pair"
    );

    let direct = alice.list_direct_messages().unwrap();
    assert_eq!(direct.len(), 1);
    assert_eq!(direct[0].guild.id, dm_id);
    assert_eq!(direct[0].peer.author, bob.author_id());
    assert_eq!(direct[0].peer.display_name, "Bob");
    assert_eq!(
        alice.direct_peer(dm_id).unwrap(),
        Some(bob.author_id()),
        "the announcement supplies the peer until the roster does"
    );

    alice.shutdown();
    bob.shutdown();
}

/// The recipient must list the conversation as soon as the announcement
/// imports, before any of the opener's DM log has arrived and while no local
/// DM guild exists at all. The result must be identical after a restart,
/// because it is derived from stored events rather than in-memory state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_received_dm_is_listed_before_the_opener_log_syncs() {
    use ovvc_client::direct_guild_id;

    let alice_dir = tempfile::tempdir().unwrap();
    let bob_dir = tempfile::tempdir().unwrap();
    let alice = Client::open_local(alice_dir.path()).unwrap();
    let bob = Client::open_local(bob_dir.path()).unwrap();
    let guild = offline_shared_guild(&alice, &bob);

    let dm_id = direct_guild_id(&alice.author_id(), &bob.author_id());
    alice
        .open_direct_message(guild.id, bob.author_id())
        .await
        .unwrap();
    let announcement = alice
        .events_since(guild.id, None, 100)
        .unwrap()
        .events
        .into_iter()
        .find(|event| {
            matches!(
                &event.body.kind,
                EventKind::DirectMessageOpened { dm, .. } if *dm == dm_id
            )
        })
        .expect("the opener must announce the conversation");
    bob.storage()
        .lock()
        .unwrap()
        .import_event(announcement)
        .unwrap();
    assert!(
        bob.get_guild(dm_id).unwrap().is_none(),
        "importing only the announcement must not create the DM guild"
    );

    let direct = bob.list_direct_messages().unwrap();
    assert_eq!(direct.len(), 1);
    assert_eq!(direct[0].guild.id, dm_id);
    assert_eq!(direct[0].peer.author, alice.author_id());
    assert_eq!(direct[0].peer.display_name, "Alice");
    assert_eq!(bob.direct_peer(dm_id).unwrap(), Some(alice.author_id()));

    // A restart reloads from the same log and must reach the same answer.
    drop(bob);
    let bob = Client::open_local(bob_dir.path()).unwrap();
    let direct = bob.list_direct_messages().unwrap();
    assert_eq!(direct.len(), 1);
    assert_eq!(direct[0].guild.id, dm_id);
    assert_eq!(direct[0].peer.author, alice.author_id());

    // Asking to open the conversation again as the recipient must treat it as
    // already opened. Creating the guild here would sign a second root for the
    // same deterministic id, and a log with two roots cannot be merged.
    let opened = bob
        .open_direct_message(guild.id, alice.author_id())
        .await
        .unwrap();
    assert_eq!(opened.id, dm_id);
    assert!(
        bob.get_guild(dm_id).unwrap().is_none(),
        "the recipient must not create a second founder for the DM"
    );

    alice.shutdown();
    bob.shutdown();
}

/// An ordinary two-member guild with no announcement and an id that is not
/// `direct_guild_id` must never appear in the DM list, even though it has the
/// same roster shape as a DM.
#[test]
fn ordinary_two_member_guilds_are_not_direct_messages() {
    let dir = tempfile::tempdir().unwrap();
    let alice = Client::open_local(dir.path()).unwrap();
    let guild = alice.create_guild("ordinary").unwrap();
    let other = Identity::from_seed([9u8; 32]);
    alice
        .storage()
        .lock()
        .unwrap()
        .append_member_joined(guild.id, "Carol", &other)
        .unwrap()
        .expect("the guild must accept a second join");

    assert_eq!(alice.list_members(guild.id).unwrap().len(), 2);
    assert!(alice.list_direct_messages().unwrap().is_empty());
    assert_eq!(alice.direct_peer(guild.id).unwrap(), None);
}
