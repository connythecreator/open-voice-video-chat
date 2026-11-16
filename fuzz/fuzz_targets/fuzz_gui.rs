#![no_main]

//! Fuzzes the parts of `ovvc-gui` that consume untrusted text: the Markdown
//! parser, the state reducer, and the headless egui renderer.
//!
//! The renderer runs with a real egui `Context`, so a panic in layout, text
//! shaping, dialog windows or scrolling is caught here rather than by a user.
//! Build with `cargo fuzz run fuzz_gui` from the `fuzz` directory.

use std::sync::OnceLock;

use egui::{Context, RawInput, Rect, pos2, vec2};
use libfuzzer_sys::fuzz_target;
use ovvc_core::{
    ChannelId, ChannelMeta, EventBody, EventKind, GuildId, GuildMeta, Identity, MemberInfo,
    SignedEvent,
};
use ovvc_gui::markdown;
use ovvc_gui::state::{AppState, Dialog, RequestTag};
use ovvc_ipc::{DaemonEvent, Reply};

const GUILD: GuildId = GuildId([1; 32]);
const CHANNEL: ChannelId = ChannelId([2; 16]);

/// The renderer is limited to a few kilobytes so one iteration stays fast;
/// the parser and state reducer still see the full input.
const RENDER_BYTES: usize = 8 * 1024;

fn me() -> Identity {
    Identity::from_seed([7u8; 32])
}

fn other() -> Identity {
    Identity::from_seed([9u8; 32])
}

fn sign(author: &Identity, seq: u64, kind: EventKind) -> SignedEvent {
    author
        .sign_body(EventBody {
            version: 2,
            guild_id: GUILD,
            channel_id: Some(CHANNEL),
            author: author.author_id(),
            seq,
            lamport: seq,
            timestamp: 1_700_000_000 + seq,
            prev: None,
            kind,
        })
        .expect("signing a fixed-size test event")
}

fn context() -> &'static Context {
    static CONTEXT: OnceLock<Context> = OnceLock::new();
    CONTEXT.get_or_init(|| {
        let ctx = Context::default();
        ovvc_gui::theme::install(&ctx);
        ctx
    })
}

fn render(state: &mut AppState, size: [f32; 2]) {
    let input = RawInput {
        screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(size[0], size[1]))),
        ..Default::default()
    };
    for _ in 0..2 {
        let mut output = context().run_ui(input.clone(), |ui| {
            let mut actions = Vec::new();
            ovvc_gui::ui::draw(ui, state, &mut actions);
        });
        output.textures_delta.clear();
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() > 64 * 1024 {
        return;
    }

    let text = String::from_utf8_lossy(data).into_owned();
    let _ = markdown::parse(&text);
    let _ = markdown::plain(&text);

    let preview: String = text.chars().take(RENDER_BYTES).collect();

    let mut state = AppState::new();
    state.apply_event(DaemonEvent::Connected {
        daemon_version: "fuzz".to_string(),
        author_id: me().author_id(),
    });
    state.apply_reply(
        RequestTag::Guilds,
        Ok(Reply::Guilds(vec![GuildMeta {
            id: GUILD,
            name: preview.chars().take(32).collect(),
            founder: me().author_id(),
            created_at: 1_700_000_000,
        }])),
    );
    state.apply_reply(
        RequestTag::Channels(GUILD),
        Ok(Reply::Channels(vec![ChannelMeta {
            id: CHANNEL,
            guild_id: GUILD,
            name: preview.chars().take(32).collect(),
            created_by: me().author_id(),
            created_at: 1_700_000_000,
        }])),
    );
    state.apply_reply(
        RequestTag::Members(GUILD),
        Ok(Reply::Members(vec![
            MemberInfo {
                guild_id: GUILD,
                author: me().author_id(),
                display_name: preview.chars().take(32).collect(),
                joined_at: 1_700_000_000,
            },
            MemberInfo {
                guild_id: GUILD,
                author: other().author_id(),
                display_name: preview.chars().take(32).collect(),
                joined_at: 1_700_000_000,
            },
        ])),
    );

    let message = sign(
        &other(),
        1,
        EventKind::MessageV2 {
            body: preview.clone(),
            reply_to: None,
            mentions: vec![me().author_id()],
        },
    );
    let mut events = vec![message.clone()];
    if let Ok(target) = message.id() {
        events.push(sign(
            &me(),
            2,
            EventKind::ReactionAdded {
                target,
                emoji: preview.chars().take(64).collect(),
            },
        ));
        state.reply_to = Some((target, other().author_id(), preview.clone()));
    }
    state.apply_reply(
        RequestTag::History {
            guild_id: GUILD,
            channel_id: CHANNEL,
        },
        Ok(Reply::Events(events)),
    );

    state.composer = preview.clone();
    state.dialog_input = preview.clone();
    state.search_query = preview.chars().take(64).collect();
    state.editing = state
        .messages
        .first()
        .map(|message| (message.event_id, preview.clone()));
    state.dialog = match data.first().copied().unwrap_or(0) % 6 {
        0 => Dialog::None,
        1 => Dialog::CreateGuild,
        2 => Dialog::CreateChannel,
        3 => Dialog::Join,
        4 => Dialog::Invite,
        _ => Dialog::Settings,
    };
    state.show_members = data.get(1).copied().unwrap_or(0) % 2 == 0;

    let width = 240.0 + f32::from(data.get(2).copied().unwrap_or(0)) * 4.0;
    let height = 200.0 + f32::from(data.get(3).copied().unwrap_or(0)) * 3.0;
    render(&mut state, [width, height]);
});
