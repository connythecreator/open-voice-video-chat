//! Headless rendering tests.
//!
//! `egui::Context::run_ui` lays out and paints a frame without a window, so the
//! real widget code runs here. Every test drives [`ovvc_gui::ui::draw`] against
//! a state built through the same reply and event path the bridge uses, which
//! catches panics in layout, markdown rendering, scrolling and dialogs that a
//! pure state test cannot see.
//!
//! Run with:
//!
//! ```text
//! cargo test -p ovvc-gui --test render
//! ```

#![cfg(feature = "ui")]

use std::collections::HashMap;

use egui::{Context, RawInput, Rect, pos2, vec2};
use ovvc_control::{CallTopology, PresenceStatus, VoiceMemberState, VoiceSnapshot, now_ms};
use ovvc_core::{
    AuthorId, ChannelId, ChannelKind, ChannelMeta, DirectConversation, EventBody, EventId,
    EventKind, GuildId, GuildMeta, Identity, MemberInfo, MemberProfile, SignedEvent,
};
use ovvc_gui::state::{
    Action, AppState, DetailLevel, Dialog, RequestTag, ScreenView, SettingsTab, WizardCommune,
    WizardStep,
};
use ovvc_ipc::{ConnectionStateDto, DaemonEvent, PeerSnapshot, Reply, SearchHit, VoiceAudioStats};
use ovvc_media::{CallId, ProfileSet, VoiceMember};

const GUILD: GuildId = GuildId([1; 32]);
const GENERAL: ChannelId = ChannelId([2; 16]);
const OFFTOPIC: ChannelId = ChannelId([3; 16]);

const SIZES: [[f32; 2]; 4] = [
    [1180.0, 760.0],
    [900.0, 600.0],
    [760.0, 480.0],
    [320.0, 240.0],
];

fn me() -> Identity {
    Identity::from_seed([7u8; 32])
}

fn other() -> Identity {
    Identity::from_seed([9u8; 32])
}

fn third() -> Identity {
    Identity::from_seed([11u8; 32])
}

fn event(author: &Identity, seq: u64, kind: EventKind) -> SignedEvent {
    event_in(author, seq, GENERAL, kind)
}

fn event_in(author: &Identity, seq: u64, channel: ChannelId, kind: EventKind) -> SignedEvent {
    author
        .sign_body(EventBody {
            version: 2,
            guild_id: GUILD,
            channel_id: Some(channel),
            author: author.author_id(),
            seq,
            lamport: seq,
            timestamp: 1_700_000_000 + seq * 60,
            prev: None,
            kind,
        })
        .expect("sign test event")
}

fn guild() -> GuildMeta {
    GuildMeta {
        id: GUILD,
        name: "The Guild".to_string(),
        founder: me().author_id(),
        created_at: 1_700_000_000,
    }
}

fn channel(id: ChannelId, name: &str) -> ChannelMeta {
    ChannelMeta {
        id,
        guild_id: GUILD,
        name: name.to_string(),
        created_by: me().author_id(),
        created_at: 1_700_000_000,
    }
}

fn member(identity: &Identity, name: &str) -> MemberInfo {
    MemberInfo {
        guild_id: GUILD,
        author: identity.author_id(),
        display_name: name.to_string(),
        joined_at: 1_700_000_000,
    }
}

fn profile(identity: &Identity, name: &str) -> MemberProfile {
    MemberProfile {
        guild_id: GUILD,
        author: identity.author_id(),
        display_name: name.to_string(),
        about: "builds small tools and reads logs".to_string(),
        pronouns: "they/them".to_string(),
        links: vec![
            "https://example.com/~ada".to_string(),
            "https://example.com/notes".to_string(),
        ],
        joined_at: 1_700_000_000,
        updated_at: 1_700_000_100,
        message_count: 42,
        last_activity: 1_700_000_200,
    }
}

fn connected_state() -> AppState {
    let mut state = AppState::new();
    state.apply_event(DaemonEvent::Connected {
        daemon_version: "0.1.0".to_string(),
        author_id: me().author_id(),
    });
    state
}

fn with_guilds() -> AppState {
    let mut state = connected_state();
    state.apply_reply(RequestTag::Guilds, Ok(Reply::Guilds(vec![guild()])));
    state
}

/// A state that exercises every part of the main window: two channels, three
/// members, plain and marked-up messages, a reply, a mention, a reaction, a
/// code block, unread badges, typing and a live connection.
fn populated_state() -> AppState {
    let mut state = with_guilds();
    state.apply_reply(
        RequestTag::Channels(GUILD),
        Ok(Reply::Channels(vec![
            channel(GENERAL, "general"),
            channel(OFFTOPIC, "off-topic"),
        ])),
    );
    state.apply_reply(
        RequestTag::Members(GUILD),
        Ok(Reply::Members(vec![
            member(&me(), "Conleth"),
            member(&other(), "Ada"),
            member(&third(), "Grace"),
        ])),
    );
    state.apply_event(DaemonEvent::Presence {
        guild_id: GUILD,
        author: other().author_id(),
        status: PresenceStatus::Online,
    });
    state.apply_event(DaemonEvent::Presence {
        guild_id: GUILD,
        author: third().author_id(),
        status: PresenceStatus::Away,
    });

    let first = event(
        &other(),
        1,
        EventKind::Message {
            body: "Morning all, the daemon build is green again.".to_string(),
        },
    );
    let second = event(
        &other(),
        2,
        EventKind::MessageV2 {
            body: "**Bold**, *italic*, ~~struck~~, `inline` and [a link](https://example.com), \
                   plus @Conleth and a mention of nobody."
                .to_string(),
            reply_to: Some(first.id().expect("event id")),
            mentions: vec![me().author_id()],
        },
    );
    let third_message = event(
        &me(),
        3,
        EventKind::Message {
            body: "Here is the snippet:\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```"
                .to_string(),
        },
    );
    let reaction = event(
        &third(),
        4,
        EventKind::ReactionAdded {
            target: second.id().expect("event id"),
            emoji: "\u{1f44d}".to_string(),
        },
    );
    state.apply_reply(
        RequestTag::History {
            guild_id: GUILD,
            channel_id: GENERAL,
        },
        Ok(Reply::Events(vec![first, second, third_message, reaction])),
    );

    state.apply_event(DaemonEvent::Event {
        guild_id: GUILD,
        event: event_in(
            &other(),
            5,
            OFFTOPIC,
            EventKind::Message {
                body: "unread elsewhere".to_string(),
            },
        ),
        local: false,
    });
    let mut unread = HashMap::new();
    unread.insert(OFFTOPIC, 1usize);
    state.apply_reply(RequestTag::Unread(GUILD), Ok(Reply::Unread(unread)));

    state.apply_event(DaemonEvent::Typing {
        guild_id: GUILD,
        channel_id: GENERAL,
        author: other().author_id(),
        until_ms: now_ms() + 6_000,
    });
    state.apply_reply(
        RequestTag::Connection(GUILD),
        Ok(Reply::Connection(ConnectionStateDto {
            guild_id: GUILD,
            connected: true,
            peer_id: Some("12D3KooWtest".to_string()),
            listen_addresses: vec!["/ip4/0.0.0.0/udp/4001/quic-v1".to_string()],
            external_addresses: vec!["/ip4/203.0.113.7/udp/4001/quic-v1".to_string()],
            peers: vec![PeerSnapshot {
                peer: "12D3KooWpeer".to_string(),
                author: Some(other().author_id()),
                connected: true,
                addresses: vec!["/ip4/198.51.100.2/udp/4001/quic-v1".to_string()],
            }],
        })),
    );
    state.composer = "a draft message".to_string();
    state.dialog_input = "settings value".to_string();
    state
}

fn render(ctx: &Context, state: &mut AppState, size: [f32; 2]) -> Vec<Action> {
    let input = RawInput {
        screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(size[0], size[1]))),
        ..Default::default()
    };
    let mut actions = Vec::new();
    // Two frames: panels remember their size from the previous frame, and a
    // window needs a frame to be created before it can be laid out.
    for _ in 0..2 {
        let mut output = ctx.run_ui(input.clone(), |ui| {
            ovvc_gui::ui::draw(ui, state, &mut actions);
        });
        // There is no painter in a headless test, so acknowledge the font
        // texture upload instead of letting it trip the debug assertion in
        // `TexturesDelta::drop`.
        output.textures_delta.clear();
    }
    actions
}

fn context() -> Context {
    let ctx = Context::default();
    ovvc_gui::theme::install(&ctx);
    ctx
}

#[test]
fn offline_and_onboarding_states_render_at_every_size() {
    let ctx = context();

    let mut offline = AppState::new();
    offline.daemon_error = Some("connection refused".to_string());

    let mut connected = connected_state();
    connected.public_key_hex = Some("ab".repeat(32));

    let mut guilds = with_guilds();
    guilds.show_members = false;

    for size in SIZES {
        render(&ctx, &mut offline, size);
        render(&ctx, &mut connected, size);
        render(&ctx, &mut guilds, size);
    }
}

#[test]
fn populated_chat_renders_at_every_size() {
    let ctx = context();
    for size in SIZES {
        let mut state = populated_state();
        render(&ctx, &mut state, size);
    }
}

#[test]
fn paging_controls_and_transcript_dividers_render() {
    let ctx = context();
    for size in SIZES {
        let mut state = populated_state();
        state.history_more = true;
        state.unread_marker = Some(2);
        state.anchor_to = state.messages.first().map(|message| message.event_id);
        render(&ctx, &mut state, size);
    }

    let mut loading = populated_state();
    loading.history_more = true;
    loading.history_loading = true;
    render(&ctx, &mut loading, [1180.0, 760.0]);

    let mut stale_marker = populated_state();
    stale_marker.unread_marker = Some(50);
    render(&ctx, &mut stale_marker, [1180.0, 760.0]);
}

#[test]
fn the_voice_panel_renders_the_call_roster_and_audio() {
    let ctx = context();
    let mut state = populated_state();
    let voice_id = ChannelId([5; 16]);
    let mut voice_channel = state.channels[0].clone();
    voice_channel.id = voice_id;
    voice_channel.name = "standup".to_string();
    voice_channel.kind = ChannelKind::Voice;
    state.channels.push(voice_channel);
    state.selected_channel = Some(voice_id);
    state.voice.guild = state.selected_guild;
    state.voice.channel = Some(voice_id);
    state.voice.muted = true;
    state.voice.push_to_talk = true;
    state.voice.audio = Some(VoiceAudioStats {
        backend: "pipewire".to_string(),
        captured: 480,
        packets: 24,
        played: 480,
        concealed: 0,
        lost: 0,
        decode_errors: 0,
        peers: 2,
        xruns: 0,
        latency_ms: 20,
        peak: 300,
        rms: 0.125,
        transport: "webrtc".to_string(),
        rtt_ms: 35,
        packets_lost: 12,
        relayed: true,
        e2ee: true,
        epoch: 1,
        e2ee_errors: 0,
    });
    state.voice.error = Some("could not connect WebRTC peer 3f2a: timed out".to_string());
    let local = me().author_id();
    let peer = other().author_id();
    state.voice.snapshots.insert(
        voice_id,
        VoiceSnapshot {
            channel_id: voice_id,
            call_id: CallId::for_channel(voice_id),
            members: vec![
                VoiceMember {
                    author: local,
                    joined_at_ms: 10,
                    last_seen_ms: 20,
                    state: VoiceMemberState {
                        speaking: true,
                        muted: true,
                        push_to_talk: true,
                        ..VoiceMemberState::default()
                    },
                    profiles: ProfileSet::STANDARD,
                },
                VoiceMember {
                    author: peer,
                    joined_at_ms: 11,
                    last_seen_ms: 21,
                    state: VoiceMemberState {
                        deafened: true,
                        ..VoiceMemberState::default()
                    },
                    profiles: ProfileSet::STANDARD,
                },
            ],
            topology: CallTopology::Star {
                hub: local,
                leaves: vec![peer],
            },
            hub: Some(local),
            scope: ovvc_core::CallScope::Star,
            hub_policy: ovvc_core::HubPolicy::elected(),
            key_epoch: 1,
            codec: None,
            started_at_ms: 10,
            updated_at_ms: 20,
            master: Some(local),
            backup: Some(peer),
            term: 1,
        },
    );

    for size in SIZES {
        render(&ctx, &mut state, size);
    }

    // A call with no usable key and dropped frames takes the error branches of
    // the E2EE line, which must still lay out at every window size.
    if let Some(audio) = state.voice.audio.as_mut() {
        audio.e2ee = false;
        audio.epoch = 0;
        audio.e2ee_errors = 2;
    }
    for size in SIZES {
        render(&ctx, &mut state, size);
    }

    // The expert connection block reports the star media path the daemon now
    // runs, not the old forwarding-pending caveat.
    state.detail_level = DetailLevel::Expert;
    let mut harness = harness_with(state, [1180.0, 760.0]);
    harness.run();
    let texts = visible_texts(&harness);
    assert!(
        texts.iter().any(|text| text.contains("media: star")),
        "the panel should name the star media path: {texts:?}"
    );
    assert!(
        texts.iter().any(|text| text.contains("keys: leaf->hub")),
        "the panel should name the star key rule: {texts:?}"
    );
    assert!(
        !texts.iter().any(|text| text.contains("forwarding pending")),
        "the panel must not repeat the stale forwarding caveat: {texts:?}"
    );
}

#[test]
fn member_panel_can_be_hidden_and_shown() {
    let ctx = context();
    for show in [true, false] {
        let mut state = populated_state();
        state.show_members = show;
        render(&ctx, &mut state, [1180.0, 760.0]);
    }
}

#[test]
fn dialogs_render() {
    let ctx = context();
    for dialog in [
        Dialog::CreateGuild,
        Dialog::CreateChannel,
        Dialog::CreateCategory,
        Dialog::ChannelSettings,
        Dialog::Join,
        Dialog::Invite,
    ] {
        for size in SIZES {
            let mut state = populated_state();
            state.dialog = dialog;
            if dialog == Dialog::ChannelSettings {
                state.begin_channel_settings(GENERAL);
            }
            render(&ctx, &mut state, size);
        }
    }
}

#[test]
fn every_settings_tab_renders() {
    let ctx = context();
    for tab in SettingsTab::ALL {
        for level in DetailLevel::ALL {
            let mut state = populated_state();
            state.screen = ScreenView::Settings;
            state.settings_tab = tab;
            state.detail_level = level;
            render(&ctx, &mut state, [1180.0, 760.0]);
            render(&ctx, &mut state, [760.0, 480.0]);
            render(&ctx, &mut state, [320.0, 240.0]);
        }
    }
}

#[test]
fn every_wizard_step_renders() {
    let ctx = context();
    for step in WizardStep::ALL {
        for size in SIZES {
            let mut state = populated_state();
            state.wizard_step = Some(step);
            state.wizard_name = "Conleth".to_string();
            state.wizard_theme_index = Some(0);
            render(&ctx, &mut state, size);
        }
    }
}

fn harness_with(state: AppState, size: [f32; 2]) -> egui_kittest::Harness<'static, AppState> {
    egui_kittest::Harness::builder()
        .with_size(egui::vec2(size[0], size[1]))
        .with_context_setup(ovvc_gui::theme::install)
        .with_max_steps(8)
        .build_ui_state(
            |ui, state| {
                let mut actions = Vec::new();
                ovvc_gui::ui::draw(ui, state, &mut actions);
            },
            state,
        )
}

/// Every AccessKit label and value in the frame, the same text an assistive
/// technology reads and the fuzz harness asserts on.
fn visible_texts(harness: &egui_kittest::Harness<'_, AppState>) -> Vec<String> {
    use egui_kittest::kittest::{NodeT, Queryable};

    let mut texts = Vec::new();
    for node in harness.query_all_by(|_node| true) {
        let access = node.accesskit_node();
        if let Some(label) = access.label()
            && !label.is_empty()
        {
            texts.push(label);
        }
        if let Some(value) = access.value()
            && !value.is_empty()
        {
            texts.push(value);
        }
    }
    texts
}

#[test]
fn the_current_user_card_owns_identity_and_the_members_toggle() {
    use egui_kittest::kittest::Queryable;

    let mut harness = harness_with(populated_state(), [1180.0, 760.0]);
    harness.run();

    // The card is at the top of the member rail, and `[M]` collapses it.
    assert!(harness.query_by_label("[T]").is_some(), "no theme control");
    assert!(harness.query_by_label("[M]").is_some(), "no members toggle");
    harness.query_by_label("[M]").expect("[M]").click();
    harness.run();
    assert!(!harness.state().show_members);
    // The collapsed rail keeps `[M]`, so the panel can be reopened without a
    // second top-bar control.
    assert!(harness.query_by_label("[M]").is_some());
    harness.query_by_label("[M]").expect("[M]").click();
    harness.run();
    assert!(harness.state().show_members);

    // `[T]` replaces the old `[config]` and opens Preferences > theme.
    harness.query_by_label("[T]").expect("[T]").click();
    harness.run();
    assert_eq!(harness.state().screen, ScreenView::Settings);
    assert_eq!(harness.state().settings_tab, SettingsTab::Preferences);
}

#[test]
fn the_global_detail_slider_changes_the_level() {
    use egui_kittest::kittest::Queryable;

    let mut state = populated_state();
    state.screen = ScreenView::Settings;
    state.settings_tab = SettingsTab::User;
    let mut harness = harness_with(state, [1180.0, 760.0]);
    harness.run();

    let rect = harness
        .query_by_label("detail level slider")
        .expect("the detail slider should be in the accessibility tree")
        .rect();
    // The slider is global. Clicking the right end selects expert.
    let pos = egui::pos2(rect.right() - 3.0, rect.center().y);
    let events = harness.input_mut();
    events.events.push(egui::Event::PointerMoved(pos));
    events.events.push(egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::default(),
    });
    events.events.push(egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::default(),
    });
    harness.run();
    assert_eq!(harness.state().detail_level, DetailLevel::Expert);
}

#[test]
fn every_detail_level_renders_the_card_roster_and_expert_block() {
    let ctx = context();
    for level in DetailLevel::ALL {
        for size in SIZES {
            let mut state = populated_state();
            state.detail_level = level;
            render(&ctx, &mut state, size);

            // The expert level adds one connection block to the member rail.
            if level == DetailLevel::Expert {
                let mut expert = populated_state();
                expert.detail_level = DetailLevel::Expert;
                render(&ctx, &mut expert, [1180.0, 760.0]);
            }
        }
    }
}

#[test]
fn the_network_map_and_diagnostics_render_at_every_size() {
    let ctx = context();
    for size in SIZES {
        let mut map = populated_state();
        map.screen = ScreenView::NetworkMap;
        render(&ctx, &mut map, size);

        let mut diagnostics = populated_state();
        diagnostics.screen = ScreenView::Diagnostics;
        diagnostics.error = Some("attachment fetch failed".to_string());
        diagnostics.error_log = Some("/tmp/ovvc/daemon.log".to_string());
        render(&ctx, &mut diagnostics, size);

        let mut confirming = populated_state();
        confirming.screen = ScreenView::Diagnostics;
        confirming.diagnostics_confirm_shutdown = true;
        render(&ctx, &mut confirming, size);
    }
}

#[test]
fn setup_requires_an_identity_and_preselects_no_theme() {
    use egui_kittest::kittest::{NodeT, Queryable};

    let mut state = populated_state();
    state.wizard_step = Some(WizardStep::Identity);
    state.wizard_name.clear();
    let mut harness = harness_with(state, [1180.0, 760.0]);
    harness.run();

    assert!(
        harness
            .query_by_label("[ next ]")
            .expect("the next action should be present")
            .accesskit_node()
            .is_disabled(),
        "next must be disabled until a display name is entered"
    );

    harness.state_mut().wizard_name = "Conleth".to_string();
    harness.run();
    assert!(
        !harness
            .query_by_label("[ next ]")
            .expect("the next action should be present")
            .accesskit_node()
            .is_disabled(),
        "next should be enabled once a display name is entered"
    );

    harness.state_mut().wizard_step = Some(WizardStep::Theme);
    harness.run();
    assert_eq!(harness.state().wizard_theme_index, None);
    for preset in ovvc_gui::theme::Preset::all() {
        assert!(
            harness
                .query_by_label(&format!("[ {} ]", preset.label()))
                .is_some(),
            "the setup theme step should offer {}",
            preset.label()
        );
    }
}

#[test]
fn setup_creates_or_joins_a_commune_without_the_cli() {
    use egui_kittest::kittest::{NodeT, Queryable};

    let mut state = populated_state();
    state.wizard_step = Some(WizardStep::Commune);
    state.wizard_name = "Conleth".to_string();
    state.wizard_commune = WizardCommune::Create;
    state.wizard_commune_name.clear();
    let mut harness = harness_with(state, [1180.0, 760.0]);
    harness.run();

    // The commune step is required: neither an empty name nor an empty invite
    // may move the flow forward.
    assert!(
        harness
            .query_by_label("[ next ]")
            .expect("next")
            .accesskit_node()
            .is_disabled(),
        "next must wait for a commune name"
    );
    harness.state_mut().wizard_commune_name = "late night devs".to_string();
    harness.run();
    assert!(
        !harness
            .query_by_label("[ next ]")
            .expect("next")
            .accesskit_node()
            .is_disabled(),
        "a commune name should enable next"
    );

    // The join path asks for a pasted invite instead.
    harness.state_mut().wizard_commune = WizardCommune::Join;
    harness.state_mut().wizard_join_code.clear();
    harness.run();
    assert!(
        harness
            .query_by_label("[ next ]")
            .expect("next")
            .accesskit_node()
            .is_disabled(),
        "the join path must wait for an invite"
    );
    harness.state_mut().wizard_join_code = "ovvc://invite".to_string();
    harness.run();
    assert!(
        !harness
            .query_by_label("[ next ]")
            .expect("next")
            .accesskit_node()
            .is_disabled(),
        "a pasted invite should enable next"
    );

    // Finish is only live when the identity and commune steps are complete.
    harness.state_mut().wizard_step = Some(WizardStep::Review);
    harness.state_mut().wizard_commune_name.clear();
    harness.state_mut().wizard_commune = WizardCommune::Create;
    harness.run();
    assert!(
        harness
            .query_by_label("[ finish ]")
            .expect("finish")
            .accesskit_node()
            .is_disabled(),
        "finish must not run while the commune step is incomplete"
    );
    harness.state_mut().wizard_commune_name = "late night devs".to_string();
    harness.run();
    assert!(
        !harness
            .query_by_label("[ finish ]")
            .expect("finish")
            .accesskit_node()
            .is_disabled(),
        "a complete identity and commune should enable finish"
    );
}

#[test]
fn network_settings_expose_the_fixed_port_public_address_and_relay() {
    use egui_kittest::kittest::Queryable;

    let mut state = populated_state();
    state.screen = ScreenView::Settings;
    state.settings_tab = SettingsTab::Network;
    state.detail_level = DetailLevel::Advanced;
    let mut harness = harness_with(state, [1180.0, 760.0]);
    harness.run();

    for label in [
        "listen address",
        "public invite address",
        "relay address",
        "invite a friend",
    ] {
        assert!(
            harness.query_by_label(label).is_some(),
            "network settings should show {label}"
        );
    }
    let texts = visible_texts(&harness);
    assert!(
        texts.iter().any(|text| text.contains("TCP 41000")),
        "the fixed listen port should be explained: {texts:?}"
    );
    assert!(
        texts.iter().any(|text| text.contains("UPnP")),
        "the router limitation should be explained: {texts:?}"
    );
    for hidden in ["relay only", "hide my IP", "mDNS", "sandbox"] {
        assert!(
            !texts.iter().any(|text| text.contains(hidden)),
            "v0.1 must not surface the unimplemented {hidden} control: {texts:?}"
        );
    }
}

#[test]
fn the_invite_dialog_names_the_address_and_the_join_path() {
    use egui_kittest::kittest::Queryable;

    let mut state = populated_state();
    state.dialog = Dialog::Invite;
    state.invite_code.clear();
    let mut harness = harness_with(state, [1180.0, 760.0]);
    harness.run();

    let texts = visible_texts(&harness);
    assert!(
        texts
            .iter()
            .any(|text| text.contains("this code is the whole invitation")),
        "invite copy should say what the code is: {texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|text| text.contains("/ip4/203.0.113.7/udp/4001/quic-v1")),
        "invite should list the address it will carry: {texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|text| text.contains("forward TCP 41000")),
        "invite should explain the remote path: {texts:?}"
    );
    assert!(
        harness.query_by_label("[ join a friend instead ]").is_some(),
        "invite should offer the join path"
    );
}

#[test]
fn categories_and_kinds_render_in_the_sidebar() {
    let ctx = context();
    let mut state = with_guilds();
    let category = ovvc_core::CategoryMeta {
        id: ChannelId([8; 16]),
        guild_id: GUILD,
        name: "projects".to_string(),
        created_by: me().author_id(),
        created_at: 1_700_000_000,
    };
    let view = |id: ChannelId, name: &str, kind, parent| ovvc_core::ChannelView {
        id,
        guild_id: GUILD,
        name: name.to_string(),
        created_by: me().author_id(),
        created_at: 1_700_000_000,
        kind,
        parent,
    };
    state.apply_reply(
        RequestTag::ChannelTree(GUILD),
        Ok(Reply::ChannelTree(ovvc_ipc::ChannelTreeDto {
            categories: vec![category.clone()],
            channels: vec![
                view(GENERAL, "general", ovvc_core::ChannelKind::Text, None),
                view(
                    ChannelId([9; 16]),
                    "announcements",
                    ovvc_core::ChannelKind::Announcement,
                    None,
                ),
                view(
                    ChannelId([10; 16]),
                    "standup",
                    ovvc_core::ChannelKind::Voice,
                    Some(category.id),
                ),
                view(
                    ChannelId([11; 16]),
                    "help",
                    ovvc_core::ChannelKind::Forum,
                    Some(category.id),
                ),
            ],
        })),
    );
    state.apply_reply(
        RequestTag::NotificationSettings,
        Ok(Reply::NotificationSettings(
            ovvc_core::NotificationSettings {
                muted_channels: vec![ChannelId([10; 16])],
                ..Default::default()
            },
        )),
    );
    for size in SIZES {
        render(&ctx, &mut state, size);
    }
}

#[test]
fn invite_dialog_renders_a_ready_invite() {
    let ctx = context();
    let mut state = populated_state();
    state.dialog = Dialog::Invite;
    state.invite_code = "ovvc1-invite-code-that-is-long-enough-to-wrap".to_string();
    render(&ctx, &mut state, [1180.0, 760.0]);
}

#[test]
fn search_results_render() {
    let ctx = context();
    let mut state = populated_state();
    state.search_query = "daemon".to_string();
    let hit = event(
        &other(),
        9,
        EventKind::Message {
            body: "the daemon build is green".to_string(),
        },
    );
    state.apply_reply(
        RequestTag::Search(GUILD),
        Ok(Reply::Search(vec![SearchHit {
            guild_id: GUILD,
            channel_id: GENERAL,
            event: hit,
            body: "the daemon build is green".to_string(),
        }])),
    );
    for size in SIZES {
        render(&ctx, &mut state, size);
    }
}

#[test]
fn reply_and_edit_banners_render() {
    let ctx = context();
    let mut state = populated_state();
    let target = state.messages[0].event_id;
    state.reply_to = Some((target, other().author_id(), "quoted body".to_string()));
    render(&ctx, &mut state, [1180.0, 760.0]);

    state.reply_to = None;
    state.editing = Some((target, "editing this body".to_string()));
    render(&ctx, &mut state, [1180.0, 760.0]);
}

#[test]
fn a_deleted_message_renders_its_tombstone() {
    let ctx = context();
    let mut state = populated_state();
    let target = state.messages[0].event_id;
    state.apply_event(DaemonEvent::Event {
        guild_id: GUILD,
        event: event(&other(), 20, EventKind::MessageDeleted { target }),
        local: false,
    });
    render(&ctx, &mut state, [1180.0, 760.0]);
}

#[test]
fn unknown_authors_render_with_short_ids() {
    let ctx = context();
    let mut state = populated_state();
    state.members.clear();
    state.apply_event(DaemonEvent::Event {
        guild_id: GUILD,
        event: event(
            &Identity::from_seed([200u8; 32]),
            30,
            EventKind::Message {
                body: "no roster entry".to_string(),
            },
        ),
        local: false,
    });
    render(&ctx, &mut state, [1180.0, 760.0]);
}

#[test]
fn pathological_bodies_do_not_panic_the_renderer() {
    let ctx = context();
    let mut state = populated_state();
    let bodies = [
        "**".to_string(),
        "```".to_string(),
        "```\nunterminated".to_string(),
        "[link](".to_string(),
        "@".to_string(),
        "\u{0}\u{1}\u{2}".to_string(),
        "a".repeat(20_000),
        "\u{1f44d}".repeat(500),
    ];
    for (index, body) in bodies.into_iter().enumerate() {
        state.apply_event(DaemonEvent::Event {
            guild_id: GUILD,
            event: event(&other(), 100 + index as u64, EventKind::Message { body }),
            local: false,
        });
    }
    for size in SIZES {
        render(&ctx, &mut state, size);
    }
}

#[test]
fn unicode_names_and_channels_render() {
    let ctx = context();
    let mut state = with_guilds();
    state.apply_reply(
        RequestTag::Channels(GUILD),
        Ok(Reply::Channels(vec![channel(GENERAL, "\u{1f680}-launch")])),
    );
    state.apply_reply(
        RequestTag::Members(GUILD),
        Ok(Reply::Members(vec![member(&other(), "\u{5f20}\u{4e09}")])),
    );
    state.apply_event(DaemonEvent::Event {
        guild_id: GUILD,
        event: event(
            &other(),
            1,
            EventKind::Message {
                body: "shipping today \u{1f389}".to_string(),
            },
        ),
        local: false,
    });
    render(&ctx, &mut state, [1180.0, 760.0]);
}

#[test]
fn a_guild_without_an_author_id_renders() {
    // This is the path that used to return a reference to a temporary for the
    // avatar seed.
    let ctx = context();
    let mut state = with_guilds();
    state.identity = None;
    render(&ctx, &mut state, [1180.0, 760.0]);
}

#[test]
fn rendering_is_stable_across_many_frames() {
    let ctx = context();
    let mut state = populated_state();
    for _ in 0..30 {
        render(&ctx, &mut state, [1180.0, 760.0]);
    }
}

#[test]
fn author_colours_are_deterministic() {
    let author: AuthorId = other().author_id();
    let first = ovvc_gui::ui::author_color(author);
    let second = ovvc_gui::ui::author_color(author);
    assert_eq!(first, second);

    let distinct = (0u8..32)
        .map(|seed| ovvc_gui::ui::author_color(Identity::from_seed([seed; 32]).author_id()))
        .collect::<std::collections::HashSet<_>>();
    assert!(distinct.len() > 1, "every author got the same colour");
}

#[test]
fn every_preset_renders_and_the_switch_takes_effect() {
    use ovvc_gui::theme::{Palette, Preset};

    for preset in Preset::all() {
        let ctx = Context::default();
        ovvc_gui::theme::install(&ctx);
        // The same call the settings tab and the wizard make on a click.
        ovvc_gui::theme::apply(&ctx, *preset);
        assert_eq!(ovvc_gui::theme::active(&ctx), *preset);

        let mut state = populated_state();
        state.screen = ScreenView::Settings;
        state.settings_tab = SettingsTab::Preferences;
        render(&ctx, &mut state, [1180.0, 760.0]);
        render(&ctx, &mut state, [320.0, 240.0]);

        // The drawing code reads the palette from the context, so the frame
        // above painted with the preset that was just installed.
        assert_eq!(
            ovvc_gui::theme::palette(&ctx),
            Palette::from(&preset.theme())
        );
    }
}

#[test]
fn switching_back_to_phosphor_restores_the_default_palette() {
    let ctx = Context::default();
    ovvc_gui::theme::install(&ctx);
    let phosphor = ovvc_gui::theme::palette(&ctx);

    ovvc_gui::theme::apply(&ctx, ovvc_gui::theme::Preset::Paper);
    assert_ne!(ovvc_gui::theme::palette(&ctx), phosphor);

    ovvc_gui::theme::apply(&ctx, ovvc_gui::theme::Preset::Phosphor);
    assert_eq!(ovvc_gui::theme::palette(&ctx), phosphor);
}

#[test]
fn a_search_jump_lands_on_the_hit_or_points_to_older_history() {
    let ctx = context();
    let mut state = populated_state();
    let target = state.messages[1].event_id;
    state.jump_to = Some(target);
    state.notice = None;
    render(&ctx, &mut state, SIZES[0]);
    assert_eq!(state.jump_to, None, "the jump was not consumed");
    assert!(
        state.notice.is_none(),
        "a hit that is loaded should not warn: {:?}",
        state.notice
    );

    // A hit older than the page kept in memory must clear itself and tell the
    // reader how to reach it, instead of pinning every later frame to a
    // message that is not there.
    state.jump_to = Some(EventId([0xEE; 32]));
    state.notice = None;
    render(&ctx, &mut state, SIZES[0]);
    assert_eq!(state.jump_to, None, "an unfound jump was not cleared");
    assert!(
        state
            .notice
            .as_deref()
            .unwrap_or_default()
            .contains("older than the loaded history"),
        "the missing hit was not explained: {:?}",
        state.notice
    );
}

// --- profiles and direct messages -----------------------------------------

#[test]
fn profile_dialogs_render_for_another_member_and_self_editing() {
    let ctx = context();
    let mut state = populated_state();

    // Another member's read-only profile, with the daemon-resolved counts.
    state.open_profile(GUILD, other().author_id());
    state.apply_reply(
        RequestTag::MemberProfile {
            guild_id: GUILD,
            author: other().author_id(),
        },
        Ok(Reply::Member(profile(&other(), "Ada"))),
    );
    for size in SIZES {
        render(&ctx, &mut state, size);
    }
    assert!(!state.profile_editing);
    assert_eq!(
        state
            .profile
            .as_ref()
            .map(|profile| profile.display_name.as_str()),
        Some("Ada")
    );

    // Your own profile can be switched into edit mode and saved.
    state.open_profile(GUILD, me().author_id());
    state.apply_reply(
        RequestTag::MemberProfile {
            guild_id: GUILD,
            author: me().author_id(),
        },
        Ok(Reply::Member(profile(&me(), "Conleth"))),
    );
    state.begin_profile_edit();
    state.profile_about = "  a draft about  ".to_string();
    state.profile_pronouns = " they/them ".to_string();
    state.profile_links = " https://example.com/new \n".to_string();
    for size in SIZES {
        render(&ctx, &mut state, size);
    }
    let save = state.profile_save_request().expect("a valid draft");
    assert_eq!(
        save,
        Action::SaveProfile {
            guild_id: GUILD,
            about: "a draft about".to_string(),
            pronouns: "they/them".to_string(),
            links: vec!["https://example.com/new".to_string()],
        }
    );
}

#[test]
fn the_direct_message_list_and_header_render() {
    let ctx = context();
    let dm = GuildMeta {
        id: GuildId([6; 32]),
        name: "ada, conleth".to_string(),
        founder: me().author_id(),
        created_at: 1_700_000_100,
    };
    let dm_channel = ChannelId([7; 16]);
    let peer = MemberInfo {
        guild_id: dm.id,
        author: other().author_id(),
        display_name: "Ada".to_string(),
        joined_at: 1_700_000_000,
    };
    let mut state = populated_state();
    state.apply_reply(
        RequestTag::DirectMessages,
        Ok(Reply::DirectMessages(vec![DirectConversation {
            guild: dm.clone(),
            peer: peer.clone(),
        }])),
    );
    state.dm_unread.insert(dm.id, 2);

    // The DM row renders next to the commune channels without being selected.
    for size in SIZES {
        render(&ctx, &mut state, size);
    }
    assert_eq!(state.direct_messages.len(), 1);
    assert_eq!(state.dm_unread_for(dm.id), 2);

    // Selecting it switches the header and status bar to `dm:Ada` and loads
    // its one channel and two-person roster.
    state.select_guild(dm.id);
    state.apply_reply(
        RequestTag::ChannelTree(dm.id),
        Ok(Reply::ChannelTree(ovvc_ipc::ChannelTreeDto {
            categories: Vec::new(),
            channels: vec![ovvc_core::ChannelView {
                id: dm_channel,
                guild_id: dm.id,
                name: "direct".to_string(),
                created_by: me().author_id(),
                created_at: 1_700_000_100,
                kind: ovvc_core::ChannelKind::Text,
                parent: None,
            }],
        })),
    );
    state.apply_reply(
        RequestTag::Members(dm.id),
        Ok(Reply::Members(vec![
            member(&me(), "Conleth"),
            MemberInfo {
                guild_id: dm.id,
                ..peer
            },
        ])),
    );
    state.apply_event(DaemonEvent::Presence {
        guild_id: dm.id,
        author: other().author_id(),
        status: PresenceStatus::Online,
    });
    for size in SIZES {
        render(&ctx, &mut state, size);
    }
    assert_eq!(
        state
            .selected_dm()
            .map(|conversation| conversation.guild.id),
        Some(dm.id)
    );
    assert!(!state.members.is_empty());
}

#[test]
fn a_late_profile_reply_for_a_closed_dialog_renders() {
    let ctx = context();
    let mut state = populated_state();
    state.open_profile(GUILD, other().author_id());
    state.close_profile();
    state.apply_reply(
        RequestTag::MemberProfile {
            guild_id: GUILD,
            author: other().author_id(),
        },
        Ok(Reply::Member(profile(&other(), "Ada"))),
    );
    assert!(
        state.profile.is_none(),
        "the closed dialog kept a late reply"
    );
    assert_eq!(state.dialog, Dialog::None);
    render(&ctx, &mut state, SIZES[0]);
}

#[test]
fn a_failed_profile_read_renders_the_unavailable_state() {
    let ctx = context();
    let mut state = populated_state();
    let unknown = Identity::from_seed([200u8; 32]);
    state.open_profile(GUILD, unknown.author_id());
    state.profile_loading = true;
    state.profile_stale = false;
    state.apply_reply(
        RequestTag::MemberProfile {
            guild_id: GUILD,
            author: unknown.author_id(),
        },
        Err(ovvc_ipc::IpcError::not_found("member not found")),
    );
    assert!(!state.profile_loading);
    assert!(
        !state.profile_stale,
        "a failed read must not retry by itself"
    );
    assert_eq!(state.error.as_deref(), Some("member not found"));
    for size in SIZES {
        render(&ctx, &mut state, size);
    }
}

// --- hover stability -------------------------------------------------------
//
// The reply control used to be inserted into the header only while the row was
// hovered, which let its own appearance move the timestamp and, with the
// one-frame hover lag, made the button flicker. This test drives a real
// pointer through the AccessKit tree across several frames and requires the
// control's rectangle to stay put.

#[test]
fn the_reply_band_is_reserved_and_does_not_oscillate_on_hover() {
    use egui_kittest::Harness;
    use egui_kittest::kittest::{NodeT, Queryable};

    let mut state = populated_state();
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1180.0, 760.0))
        .with_context_setup(ovvc_gui::theme::install)
        .with_max_steps(8)
        .build_ui(move |ui| {
            let mut actions = Vec::new();
            ovvc_gui::ui::draw(ui, &mut state, &mut actions);
        });
    harness.run();

    let body_rect = harness
        .query_by_label("Morning all, the daemon build is green again.")
        .expect("the first message body should be in the tree")
        .rect();
    // The band exists at rest, before the pointer enters the row. That is what
    // keeps a hover from reflowing the transcript.
    let first = harness
        .query_all_by_label("[ reply ]")
        .next()
        .expect("the reply band should be in the layout at rest")
        .rect();
    assert!(
        !harness
            .query_all_by_label("[ reply ]")
            .next()
            .expect("reply control")
            .accesskit_node()
            .is_disabled(),
        "the reply control should be live at rest"
    );

    // Rest the pointer on the control itself. The old implementation used a
    // plain `hovered()` row response, which went false under a child and made
    // the button disappear and come back on alternating frames.
    {
        let events = harness.input_mut();
        events.events.push(egui::Event::PointerMoved(first.center()));
    }
    harness.step();

    for frame in 0..12 {
        harness.step();
        let reply = harness
            .query_all_by_label("[ reply ]")
            .next()
            .unwrap_or_else(|| panic!("the reply control vanished on frame {frame}"));
        assert_eq!(
            reply.rect(),
            first,
            "the reply control moved on frame {frame}"
        );
        assert!(
            !reply.accesskit_node().is_disabled(),
            "the reply control went inactive on frame {frame}"
        );
        assert_eq!(
            harness
                .query_by_label("Morning all, the daemon build is green again.")
                .expect("message body")
                .rect(),
            body_rect,
            "the message body moved on frame {frame}"
        );
    }
}

// --- real rasterization ----------------------------------------------------
//
// `render` above runs the layout and paint code but throws the shapes away.
// These tests hand the same frames to egui's wgpu test renderer, so the whole
// pipeline runs: fonts, shaping, colours, clipping and the final pixel buffer.
// They need no window: the renderer is headless and prefers the Vulkan
// software rasterizer.

mod raster {
    use std::collections::HashSet;

    use egui_kittest::Harness;
    use ovvc_gui::theme::Palette;

    use super::*;

    struct Image {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    }

    impl Image {
        fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
            let offset = ((y * self.width + x) * 4) as usize;
            self.pixels[offset..offset + 4]
                .try_into()
                .expect("four channels")
        }

        fn unique_colors(&self) -> usize {
            self.pixels
                .as_chunks::<4>()
                .0
                .iter()
                .copied()
                .collect::<HashSet<_>>()
                .len()
        }

        fn count_near(&self, color: [u8; 4], tolerance: u8) -> usize {
            self.pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|pixel| {
                    pixel
                        .iter()
                        .zip(color.iter())
                        .all(|(a, b)| a.abs_diff(*b) <= tolerance)
                })
                .count()
        }

        fn dark_pixels(&self) -> usize {
            self.pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|pixel| pixel[0] < 32 && pixel[1] < 32 && pixel[2] < 32)
                .count()
        }

        fn bright_pixels(&self) -> usize {
            self.pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|pixel| pixel[0] > 160 && pixel[1] > 160 && pixel[2] > 160)
                .count()
        }

        fn differing(&self, other: &Self) -> usize {
            assert_eq!((self.width, self.height), (other.width, other.height));
            self.pixels
                .as_chunks::<4>()
                .0
                .iter()
                .zip(other.pixels.as_chunks::<4>().0.iter())
                .filter(|(a, b)| a != b)
                .count()
        }
    }

    /// Run the real `ui::draw` through the real egui painter and read the
    /// resulting RGBA frame back out of the software GPU.
    fn rasterize(mut state: AppState, size: [f32; 2]) -> Image {
        let mut harness = Harness::builder()
            .with_size(egui::vec2(size[0], size[1]))
            .with_context_setup(ovvc_gui::theme::install)
            .with_max_steps(8)
            .build_ui(move |ui| {
                let mut actions = Vec::new();
                ovvc_gui::ui::draw(ui, &mut state, &mut actions);
            });
        harness.run();
        let image = harness.render().expect("headless wgpu render");
        Image {
            width: image.width(),
            height: image.height(),
            pixels: image.as_raw().clone(),
        }
    }

    fn color(color: egui::Color32) -> [u8; 4] {
        color.to_array()
    }

    #[test]
    fn a_populated_window_rasterizes_palette_text_and_code() {
        let image = rasterize(populated_state(), [1180.0, 760.0]);
        assert_eq!((image.width, image.height), (1180, 760));

        let palette = Palette::default();
        let unique = image.unique_colors();
        assert!(unique > 48, "only {unique} distinct colours were rendered");

        // The terminal theme is dark; the screenshot must be too.
        assert!(
            image.dark_pixels() > image.pixels.len() / 4 / 4,
            "less than a quarter of the frame is the dark terminal background"
        );
        // Glyphs, borders and the accent all need bright pixels.
        assert!(
            image.bright_pixels() > 1_000,
            "only {} bright text/border pixels",
            image.bright_pixels()
        );

        // Chrome, body text and the accent are all painted with palette
        // colours. Allow a little tolerance for the sRGB conversion.
        assert!(
            image.count_near(color(palette.panel), 10) > 500,
            "panel colour missing"
        );
        assert!(
            image.count_near(color(palette.chat), 10) > 5_000,
            "chat colour missing"
        );
        assert!(
            image.count_near(color(palette.text), 12) > 500,
            "text colour missing"
        );
        assert!(
            image.count_near(color(palette.accent), 16) > 50,
            "accent colour missing"
        );
        // `populated_state` contains a fenced Rust block, so its background
        // has to be visible in the raster output.
        assert!(
            image.count_near(color(palette.code_bg), 10) > 100,
            "code-block background missing"
        );

        // The top chrome must differ from the transcript body.
        let top = image.pixel(4, 4);
        let middle = image.pixel(image.width / 2, image.height / 2);
        assert_ne!(
            top, middle,
            "top bar and transcript painted the same colour"
        );
    }

    #[test]
    fn different_states_and_sizes_produce_different_pixels() {
        let offline = rasterize(
            {
                let mut state = AppState::new();
                state.daemon_error = Some("connection refused".to_string());
                state
            },
            [900.0, 600.0],
        );
        let guilds = rasterize(with_guilds(), [900.0, 600.0]);
        let populated = rasterize(populated_state(), [900.0, 600.0]);

        assert_eq!((offline.width, offline.height), (900, 600));
        assert_eq!((guilds.width, guilds.height), (900, 600));
        assert_eq!((populated.width, populated.height), (900, 600));
        assert!(
            offline.differing(&guilds) > 5_000,
            "onboarding and guild list rendered almost identically"
        );
        assert!(
            guilds.differing(&populated) > 5_000,
            "empty and populated transcripts rendered almost identically"
        );
    }

    #[test]
    fn a_full_page_view_replaces_the_chat() {
        let base = rasterize(populated_state(), [1180.0, 760.0]);
        let settings = rasterize(
            {
                let mut state = populated_state();
                state.screen = ScreenView::Settings;
                state.settings_tab = SettingsTab::Preferences;
                state
            },
            [1180.0, 760.0],
        );
        assert!(
            base.differing(&settings) > 20_000,
            "opening settings changed only {} pixels",
            base.differing(&settings)
        );
    }

    #[test]
    fn a_synthetic_busy_state_rasterizes_the_loading_grid() {
        // An empty transcript keeps the history marker at the top of the
        // scroll area, where this busy-state screenshot can be read back.
        let idle = rasterize(
            {
                let mut state = populated_state();
                state.messages.clear();
                state
            },
            [900.0, 600.0],
        );
        let busy = rasterize(
            {
                let mut state = populated_state();
                state.messages.clear();
                state.history_loading = true;
                state
            },
            [900.0, 600.0],
        );

        let changed = idle.differing(&busy);
        assert!(
            changed > 100,
            "the loading grid changed only {changed} pixels"
        );

        let palette = Palette::default();
        assert!(
            busy.count_near(color(palette.accent), 16) > 0,
            "the busy state painted no accent cell"
        );
        assert!(
            busy.count_near(color(palette.accent_dim), 16) > 0
                || busy.count_near(color(palette.faint), 16) > 0,
            "the busy state painted no theme-mapped cell"
        );

        // The harness can hand back the real raster, so a screenshot can be
        // kept as evidence without a window: set OVVC_GUI_SCREENSHOT=/path.png.
        if let Some(path) = std::env::var_os("OVVC_GUI_SCREENSHOT") {
            image::save_buffer(
                &path,
                &busy.pixels,
                busy.width,
                busy.height,
                image::ColorType::Rgba8,
            )
            .expect("write the busy-state screenshot");
        }
    }
}
