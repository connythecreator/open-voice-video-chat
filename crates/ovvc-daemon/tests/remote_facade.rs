//! End-to-end coverage for the daemon-backed frontend facade.
//!
//! Starts the real `ovvc-daemon` binary and drives every [`Frontend`] method
//! once over the real Unix socket. The GUI and TUI call this same facade, so a
//! regression in an op conversion, reply match or daemon dispatch fails here
//! without needing a display or a terminal.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ovvc_client::{Attachment, Identity, QuietHours, hash_bytes};
use ovvc_control::{PresenceStatus, SessionId};
use ovvc_core::{
    AuthorId, ChannelKind, ClientPolicyFloor, GuildId, GuildPolicy, PaddingPolicy, SyncCursor,
    TransportPolicy,
};
use ovvc_remote::{Frontend, RemoteClient};

const BIN: &str = env!("CARGO_BIN_EXE_ovvc-daemon");
const BUDGET: Duration = Duration::from_secs(20);

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct DaemonProcess {
    child: Child,
    dir: tempfile::TempDir,
    socket: PathBuf,
}

impl DaemonProcess {
    fn start() -> Self {
        Self::start_with_config(None)
    }

    /// Start a daemon whose layered configuration contains `config_toml`.
    ///
    /// The config directory, system file and `HOME` all point into the temp
    /// directory, so a developer's real `~/.config/ovvc/config.toml` or
    /// `OVVC_PROFILE` cannot change what these assertions see.
    fn start_with_config(config_toml: Option<&str>) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        if let Some(text) = config_toml {
            std::fs::write(config_dir.join("config.toml"), text).expect("config file");
        }
        let socket = dir.path().join("daemon.sock");
        let child = Command::new(BIN)
            .arg("--data-dir")
            .arg(dir.path())
            .arg("--socket")
            .arg(&socket)
            .env("OVVC_CONFIG_DIR", &config_dir)
            .env("OVVC_SYSTEM_CONFIG", dir.path().join("system.toml"))
            .env("OVVC_SYSTEM_CONFIG_DIR", dir.path().join("system.d"))
            .env("HOME", dir.path())
            .env_remove("OVVC_PROFILE")
            .env_remove("OVVC_THEME")
            .env_remove("OVVC_DATA_DIR")
            .env_remove("OVVC_SOCKET")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ovvc-daemon");
        Self { child, dir, socket }
    }

    fn wait_ready(&self) -> bool {
        let deadline = Instant::now() + BUDGET;
        while Instant::now() < deadline {
            if std::os::unix::net::UnixStream::connect(&self.socket).is_ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        false
    }

    fn socket(&self) -> &Path {
        &self.socket
    }

    #[allow(dead_code)]
    fn wait_exit(&mut self, budget: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + budget;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                _ => return None,
            }
        }
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = &self.dir;
    }
}

fn attach_remote(socket: &Path, data_dir: &Path) -> RemoteClient {
    let options = ovvc_remote::ConnectOptions {
        spawn_daemon: false,
        socket: Some(socket.to_path_buf()),
        ..Default::default()
    };
    RemoteClient::attach(data_dir, options).expect("attach to daemon")
}

#[test]
fn every_frontend_operation_roundtrips_over_the_socket() {
    let _serial = serial();
    let daemon = DaemonProcess::start();
    assert!(daemon.wait_ready(), "daemon did not become ready");
    let client = attach_remote(daemon.socket(), daemon.dir.path());

    // Inherent-only surface.
    assert_eq!(client.data_dir(), daemon.dir.path());
    assert_eq!(client.socket(), daemon.socket());
    assert!(!client.public_key_hex().is_empty());
    let _ = client.daemon_peer_id();
    let status = client.daemon_status().unwrap();
    assert!(!status.version.is_empty());
    assert_eq!(status.data_dir, daemon.dir.path().display().to_string());

    // The trait is what the GUI bridge and the TUI runtime hold.
    let app: &dyn Frontend = &client;
    assert_eq!(app.author_id(), client.author_id());

    let guild = app.create_guild("facade").unwrap();
    assert_eq!(guild.name, "facade");
    assert_eq!(app.list_guilds().unwrap().len(), 1);
    assert_eq!(app.find_guild("facade").unwrap().id, guild.id);
    assert_eq!(app.find_guild(&guild.id.to_hex()).unwrap().id, guild.id);
    assert!(app.find_guild("missing").is_err());
    assert_eq!(client.get_guild(guild.id).unwrap().unwrap().id, guild.id);
    assert!(client.get_guild(GuildId([9u8; 32])).unwrap().is_none());

    // The policy read model defaults to the standard guild preset until signed
    // policy events land, and no floor is configured in this test.
    let policy = app.policy(guild.id).unwrap();
    assert_eq!(policy.guild, GuildPolicy::standard());
    assert_eq!(policy.effective, GuildPolicy::standard());
    assert!(policy.raised_by_client.is_empty());

    // An id the daemon does not have is an error, not the standard preset.
    match app.policy(GuildId([9u8; 32])) {
        Err(ovvc_client::ClientError::Core(ovvc_core::CoreError::NotFound(_))) => {}
        other => panic!("an unknown guild must return NotFound, got {other:?}"),
    }

    let general = app.default_channel(guild.id).unwrap();
    assert_eq!(general.name, "general");
    assert_eq!(
        app.find_channel(guild.id, "#general").unwrap().id,
        general.id
    );
    assert_eq!(
        app.find_channel(guild.id, &general.id.to_hex()[..8])
            .unwrap()
            .id,
        general.id
    );
    assert!(app.find_channel(guild.id, "missing").is_err());
    assert!(!app.list_channels(guild.id).unwrap().is_empty());
    assert_eq!(
        app.get_channel(guild.id, general.id).unwrap().unwrap().id,
        general.id
    );
    assert!(
        app.get_channel(guild.id, ovvc_core::ChannelId([7u8; 16]))
            .unwrap()
            .is_none()
    );

    let category = app.create_category(guild.id, "Voice").unwrap();
    assert_eq!(category.name, "Voice");
    let voice = app
        .create_channel_in(guild.id, "lobby", ChannelKind::Voice, Some(category.id))
        .unwrap();
    let text = app.create_channel(guild.id, "notes").unwrap();
    app.rename_channel(guild.id, text.id, "renamed").unwrap();
    app.move_channel(guild.id, text.id, Some(category.id))
        .unwrap();
    app.move_channel(guild.id, text.id, None).unwrap();
    assert_eq!(app.list_categories(guild.id).unwrap().len(), 1);
    let views = app.list_channel_views(guild.id).unwrap();
    assert!(views.iter().any(|view| view.name == "renamed"));
    let (categories, channels) = client.channel_tree(guild.id).unwrap();
    assert_eq!(categories.len(), 1);
    let lobby = channels
        .iter()
        .find(|channel| channel.id == voice.id)
        .expect("the voice channel is in the tree");
    assert_eq!(lobby.kind, ChannelKind::Voice);
    assert_eq!(lobby.parent, Some(category.id));

    let members = app.list_members(guild.id).unwrap();
    assert_eq!(members.len(), 1);
    let me = members[0].author;
    assert_eq!(app.get_member(guild.id, me).unwrap().unwrap().author, me);
    assert!(
        app.get_member(guild.id, AuthorId([7u8; 32]))
            .unwrap()
            .is_none()
    );

    app.set_display_name(guild.id, "Ada").unwrap();
    let profile_event = app
        .set_profile(
            guild.id,
            "building ovvc",
            "she/her",
            &["https://example.com".to_string()],
        )
        .unwrap();
    assert!(profile_event.verify().is_ok());
    let profile = app.member_profile(guild.id, me).unwrap();
    assert_eq!(profile.display_name, "Ada");
    assert_eq!(profile.about, "building ovvc");
    assert_eq!(profile.pronouns, "she/her");
    assert_eq!(profile.links, vec!["https://example.com".to_string()]);

    let settings = app.notification_settings().unwrap();
    assert!(settings.desktop);
    app.set_desktop_notifications(false).unwrap();
    app.set_do_not_disturb(true).unwrap();
    app.set_quiet_hours(Some(QuietHours {
        start_minute: 60,
        end_minute: 120,
    }))
    .unwrap();
    app.set_quiet_hours(None).unwrap();
    app.set_channel_muted(general.id, true).unwrap();
    app.set_guild_muted(guild.id, true).unwrap();
    let settings = app.notification_settings().unwrap();
    assert!(!settings.desktop);
    assert!(settings.dnd);
    assert!(settings.muted_channels.contains(&general.id));
    assert!(settings.muted_guilds.contains(&guild.id));

    let first = app
        .post_message_full(guild.id, general.id, "facade message", None, &[])
        .unwrap();
    let first_id = first.id().unwrap();
    let reply = app
        .post_message_full(guild.id, general.id, "a reply", Some(first_id), &[me])
        .unwrap();
    assert_eq!(reply.body.kind.reply_to(), Some(first_id));

    let bytes = b"facade blob";
    let hash = hash_bytes(bytes);
    let attachment = Attachment {
        hash,
        size: bytes.len() as u64,
        mime: "application/octet-stream".to_string(),
        name: "facade.bin".to_string(),
    };
    assert_eq!(app.put_blob(bytes).unwrap(), hash);
    let rich = app
        .post_message_rich(
            guild.id,
            general.id,
            "with a file",
            None,
            &[],
            std::slice::from_ref(&attachment),
        )
        .unwrap();
    assert!(rich.body.kind.attachments().contains(&attachment));

    let edited = app
        .edit_message(guild.id, first_id, "facade edited")
        .unwrap();
    assert!(edited.verify().is_ok());
    let reacted = app.add_reaction(guild.id, first_id, "👍").unwrap();
    assert!(reacted.verify().is_ok());
    let unreacted = app.remove_reaction(guild.id, first_id, "👍").unwrap();
    assert!(unreacted.verify().is_ok());

    let history = app.history(guild.id, general.id, 100).unwrap();
    assert!(history.iter().any(|event| event.id().unwrap() == first_id));
    let page = app
        .history_before(
            guild.id,
            general.id,
            Some(SyncCursor::at(&first).unwrap()),
            100,
        )
        .unwrap();
    assert!(
        page.events
            .iter()
            .all(|event| event.id().unwrap() != first_id)
    );
    assert!(!page.more);
    let newest = app.history_before(guild.id, general.id, None, 100).unwrap();
    assert!(!newest.events.is_empty());

    app.mark_read(guild.id, general.id, first_id).unwrap();
    let unread = app.unread_counts(guild.id).unwrap();
    assert!(unread.get(&general.id).copied().unwrap_or(0) <= 1);

    let hits = app.search(guild.id, "facade", 10).unwrap();
    assert!(hits.iter().any(|hit| hit.body.contains("facade edited")));
    assert!(app.search(guild.id, "   ", 10).unwrap().is_empty());

    assert_eq!(app.get_blob(hash).unwrap(), Some(bytes.to_vec()));
    assert!(app.has_blob(hash).unwrap());
    assert_eq!(app.fetch_blob(guild.id, hash).unwrap(), bytes.to_vec());
    let missing = ovvc_core::AttachmentId([0xEE; 32]);
    assert!(app.get_blob(missing).unwrap().is_none());
    assert!(!app.has_blob(missing).unwrap());
    assert!(app.fetch_blob(guild.id, missing).is_err());

    let invite = app
        .create_invite(
            guild.id,
            vec!["/ip4/127.0.0.1/tcp/4001".to_string()],
            Some(60),
        )
        .unwrap();
    assert_eq!(invite.body.guild_id, guild.id);

    // Offline: no running node, so the control session is local and peer
    // lookup has nothing to dial.
    let session = SessionId::for_guild(guild.id);
    app.control_join(guild.id, session, "Ada").unwrap();
    let snapshot = client.control_snapshot(guild.id, session).unwrap();
    assert!(snapshot.roster.iter().any(|member| member.author == me));
    app.set_presence(guild.id, PresenceStatus::Away).unwrap();
    let _ = client.lookup(guild.id, me);
    assert!(app.peer_id(guild.id).is_none());
    let _events = app.subscribe();

    // A peer that is not on the roster cannot be opened, but the request still
    // crosses the socket and the daemon answers with a typed error.
    let peer = Identity::from_seed([0x33; 32]).author_id();
    assert!(app.open_direct_message(guild.id, peer).is_err());
    assert!(app.list_direct_messages().unwrap().is_empty());
    assert!(app.direct_peer(guild.id).unwrap().is_none());

    app.disconnect(guild.id);
    client.leave_guild(guild.id).unwrap();
    assert!(
        app.list_members(guild.id)
            .unwrap()
            .iter()
            .all(|member| member.author != me),
        "leaving removes the local roster row"
    );
}

#[test]
fn a_configured_client_floor_raises_the_effective_policy() {
    let _serial = serial();
    let daemon = DaemonProcess::start_with_config(Some(
        "[policy.client_floor]\n\
         transport = \"tor\"\n\
         padding = \"buckets\"\n",
    ));
    assert!(daemon.wait_ready(), "daemon did not become ready");
    let client = attach_remote(daemon.socket(), daemon.dir.path());

    let guild = client.create_guild("floor").unwrap();
    let view = client.policy(guild.id).unwrap();
    assert_eq!(view.guild, GuildPolicy::standard());
    assert_eq!(
        view.client_floor.transport,
        Some(TransportPolicy::Tor),
        "the floor must come from the isolated configuration"
    );
    assert_eq!(view.client_floor.padding, Some(PaddingPolicy::Buckets));
    assert_eq!(view.effective.transport, TransportPolicy::Tor);
    assert_eq!(view.effective.padding, PaddingPolicy::Buckets);
    assert_eq!(
        view.raised_by_client,
        vec!["transport".to_string(), "padding".to_string()]
    );
    assert_eq!(view.client_floor, ClientPolicyFloor {
        transport: Some(TransportPolicy::Tor),
        padding: Some(PaddingPolicy::Buckets),
        ..ClientPolicyFloor::default()
    });
}
