//! egui rendering. Every interaction is expressed as an [`Action`]; this module
//! never talks to the daemon itself, which is what lets the headless tests run
//! the whole layout without a runtime.
//!
//! The layout follows a terminal rather than a chat app: numbered guild tabs,
//! a monospace transcript with `<author>` prefixes, an input line with a `>`
//! prompt, and a status bar along the bottom. All colours come from the
//! [`theme::Palette`] installed for the context.

use egui::text::LayoutJob;
use egui::{Align, Align2, Color32, FontFamily, FontId, RichText, Sense, Stroke, TextFormat};
use ovvc_control::{PresenceStatus, VoiceMemberState, now_ms};
use ovvc_core::{
    AuthorId, ChannelId, ChannelKind, ChannelView, DirectConversation, ErrorClass, GuildId,
    GuildMeta, MAX_PROFILE_ABOUT_BYTES, MAX_PROFILE_LINK_BYTES, MAX_PROFILE_LINKS,
};
use ovvc_ipc::{PolicyView, SearchHit, VoiceAudioStats};

use crate::markdown;
use crate::state::{
    Action, AppState, DetailLevel, Dialog, MessageView, ScreenView, SettingsTab, WizardCommune,
    WizardStep, short_id,
};
use crate::theme;

pub fn draw(ui: &mut egui::Ui, state: &mut AppState, actions: &mut Vec<Action>) {
    let t = theme::palette(ui.ctx());
    // Setup and the full-page utility views own the window: the chat chrome is
    // not drawn underneath them, so no screen has two headers or two homes for
    // the same state.
    if state.wizard_step.is_some() {
        // A full-page view owns the window, so a dialog that was open behind it
        // would be invisible and impossible to close.
        close_underlying_dialogs(state);
        draw_setup_page(ui, state, actions, &t);
        return;
    }
    if state.screen != ScreenView::Chat {
        close_underlying_dialogs(state);
    }
    match state.screen {
        ScreenView::Chat => {}
        ScreenView::Settings => {
            draw_settings_page(ui, state, actions, &t);
            return;
        }
        ScreenView::NetworkMap => {
            draw_network_map_page(ui, state, actions, &t);
            return;
        }
        ScreenView::Diagnostics => {
            draw_diagnostics_page(ui, state, actions, &t);
            return;
        }
    }
    draw_top_bar(ui, state, actions, &t);
    draw_alerts(ui, state, &t);
    draw_guild_tabs(ui, state, actions, &t);
    draw_status_bar(ui, state, &t);
    if state.selected_channel.is_some() {
        draw_composer(ui, state, actions, &t);
    }
    if state.selected_guild.is_some() {
        draw_channel_sidebar(ui, state, actions, &t);
    }
    // The member rail is always present so `[M]` has a home even when the rail
    // is collapsed.
    if state.selected_guild.is_some() {
        draw_member_panel(ui, state, actions, &t);
    }
    draw_central(ui, state, actions, &t);
    let ctx = ui.ctx().clone();
    draw_dialogs(&ctx, state, actions, &t);
}

/// Drop any overlay dialog before a full-page view draws. The overlay is not
/// painted on these screens, so leaving it open would make it invisible and
/// leave a profile target that a late reply could still match.
fn close_underlying_dialogs(state: &mut AppState) {
    if state.dialog != Dialog::None
        || state.profile_author.is_some()
        || state.profile_guild.is_some()
    {
        state.close_profile();
        state.dialog = Dialog::None;
    }
}

// --- chrome ----------------------------------------------------------------

fn draw_top_bar(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    egui::Panel::top("top")
        .exact_size(34.0)
        .frame(
            egui::Frame::new()
                .fill(t.panel)
                .stroke(Stroke::new(1.0, t.border))
                .inner_margin(egui::Margin::symmetric(10, 5)),
        )
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                // The brand keeps its `//` separator at every connection
                // state. It is identity, not daemon state, so it no longer
                // appears and disappears with the socket.
                ui.label(RichText::new("ovvc").size(15.0).strong().color(t.accent));
                ui.label(RichText::new("//").size(11.0).color(t.faint));
                ui.add_space(6.0);

                // `[members]` and `[config]` have moved into the member rail:
                // `[M]` in its header and `[T]` in the current-user card.
                let search_width = (ui.available_width() - 28.0).clamp(90.0, 520.0);
                ui.label(RichText::new("find:").size(12.0).color(t.muted));
                let response = ui.add_sized(
                    [search_width, 22.0],
                    egui::TextEdit::singleline(&mut state.search_query)
                        .frame(field_frame(t))
                        .hint_text("..."),
                );
                if response.lost_focus()
                    && ui.input(|input| input.key_pressed(egui::Key::Enter))
                    && let Some(guild_id) = state.selected_guild
                    && !state.search_query.trim().is_empty()
                {
                    actions.push(Action::Search {
                        guild_id,
                        query: state.search_query.trim().to_string(),
                    });
                }
            });
        });
}

/// A one-line strip for the last failure or the last piece of information.
///
/// `state.error` and `state.notice` are the only record the GUI keeps of a
/// refused request, a failed join or a connect error, so without this strip
/// every one of them happened silently. Errors stay until dismissed; notices
/// expire on their own in `App::expire_notice`.
/// The label and colour for one class. A bug is the only red alert; an
/// operational condition is a yellow warning rather than a failure banner.
fn alert_style(class: Option<ErrorClass>, t: &theme::Palette) -> (String, Color32) {
    match class {
        Some(ErrorClass::Bug) => ("! bug".to_string(), t.red),
        Some(ErrorClass::Operational) => ("! warning".to_string(), t.yellow),
        Some(ErrorClass::Security) => ("! security".to_string(), t.magenta),
        Some(ErrorClass::Policy) => ("! policy".to_string(), t.cyan),
        Some(ErrorClass::User) => ("! error".to_string(), t.bright),
        None => ("! error".to_string(), t.red),
    }
}

/// The message shown in the strip. A bug names the daemon log because that is
/// what a report needs; the other classes stay short.
fn alert_message(message: &str, class: Option<ErrorClass>, log: Option<&str>) -> String {
    match (class, log) {
        (Some(ErrorClass::Bug), Some(path)) => format!("{message} (log: {path})"),
        _ => message.to_string(),
    }
}

fn draw_alerts(ui: &mut egui::Ui, state: &mut AppState, t: &theme::Palette) {
    let alert = match (&state.error, &state.notice) {
        (Some(text), _) => {
            let (kind, colour) = alert_style(state.error_class, t);
            Some((
                kind,
                colour,
                alert_message(text, state.error_class, state.error_log.as_deref()),
            ))
        }
        (None, Some(text)) => Some(("notice".to_string(), t.accent, text.clone())),
        (None, None) => None,
    };
    let Some((kind, colour, text)) = alert else {
        return;
    };
    egui::Panel::top("alerts")
        .frame(
            egui::Frame::new()
                .fill(t.bg)
                .stroke(Stroke::new(1.0, t.border))
                .inner_margin(egui::Margin::symmetric(10, 3)),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(kind)
                        .size(11.0)
                        .strong()
                        .color(colour),
                );
                ui.label(RichText::new(text).size(11.0).color(t.text));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("[ dismiss ]").clicked() {
                        if state.error.is_some() {
                            state.error = None;
                        } else {
                            state.notice = None;
                        }
                    }
                });
            });
        });
}

fn draw_guild_tabs(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    egui::Panel::top("guilds")
        .frame(
            egui::Frame::new()
                .fill(t.bg)
                .stroke(Stroke::new(1.0, t.border))
                .inner_margin(egui::Margin::symmetric(10, 4)),
        )
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                // Direct conversations are guilds in the store but belong in
                // the sidebar's DM section, not among the communes.
                for (index, guild) in state
                    .guilds
                    .iter()
                    .filter(|guild| !state.is_direct_guild(guild.id))
                    .enumerate()
                {
                    let unread = guild_unread(state, guild.id);
                    if guild_tab(
                        ui,
                        index,
                        guild,
                        state.selected_guild == Some(guild.id),
                        unread,
                        t,
                    )
                    .clicked()
                    {
                        actions.push(Action::SelectGuild(guild.id));
                    }
                }
                if ui.button("[+]").on_hover_text("Create a commune").clicked() {
                    state.dialog_input.clear();
                    state.dialog = Dialog::CreateGuild;
                }
                if ui
                    .button("[join]")
                    .on_hover_text("Join a commune with an invite")
                    .clicked()
                {
                    state.dialog_input.clear();
                    state.dialog = Dialog::Join;
                }
                // Invite is no longer offered under the commune name, but the
                // action remains reachable from the guild tab.
                if state.selected_guild.is_some()
                    && ui
                        .button("[invite]")
                        .on_hover_text("Create an invite for this commune")
                        .clicked()
                {
                    state.invite_code.clear();
                    state.dialog_input.clear();
                    state.dialog = Dialog::Invite;
                }
            });
        });
}

fn guild_tab(
    ui: &mut egui::Ui,
    index: usize,
    guild: &GuildMeta,
    selected: bool,
    unread: usize,
    t: &theme::Palette,
) -> egui::Response {
    let mut label = format!("[{}] {}", index + 1, guild.name);
    if unread > 0 {
        label.push_str(&format!(" ({unread})"));
    }
    let color = if selected || unread > 0 {
        t.bright
    } else {
        t.text
    };
    let mut button = egui::Button::new(RichText::new(label).size(13.0).color(color));
    if selected {
        button = button.fill(t.accent_dim);
    }
    ui.add(button)
}

fn guild_unread(state: &AppState, guild_id: GuildId) -> usize {
    if state.selected_guild != Some(guild_id) {
        return 0;
    }
    state
        .channels
        .iter()
        .map(|channel| state.unread_for(channel.id))
        .sum()
}

fn draw_status_bar(ui: &mut egui::Ui, state: &AppState, t: &theme::Palette) {
    egui::Panel::bottom("status")
        .exact_size(22.0)
        .frame(
            egui::Frame::new()
                .fill(t.panel)
                .stroke(Stroke::new(1.0, t.border))
                .inner_margin(egui::Margin::symmetric(10, 2)),
        )
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                // The status line is the single home for global online and peer
                // state. Self identity lives in the current-user card and the
                // daemon version and log path live in diagnostics.
                let online = state.connected
                    && state
                        .selected_guild
                        .is_some_and(|guild| state.is_online(guild));
                let peers = state.online_peers();
                ui.label(
                    RichText::new(if online { "net:online" } else { "net:offline" })
                        .size(11.0)
                        .color(if online { t.green } else { t.red }),
                );
                // `connect_requested` stays set until a listen address arrives,
                // so this is the guild-startup busy state rather than an
                // offline one.
                if state.connect_requested.is_some() {
                    crate::loading::spinner(ui);
                }
                if !state.connected || state.daemon_error.is_some() {
                    separator(ui, t);
                    ui.label(RichText::new("! daemon").size(11.0).color(t.red));
                }
                if online {
                    separator(ui, t);
                    ui.label(
                        RichText::new(match peers {
                            0 => "waiting for peers".to_string(),
                            1 => "1 peer".to_string(),
                            other => format!("{other} peers"),
                        })
                        .size(11.0)
                        .color(t.muted),
                    );
                }
                separator(ui, t);
                if let Some(dm) = state.selected_dm() {
                    ui.label(
                        RichText::new(format!("dm:{}", state.dm_name(dm)))
                            .size(11.0)
                            .color(t.muted),
                    );
                } else if let Some(guild) = state.selected_guild_meta() {
                    ui.label(
                        RichText::new(format!("commune:{}", guild.name))
                            .size(11.0)
                            .color(t.muted),
                    );
                } else {
                    ui.label(RichText::new("commune:-").size(11.0).color(t.faint));
                }
                separator(ui, t);
                if let Some(channel) = state.selected_channel_meta() {
                    ui.label(
                        RichText::new(format!("{}{}", channel.kind.glyph(), channel.name))
                            .size(11.0)
                            .color(t.muted),
                    );
                } else {
                    ui.label(RichText::new("channel:-").size(11.0).color(t.faint));
                }
            });
        });
}

fn separator(ui: &mut egui::Ui, t: &theme::Palette) {
    ui.label(RichText::new("|").size(11.0).color(t.border));
}

// --- channel sidebar -------------------------------------------------------

fn draw_channel_sidebar(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    egui::Panel::left("channels")
        .exact_size(216.0)
        .resizable(false)
        .frame(
            egui::Frame::new()
                .fill(t.panel)
                .stroke(Stroke::new(1.0, t.border))
                .inner_margin(egui::Margin::symmetric(8, 6)),
        )
        .show(ui, |ui| {
            let guild_name = state
                .selected_guild_meta()
                .map(|guild| guild.name.clone())
                .unwrap_or_else(|| "commune".to_string());
            let heading = match state.selected_dm() {
                Some(conversation) => format!("-- dm @{} --", state.dm_name(conversation)),
                None => format!("-- {} --", guild_name.to_lowercase()),
            };
            ui.label(RichText::new(heading).size(t.small).color(t.muted));
            let direct = state.selected_dm().is_some();
            // Invite and join no longer sit under the commune name. The guild
            // tabs keep them, and a direct conversation has no membership row.
            ui.add_space(2.0);

            let channels = state.channels.clone();
            let categories = state.categories.clone();
            let conversations = state.direct_messages.clone();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .max_height((ui.available_height() - 96.0).max(80.0))
                .show(ui, |ui| {
                    if !conversations.is_empty() {
                        section_header(ui, "direct messages", t);
                        for conversation in &conversations {
                            if dm_row(ui, state, conversation, t).clicked() {
                                actions.push(Action::SelectGuild(conversation.guild.id));
                            }
                        }
                        ui.add_space(4.0);
                    }
                    if channels.is_empty() {
                        ui.label(RichText::new("-- no channels --").size(12.0).color(t.faint));
                    }
                    for channel in channels.iter().filter(|channel| channel.parent.is_none()) {
                        if channel_row(ui, state, channel, t).clicked() {
                            actions.push(Action::SelectChannel {
                                guild_id: channel.guild_id,
                                channel_id: channel.id,
                            });
                        }
                    }
                    for category in &categories {
                        ui.add_space(4.0);
                        section_header(ui, &category.name, t);
                        let mut any = false;
                        for channel in channels
                            .iter()
                            .filter(|channel| channel.parent == Some(category.id))
                        {
                            any = true;
                            if channel_row(ui, state, channel, t).clicked() {
                                actions.push(Action::SelectChannel {
                                    guild_id: channel.guild_id,
                                    channel_id: channel.id,
                                });
                            }
                        }
                        if !any {
                            ui.label(RichText::new("-- empty --").size(11.0).color(t.faint));
                        }
                    }
                    if state.selected_guild.is_some() && !direct {
                        ui.add_space(4.0);
                        // Wrapped, because the two buttons do not fit on one
                        // line in a narrow sidebar and a clipped `[+ category]`
                        // hides a control that works.
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            if ui.button("[+ channel]").clicked() {
                                state.dialog_input.clear();
                                state.channel_kind = ChannelKind::Text;
                                state.channel_parent = None;
                                state.dialog = Dialog::CreateChannel;
                            }
                            if ui.button("[+ category]").clicked() {
                                state.dialog_input.clear();
                                state.dialog = Dialog::CreateCategory;
                            }
                        });
                    }
                });
        });
}

fn channel_row(
    ui: &mut egui::Ui,
    state: &mut AppState,
    channel: &ChannelView,
    t: &theme::Palette,
) -> egui::Response {
    let selected = state.selected_channel == Some(channel.id);
    let unread = state.unread_for(channel.id);
    let muted = state.is_muted_channel(channel.id);
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 22.0), Sense::click());
    let fill = if selected {
        t.select_bg
    } else if response.hovered() {
        t.hover
    } else {
        Color32::TRANSPARENT
    };
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::ZERO, fill);
    let color = if selected {
        t.select_fg
    } else if unread > 0 {
        t.bright
    } else if muted {
        t.faint
    } else {
        t.muted
    };
    let painter = ui.painter().with_clip_rect(rect);
    let suffix = if muted { " [m]" } else { "" };
    painter.text(
        rect.left_center() + egui::vec2(6.0, 0.0),
        Align2::LEFT_CENTER,
        format!("{} {}{suffix}", channel.kind.glyph(), channel.name),
        FontId::monospace(13.0),
        color,
    );
    if unread > 0 {
        painter.text(
            rect.right_center() - egui::vec2(6.0, 0.0),
            Align2::RIGHT_CENTER,
            format!("[{unread}]"),
            FontId::monospace(11.0),
            if selected { t.select_fg } else { t.yellow },
        );
    }
    response.context_menu(|ui| {
        if ui.button("[channel settings]").clicked() {
            state.begin_channel_settings(channel.id);
            ui.close();
        }
    });
    response.on_hover_text(format!(
        "{} {} ({})",
        channel.kind.glyph(),
        channel.name,
        channel.kind.label()
    ))
}

/// One direct conversation in the sidebar, marked as a DM and separate from
/// the channel rows and commune tabs.
fn dm_row(
    ui: &mut egui::Ui,
    state: &AppState,
    conversation: &DirectConversation,
    t: &theme::Palette,
) -> egui::Response {
    let guild_id = conversation.guild.id;
    let selected = state.selected_guild == Some(guild_id);
    let unread = state.dm_unread_for(guild_id);
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 22.0), Sense::click());
    let fill = if selected {
        t.select_bg
    } else if response.hovered() {
        t.hover
    } else {
        Color32::TRANSPARENT
    };
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::ZERO, fill);
    let color = if selected {
        t.select_fg
    } else if unread > 0 {
        t.bright
    } else {
        t.muted
    };
    let painter = ui.painter().with_clip_rect(rect);
    let name = state.dm_name(conversation);
    // Presence is only known for the guild being shown, so an unselected DM
    // gets a neutral dash instead of claiming the peer is offline.
    let marker = state
        .presence
        .get(&conversation.peer.author)
        .map(|status| theme::presence_glyph(*status))
        .unwrap_or("-");
    painter.text(
        rect.left_center() + egui::vec2(6.0, 0.0),
        Align2::LEFT_CENTER,
        format!("{marker} dm @{name}"),
        FontId::monospace(13.0),
        color,
    );
    if unread > 0 {
        painter.text(
            rect.right_center() - egui::vec2(6.0, 0.0),
            Align2::RIGHT_CENTER,
            format!("[{unread}]"),
            FontId::monospace(11.0),
            if selected { t.select_fg } else { t.yellow },
        );
    }
    response.on_hover_text(format!("direct message with {name}"))
}

fn presence_menu(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    let current = state.own_presence;
    let glyph = theme::presence_glyph(current);
    let label = format!("{glyph} {}", presence_label(current));
    ui.menu_button(
        RichText::new(label)
            .size(11.0)
            .color(t.presence_color(current)),
        |ui| {
            for status in [
                PresenceStatus::Online,
                PresenceStatus::Away,
                PresenceStatus::DoNotDisturb,
                PresenceStatus::Offline,
            ] {
                let marker = if status == current { "*" } else { " " };
                if ui
                    .button(format!("{marker} {glyph} {}", presence_label(status)))
                    .clicked()
                {
                    state.own_presence = status;
                    if let Some(guild_id) = state.selected_guild {
                        actions.push(Action::SetPresence { guild_id, status });
                    }
                    ui.close();
                }
            }
        },
    );
}

fn presence_label(status: PresenceStatus) -> &'static str {
    match status {
        PresenceStatus::Online => "online",
        PresenceStatus::Away => "away",
        PresenceStatus::DoNotDisturb => "dnd",
        PresenceStatus::Offline => "offline",
    }
}

// --- member panel ----------------------------------------------------------

fn draw_member_panel(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    let direct = state.selected_dm().is_some();
    // When the rail is collapsed it stays a narrow strip rather than vanishing,
    // so `[M]` still has a home and the panel can be reopened without another
    // top-bar control.
    let width = if state.show_members { 186.0 } else { 32.0 };
    egui::Panel::right("members")
        .exact_size(width)
        .resizable(false)
        .frame(
            egui::Frame::new()
                .fill(t.panel)
                .stroke(Stroke::new(1.0, t.border))
                .inner_margin(egui::Margin::symmetric(8, 6)),
        )
        .show(ui, |ui| {
            if !state.show_members {
                if ui
                    .add(
                        egui::Button::new(RichText::new("[M]").size(11.0).color(t.muted))
                            .frame(false),
                    )
                    .on_hover_text("open the member panel")
                    .clicked()
                {
                    state.show_members = true;
                }
                ui.label(
                    RichText::new(format!("m\n{}", state.members.len()))
                        .size(10.0)
                        .color(t.faint),
                );
                return;
            }

            current_user_card(ui, state, actions, t);
            ui.add_space(8.0);

            // The members header owns the roster count and the `[M]` toggle.
            // The old channel-header `// N online` count is gone, so this is
            // the only participant count on the screen.
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                if ui
                    .add(
                        egui::Button::new(RichText::new("[M]").size(11.0).color(t.muted))
                            .frame(false),
                    )
                    .on_hover_text("collapse the member panel")
                    .clicked()
                {
                    state.show_members = false;
                }
                let label = if direct {
                    "direct message".to_string()
                } else {
                    format!("members {}", state.members.len())
                };
                section_header(ui, &label, t);
            });

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .max_height((ui.available_height() - 66.0).max(60.0))
                .show(ui, |ui| {
                    if state.members.is_empty() {
                        ui.label(RichText::new("-- no members --").size(11.0).color(t.faint));
                    }
                    let members = state.members.clone();
                    let guild_id = state.selected_guild;
                    for member in &members {
                        let status = state.presence_for(member.author);
                        let color = if status == ovvc_control::PresenceStatus::Offline {
                            t.faint
                        } else {
                            t.text
                        };
                        let name = if state.is_self(member.author) {
                            format!("{} (you)", member.display_name)
                        } else {
                            member.display_name.clone()
                        };
                        let response = ui
                            .horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                ui.label(
                                    RichText::new(theme::presence_glyph(status))
                                        .size(12.0)
                                        .color(t.presence_color(status)),
                                );
                                ui.add(
                                    egui::Button::new(RichText::new(name).size(12.0).color(color))
                                        .frame(false),
                                )
                                .on_hover_text("open profile")
                            })
                            .inner;
                        if response.clicked()
                            && let Some(guild_id) = guild_id
                        {
                            actions.push(Action::OpenProfile {
                                guild_id,
                                author: member.author,
                            });
                        }
                    }
                });

            // Expert adds exactly one connection block, at the bottom of the
            // rail. It is the single home for trust, key epoch and transport.
            if state.detail_level == DetailLevel::Expert {
                expert_connection_block(ui, state, t);
            }
        });
}

/// The compact identity card at the top of the member rail.
///
/// It owns self identity and self presence only. Connection, security and bio
/// detail live in Settings and in the expert connection block.
fn current_user_card(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    let width = ui.available_width().min(170.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 50.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::ZERO, t.panel);
    ui.painter().rect_stroke(
        rect,
        egui::CornerRadius::ZERO,
        Stroke::new(1.0, t.border),
        egui::StrokeKind::Inside,
    );

    let author = state.identity.unwrap_or(ovvc_core::AuthorId([0; 32]));
    let status = if state.connected {
        state.own_presence
    } else {
        PresenceStatus::Offline
    };

    let avatar_rect = egui::Rect::from_min_size(rect.left_top() + egui::vec2(6.0, 5.0), egui::vec2(40.0, 40.0));
    let mut avatar_ui = ui.new_child(egui::UiBuilder::new().max_rect(avatar_rect));
    avatar_tile(&mut avatar_ui, author, t.accent, status, t);

    let text_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 54.0, rect.top() + 5.0),
        egui::pos2(rect.right() - 28.0, rect.bottom() - 4.0),
    );
    let mut text_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(text_rect)
            .layout(egui::Layout::top_down(Align::Min)),
    );
    text_ui.spacing_mut().item_spacing.y = 1.0;
    let name = state
        .identity
        .map(|author| state.display_name(author))
        .unwrap_or_else(|| "offline".to_string());
    text_ui.label(RichText::new(name).size(12.0).color(t.bright));
    if state.connect_requested.is_some() {
        text_ui.horizontal(|ui| {
            crate::loading::spinner(ui);
            ui.label(RichText::new("connecting").size(10.0).color(t.muted));
        });
    } else if state.connected {
        presence_menu(&mut text_ui, state, actions, t);
    } else {
        text_ui.label(
            RichText::new(format!(
                "{} {}  ! daemon",
                theme::presence_glyph(PresenceStatus::Offline),
                presence_label(PresenceStatus::Offline)
            ))
            .size(10.0)
            .color(t.red),
        );
    }

    let theme_rect = egui::Rect::from_min_size(
        egui::pos2(rect.right() - 28.0, rect.top() + 3.0),
        egui::vec2(24.0, 16.0),
    );
    let mut theme_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(theme_rect)
            .layout(egui::Layout::left_to_right(Align::Center)),
    );
    if theme_ui
        .add(egui::Button::new(RichText::new("[T]").size(10.0).color(t.accent)).frame(false))
        .on_hover_text("open theme settings")
        .clicked()
    {
        state.settings_tab = SettingsTab::Preferences;
        state.screen = ScreenView::Settings;
        state.dialog = Dialog::None;
        // Another window or the TUI may have changed a setting since this one
        // attached, so settings opens on the daemon's copy.
        actions.push(Action::RefreshNotificationSettings);
    }
}

/// The one expert block on a chat or DM screen. Voice owns its own block in
/// the call panel instead.
fn expert_connection_block(ui: &mut egui::Ui, state: &AppState, t: &theme::Palette) {
    ui.add_space(6.0);
    section_header(ui, "connection", t);
    let epoch = state
        .voice
        .snapshots
        .values()
        .map(|snapshot| snapshot.key_epoch)
        .chain(state.voice.audio.as_ref().map(|audio| audio.epoch))
        .max()
        .filter(|epoch| *epoch > 0)
        .map(|epoch| format!("key epoch {epoch}"))
        .unwrap_or_else(|| "key epoch -".to_string());
    ui.label(
        RichText::new(format!("trust signed · {epoch}"))
            .size(10.0)
            .color(t.muted),
    );
    let transport = match &state.connection {
        Some(connection) if connection.connected && !connection.peers.is_empty() => "direct",
        Some(connection) if connection.connected => "connected",
        _ => "-",
    };
    ui.label(
        RichText::new(format!("transport {transport}"))
            .size(10.0)
            .color(t.muted),
    );
}

/// Draw a section label and a hairline rule that fills the rest of the row.
fn section_header(ui: &mut egui::Ui, title: &str, t: &theme::Palette) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        ui.label(
            RichText::new(title.to_lowercase())
                .size(11.0)
                .color(t.muted),
        );
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), Sense::hover());
        if rect.width() > 2.0 {
            ui.painter()
                .hline(rect.x_range(), rect.center().y, Stroke::new(1.0, t.border));
        }
    });
}

// --- chat ------------------------------------------------------------------

fn draw_central(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(t.chat)
                .stroke(Stroke::new(1.0, t.border))
                .inner_margin(egui::Margin::symmetric(12, 6)),
        )
        .show(ui, |ui| {
            if state.selected_channel.is_none() {
                onboarding(ui, state, actions, t);
                return;
            }
            draw_channel_header(ui, state, t);
            section_header(ui, "", t);
            let selected = state
                .selected_channel_meta()
                .map(|channel| (channel.id, channel.name.clone(), channel.kind));
            if let Some((channel_id, channel_name, kind)) = selected {
                match kind {
                    ChannelKind::Voice => {
                        draw_voice_panel(ui, state, channel_id, &channel_name, actions, t);
                        return;
                    }
                    ChannelKind::Forum => {
                        ui.label(
                            RichText::new(
                                "-- forum channel: no thread UI yet --",
                            )
                            .size(11.0)
                            .color(t.yellow),
                        );
                    }
                    _ => {}
                }
            }

            let query = state.search_query.trim().to_string();
            if !query.is_empty() && state.searched_query.as_deref() == Some(query.as_str()) {
                if !state.search_hits.is_empty() {
                    draw_search_results(ui, state, actions, t);
                } else if state.searching {
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new("-- searching --")
                                .size(t.small)
                                .color(t.muted),
                        );
                    });
                } else {
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new("-- no matches --")
                                .size(t.body)
                                .color(t.muted),
                        );
                        ui.label(
                            RichText::new("change the find text, or clear it to read the channel")
                                .size(t.small)
                                .color(t.faint),
                        );
                    });
                }
                return;
            }

            let mut anchor_rect = None;
            let mut jump_rect = None;
            let jump = state.jump_to;
            egui::ScrollArea::vertical()
                .id_salt("transcript")
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if state.history_loading {
                        ui.vertical_centered(|ui| {
                            ui.horizontal(|ui| {
                                crate::loading::spinner(ui);
                                ui.label(
                                    RichText::new("-- loading older messages --")
                                        .size(t.small)
                                        .color(t.muted),
                                );
                            });
                        });
                    } else if state.history_more {
                        ui.vertical_centered(|ui| {
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new("[ load older ]")
                                            .size(t.small)
                                            .color(t.muted),
                                    )
                                    .frame(false),
                                )
                                .clicked()
                                && let (Some(guild_id), Some(channel_id)) =
                                    (state.selected_guild, state.selected_channel)
                            {
                                actions.push(Action::LoadOlder {
                                    guild_id,
                                    channel_id,
                                });
                            }
                        });
                    }
                    if state.messages.is_empty() {
                        ui.add_space(24.0);
                        ui.vertical_centered(|ui| {
                            ui.label(
                                RichText::new("-- no messages --")
                                    .size(t.body)
                                    .color(t.muted),
                            );
                            ui.label(
                                RichText::new("say something to start the channel")
                                    .size(t.small)
                                    .color(t.faint),
                            );
                        });
                    }
                    let anchor = state.anchor_to;
                    let divider_before = state
                        .unread_marker
                        .map(|count| state.messages.len().saturating_sub(count));
                    let mut previous_day = None;
                    for (index, message) in state.messages.iter().enumerate() {
                        let day = format_date(message.timestamp);
                        if previous_day.as_deref() != Some(day.as_str()) {
                            day_rule(ui, &day, t);
                            previous_day = Some(day);
                        }
                        if divider_before == Some(index) {
                            new_messages_rule(ui, t);
                        }
                        let response = message_row(ui, state, message, actions, t);
                        if anchor == Some(message.event_id) {
                            anchor_rect = Some(response.rect);
                        }
                        if jump == Some(message.event_id) {
                            jump_rect = Some(response.rect);
                        }
                    }
                    if let Some(index) = divider_before
                        && index >= state.messages.len()
                    {
                        new_messages_rule(ui, t);
                    }
                    if let Some(rect) = anchor_rect {
                        // Keep the reader on the message they were looking at
                        // when an older page pushed everything down.
                        ui.scroll_to_rect(rect, Some(Align::TOP));
                    }
                    if let Some(rect) = jump_rect {
                        // A search hit opens its channel and lands on the
                        // message, instead of leaving the reader at the bottom
                        // of a transcript that may be hundreds of messages on.
                        ui.scroll_to_rect(rect, Some(Align::Center));
                    }
                });
            if anchor_rect.is_some() {
                state.anchor_to = None;
            }
            if jump_rect.is_some() {
                state.jump_to = None;
            } else if jump.is_some()
                && !state.history_loading
                && state.loaded_channel == state.selected_channel
            {
                // The hit is older than the page the window keeps in memory.
                // Say so once rather than pinning the next frames to a message
                // that is not there.
                state.jump_to = None;
                state.notice = Some(
                    "that message is older than the loaded history; scroll up to load it"
                        .to_string(),
                );
            }
        });
}

fn onboarding(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new("ovvc").size(26.0).strong().color(t.accent));
        ui.label(
            RichText::new("a terminal for peers")
                .size(12.0)
                .color(t.faint),
        );
        ui.add_space(16.0);
        if !state.connected {
            ui.label(
                RichText::new("starting the local service...")
                    .size(14.0)
                    .color(t.bright),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "ovvc starts it for you. if this message stays, close the window and open ovvc again.",
                )
                .size(12.0)
                .color(t.muted),
            );
            if let Some(error) = &state.daemon_error {
                ui.add_space(4.0);
                ui.add(egui::Label::new(RichText::new(error).color(t.red).size(11.0)).wrap());
            }
            ui.add_space(10.0);
            if ui.button("[ retry ]").clicked() {
                actions.push(Action::Refresh);
            }
        } else if state.guilds.is_empty() {
            ui.label(RichText::new("welcome to ovvc").size(14.0).color(t.bright));
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "your identity is already on this machine. start a commune of your own, or join a friend who sent you an invite.",
                )
                .size(12.0)
                .color(t.muted),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("[ start a commune ]").clicked() {
                    state.dialog_input.clear();
                    state.dialog = Dialog::CreateGuild;
                }
                if ui.button("[ join a friend ]").clicked() {
                    state.join_code.clear();
                    state.dialog = Dialog::Join;
                }
            });
        } else if state.selected_dm().is_some() {
            ui.label(
                RichText::new("direct message has no channel yet")
                    .size(14.0)
                    .color(t.yellow),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "the daemon may still be creating it; retry, or pick another commune",
                )
                .size(12.0)
                .color(t.muted),
            );
            ui.add_space(10.0);
            if ui.button("[ retry ]").clicked() {
                actions.push(Action::Refresh);
            }
        } else {
            ui.label(RichText::new("pick a commune").size(14.0).color(t.bright));
        }
    });
}

/// Calls at or below this size are fully connected under the automatic scope.
/// Above it the control plane reports a flat star and the media path forwards
/// each leaf through the elected hub. Mirrors `ovvc_media::MESH_LIMIT` so the
/// panel does not grow a media dependency to render one number.
const VOICE_CAPACITY: usize = 6;

/// The voice-channel panel: join/leave, the three local flags, the roster and
/// the daemon's audio counters. It replaces the transcript, because a voice
/// channel has no message history of its own.
fn draw_voice_panel(
    ui: &mut egui::Ui,
    state: &mut AppState,
    channel_id: ChannelId,
    channel_name: &str,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    let joined_here = state.voice_joined_here();
    let guild_id = state.selected_guild;
    let snapshot = state.voice.snapshots.get(&channel_id).cloned();
    let audio = state.voice.audio.clone();
    let muted = state.voice.muted;
    let deafened = state.voice.deafened;
    let push_to_talk = state.voice.push_to_talk;
    let can_join = guild_id.is_some();

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        if joined_here {
            if primary_button(ui, "[ leave call ]", t).clicked()
                && let Some(guild_id) = guild_id
            {
                actions.push(Action::VoiceLeave {
                    guild_id,
                    channel_id,
                });
            }
        } else if can_join
            && primary_button(ui, "[ join call ]", t).clicked()
            && let Some(guild_id) = guild_id
        {
            actions.push(Action::VoiceJoin {
                guild_id,
                channel_id,
                muted,
                deafened,
                push_to_talk,
            });
        }
        let set_flag = |actions: &mut Vec<Action>, muted, deafened, push_to_talk| {
            if let Some(guild_id) = guild_id {
                actions.push(Action::SetVoiceState {
                    guild_id,
                    channel_id,
                    muted,
                    deafened,
                    push_to_talk,
                });
            }
        };
        if voice_flag_button(ui, "mute", muted, t).clicked() {
            set_flag(actions, !muted, deafened, push_to_talk);
        }
        if voice_flag_button(ui, "deafen", deafened, t).clicked() {
            set_flag(actions, muted, !deafened, push_to_talk);
        }
        if voice_flag_button(ui, "ptt", push_to_talk, t).clicked() {
            set_flag(actions, muted, deafened, !push_to_talk);
        }
        ui.label(
            RichText::new(format!("#{channel_name}"))
                .size(11.0)
                .color(t.faint),
        );
    });

    if !joined_here {
        ui.label(
            RichText::new(
                "not in this call; the flags above are remembered and published when you join",
            )
            .size(10.0)
            .color(t.faint),
        );
    } else if push_to_talk {
        ui.label(
            RichText::new("push-to-talk is on: hold F8 to transmit")
                .size(10.0)
                .color(t.faint),
        );
    }

    ui.add_space(4.0);
    let Some(snapshot) = snapshot else {
        ui.label(
            RichText::new("-- no call in progress --")
                .size(t.small)
                .color(t.muted),
        );
        return;
    };

    let capacity = format!("{}/{}", snapshot.members.len(), VOICE_CAPACITY);
    let hub = snapshot
        .hub
        .map(|author| state.display_name(author))
        .unwrap_or_else(|| "none".to_string());
    let master = snapshot
        .master
        .map(|author| state.display_name(author))
        .unwrap_or_else(|| "none".to_string());
    let backup = snapshot
        .backup
        .map(|author| state.display_name(author))
        .unwrap_or_else(|| "none".to_string());

    // The topology note explains the shape; it never restates live counters.
    ui.label(
        RichText::new("1:1 direct · 3+ star hub + backup · mesh above six")
            .size(10.0)
            .color(t.faint),
    );

    if joined_here {
        if let Some(audio) = &audio {
            if state.detail_level == DetailLevel::Expert {
                // One expert connection block owns the roles, audio backend,
                // link counters, E2EE epoch and trust. Nothing repeats it in
                // the roster or the status line.
                ui.add_space(3.0);
                section_header(ui, "connection", t);
                ui.label(
                    RichText::new(format!(
                        "{capacity} {} [hub] {hub} [master] {master} [backup] {backup}",
                        voice_topology(&snapshot.topology)
                    ))
                    .size(10.0)
                    .color(t.muted),
                );
                ui.label(
                    RichText::new(voice_media_line(&snapshot.topology))
                        .size(10.0)
                        .color(t.muted),
                );
                ui.label(
                    RichText::new(format!(
                        "audio: backend={} peers={} latency={}ms xruns={}",
                        audio.backend, audio.peers, audio.latency_ms, audio.xruns
                    ))
                    .size(10.0)
                    .color(t.muted),
                );
                let link = voice_link_line(audio);
                let color = if audio.transport != "webrtc" {
                    t.muted
                } else if audio.relayed {
                    t.yellow
                } else {
                    t.green
                };
                ui.label(RichText::new(link).size(10.0).color(color));
                let e2ee = voice_e2ee_line(audio);
                let e2ee_color = if audio.e2ee && audio.epoch > 0 {
                    t.green
                } else {
                    t.red
                };
                ui.label(
                    RichText::new(if audio.e2ee && audio.epoch > 0 {
                        format!("{e2ee} · trust signed")
                    } else {
                        e2ee
                    })
                    .size(10.0)
                    .color(e2ee_color),
                );
            } else {
                // Simple and advanced keep the call readable without raw
                // transport internals: the link line is the one transport
                // datum they show, and the compact E2EE state stays visible.
                let link = voice_link_line(audio);
                let color = if audio.transport != "webrtc" {
                    t.muted
                } else if audio.relayed {
                    t.yellow
                } else {
                    t.green
                };
                ui.label(RichText::new(link).size(10.0).color(color));
                let active = audio.e2ee && audio.epoch > 0;
                ui.label(
                    RichText::new(if active {
                        "e2ee: active".to_string()
                    } else {
                        voice_e2ee_line(audio)
                    })
                    .size(10.0)
                    .color(if active { t.green } else { t.red }),
                );
            }
            // A missing media key or dropped frames is essential state at every
            // level, not an expert detail.
            if let Some(error) = voice_e2ee_error(audio) {
                ui.label(RichText::new(error).size(10.0).color(t.red));
            }
        } else if state.detail_level == DetailLevel::Expert {
            ui.add_space(3.0);
            section_header(ui, "connection", t);
            ui.label(
                RichText::new("audio: waiting for the daemon's audio service")
                    .size(10.0)
                    .color(t.faint),
            );
        }
        // A failed WebRTC transport still lets the control session run, so the
        // panel has to say why there is no media rather than only that the
        // audio service is waiting.
        if let Some(error) = &state.voice.error {
            ui.label(
                RichText::new(format!("! media: {error}"))
                    .size(10.0)
                    .color(t.red),
            );
        }
    }

    ui.add_space(4.0);
    section_header(ui, "members", t);
    for member in &snapshot.members {
        // The roster owns speaking, mute, deaf and PTT only. Hub, master and
        // backup are role detail and live in the expert block.
        let note = voice_markers(member.state);
        let color = if member.state.speaking {
            t.bright
        } else {
            t.text
        };
        let name = state.display_name(member.author);
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{note:<20}")).size(t.small).color(
                if member.state.speaking {
                    t.bright
                } else {
                    t.faint
                },
            ));
            ui.label(RichText::new(name).size(t.body).color(color));
        });
    }
}

fn voice_flag_button(
    ui: &mut egui::Ui,
    label: &str,
    on: bool,
    t: &theme::Palette,
) -> egui::Response {
    let text = format!("[ {label}: {} ]", if on { "on" } else { "off" });
    ui.add(
        egui::Button::new(RichText::new(text).size(11.0).color(if on {
            t.bright
        } else {
            t.muted
        }))
        .frame(false),
    )
}

fn voice_topology(topology: &ovvc_control::CallTopology) -> &'static str {
    match topology {
        ovvc_control::CallTopology::Mesh => "mesh",
        ovvc_control::CallTopology::Star { .. } => "star",
    }
}

/// The media path that is running, with the key rule that goes with it.
fn voice_media_line(topology: &ovvc_control::CallTopology) -> String {
    match topology {
        ovvc_control::CallTopology::Mesh => "media: mesh  keys: all-pairs".to_string(),
        ovvc_control::CallTopology::Star { leaves, .. } => {
            let noun = if leaves.len() == 1 { "leaf" } else { "leaves" };
            format!(
                "media: star  {} {noun}  keys: leaf->hub",
                leaves.len()
            )
        }
    }
}

/// The media path and connection quality for the VOICE panel. RTT and loss
/// belong to the WebRTC transport, so the libp2p fallback marks them as
/// unavailable rather than showing zeroes that were never measured.
fn voice_link_line(audio: &VoiceAudioStats) -> String {
    if audio.transport == "webrtc" {
        let link = if audio.relayed { "relayed" } else { "direct" };
        format!(
            "link: webrtc  rtt={}ms  lost={}  {link}",
            audio.rtt_ms, audio.packets_lost
        )
    } else {
        // The libp2p fallback never measures relay state, so it must not
        // claim the link is direct.
        format!("link: {}  rtt=-  lost=-  relay=unknown", audio.transport)
    }
}

/// The end-to-end encryption state for the VOICE panel.
///
/// A live call always has a keyring, so `e2ee = false` or `epoch = 0` means a
/// peer has no usable key and the daemon is dropping its media, which the
/// panel must not present as a working call.
fn voice_e2ee_line(audio: &VoiceAudioStats) -> String {
    if audio.e2ee && audio.epoch > 0 {
        format!("e2ee: active  epoch={}", audio.epoch)
    } else if audio.epoch == 0 {
        "! e2ee: no media key for this call; media is not flowing  epoch=0".to_string()
    } else {
        format!(
            "! e2ee: a peer has no usable media key; some media is dropped  epoch={}",
            audio.epoch
        )
    }
}

/// A clear error when frames were dropped for a missing key, the wrong epoch,
/// a replay or a failed authentication tag. `None` when the call is decrypting
/// cleanly.
fn voice_e2ee_error(audio: &VoiceAudioStats) -> Option<String> {
    (audio.e2ee_errors > 0).then(|| {
        format!(
            "! e2ee: {} media frame(s) dropped for a missing key, wrong epoch, replay or failed tag",
            audio.e2ee_errors
        )
    })
}

fn voice_markers(state: VoiceMemberState) -> String {
    let mut markers = String::new();
    if state.speaking {
        markers.push_str("[spk]");
    }
    if state.muted {
        push_voice_marker(&mut markers, "[mute]");
    }
    if state.deafened {
        push_voice_marker(&mut markers, "[deaf]");
    }
    if state.push_to_talk {
        push_voice_marker(&mut markers, "[ptt]");
    }
    markers
}

fn push_voice_marker(markers: &mut String, marker: &str) {
    if !markers.is_empty() {
        markers.push(' ');
    }
    markers.push_str(marker);
}

fn draw_channel_header(ui: &mut egui::Ui, state: &mut AppState, t: &theme::Palette) {
    let channel = state.selected_channel_meta().cloned();
    let direct = state
        .selected_dm()
        .map(|conversation| state.dm_name(conversation));
    ui.horizontal(|ui| {
        if let Some(name) = &direct {
            // A DM is a two-person guild, not a commune with a channel tree;
            // name the peer instead of framing it as a normal commune.
            ui.label(
                RichText::new(format!("dm:{name}"))
                    .size(14.0)
                    .strong()
                    .color(t.bright),
            );
        }
        if let Some(channel) = channel {
            ui.label(
                RichText::new(format!("{} {}", channel.kind.glyph(), channel.name))
                    .size(if direct.is_some() { t.small } else { 14.0 })
                    .strong()
                    .color(if direct.is_some() { t.muted } else { t.bright }),
            );
        }
        // The member list owns the participant count. Repeating an online
        // count here made the same datum live in two homes.
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            if !state.unread.is_empty()
                && let Some(channel_id) = state.selected_channel
                && state.unread_for(channel_id) > 0
            {
                ui.label(
                    RichText::new(format!("[{} unread]", state.unread_for(channel_id)))
                        .size(11.0)
                        .color(t.yellow),
                );
            }
            if let Some(channel_id) = state.selected_channel
                && ui
                    .button(RichText::new("[channel]").size(11.0).color(t.muted))
                    .on_hover_text("rename, move or mute this channel")
                    .clicked()
            {
                state.begin_channel_settings(channel_id);
            }
        });
    });
}

fn draw_search_results(
    ui: &mut egui::Ui,
    state: &AppState,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    ui.add_space(4.0);
    section_header(ui, &format!("{} results", state.search_hits.len()), t);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for hit in &state.search_hits {
                search_hit_row(ui, state, hit, actions, t);
            }
        });
}

fn search_hit_row(
    ui: &mut egui::Ui,
    state: &AppState,
    hit: &SearchHit,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    let author = hit.event.body.author;
    let name = state.display_name(author);
    let channel = state
        .channels
        .iter()
        .find(|channel| channel.id == hit.channel_id)
        .map(|channel| format!("{}{}", channel.kind.glyph(), channel.name))
        .unwrap_or_else(|| "#channel".to_string());
    let preview = markdown::plain(&hit.body);
    let preview = preview.replace('\n', " ");
    if ui
        .add(
            egui::Button::new(
                RichText::new(format!("{channel} <{name}>\n  {preview}"))
                    .size(12.0)
                    .color(t.text),
            )
            .frame(false)
            .wrap(),
        )
        .clicked()
        && let Ok(event_id) = hit.event.id()
    {
        actions.push(Action::OpenSearchHit {
            guild_id: hit.guild_id,
            channel_id: hit.channel_id,
            event_id,
        });
    }
    ui.separator();
}

fn message_row(
    ui: &mut egui::Ui,
    state: &AppState,
    message: &MessageView,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) -> egui::Response {
    let name = state.display_name(message.author);
    let is_self = state.is_self(message.author);
    let color = if is_self {
        t.accent
    } else {
        t.author_color(message.author.as_bytes())
    };
    let presence = state.presence_for(message.author);
    let mentions_me = state
        .identity
        .is_some_and(|me| message.mentions.contains(&me));

    let response = ui
        .scope(|ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 2.0);
                ui.add_space(2.0);
                avatar_tile(ui, message.author, color, presence, t);
                let width = ui.available_width();
                ui.allocate_ui_with_layout(
                    egui::vec2(width, 0.0),
                    egui::Layout::top_down(Align::Min),
                    |ui| {
                        ui.set_width(width);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 5.0;
                            ui.label(RichText::new(name).size(t.body).strong().color(color))
                                .on_hover_text(format!("identity {}", message.author));
                            if message.edited {
                                ui.label(RichText::new("[edited]").size(t.small).color(t.faint));
                            }
                            if mentions_me {
                                ui.label(RichText::new("[mention]").size(t.small).color(t.yellow));
                            }
                            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                ui.label(
                                    RichText::new(format_time(message.timestamp))
                                        .size(t.small)
                                        .color(t.muted),
                                );
                            });
                        });
                        if let Some(reply) = &message.reply_to {
                            let preview = reply
                                .body
                                .as_deref()
                                .map(|body| elide(&markdown::plain(body), 90))
                                .unwrap_or_else(|| "deleted message".to_string());
                            ui.label(
                                RichText::new(format!(
                                    "| re <{}>: {preview}",
                                    state.display_name(reply.author)
                                ))
                                .size(t.small)
                                .color(t.muted),
                            );
                        }
                        if message.deleted {
                            ui.label(
                                RichText::new("[message deleted]")
                                    .size(t.body)
                                    .italics()
                                    .color(t.faint),
                            );
                        } else {
                            render_body(ui, &message.body, t);
                        }
                        // The reply band is always in the layout, visible or
                        // not, so the hover state can never change the row
                        // height or move the message body.
                        message_control_band(ui, state, message, actions, t);
                        if !message.attachments.is_empty() {
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing = egui::vec2(4.0, 3.0);
                                for attachment in &message.attachments {
                                    let label = format!(
                                        "[file: {}  {}]",
                                        attachment.name,
                                        file_size(attachment.size)
                                    );
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                RichText::new(label).size(t.small).color(t.cyan),
                                            )
                                            .frame(false),
                                        )
                                        .on_hover_text("fetch and open this file")
                                        .clicked()
                                    {
                                        actions.push(Action::OpenAttachment {
                                            guild_id: guild_of(state, message),
                                            attachment: attachment.clone(),
                                        });
                                    }
                                }
                            });
                        }
                        if !message.reactions.is_empty() {
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing = egui::vec2(4.0, 3.0);
                                for reaction in &message.reactions {
                                    let mine = state
                                        .identity
                                        .map(|author| reaction.authors.contains(&author))
                                        .unwrap_or(false);
                                    let label = format!(
                                        "[{} {}]",
                                        reaction_label(&reaction.emoji),
                                        reaction.authors.len()
                                    );
                                    let mut button = egui::Button::new(
                                        RichText::new(label).size(t.small).color(if mine {
                                            t.bright
                                        } else {
                                            t.muted
                                        }),
                                    );
                                    if mine {
                                        button = button.fill(t.accent_dim);
                                    }
                                    if ui.add(button).clicked() {
                                        actions.push(Action::ToggleReaction {
                                            guild_id: guild_of(state, message),
                                            target: message.event_id,
                                            emoji: reaction.emoji.clone(),
                                            mine,
                                        });
                                    }
                                }
                            });
                        }
                        ui.add_space(4.0);
                    },
                );
            });
        })
        .response;

    response.context_menu(|ui| {
        if ui.button("[reply]").clicked() {
            actions.push(Action::Reply {
                event_id: message.event_id,
            });
            ui.close();
        }
        if !message.deleted {
            for (emoji, label) in [
                ("\u{1f44d}", "+1"),
                ("\u{1f602}", ":joy:"),
                ("\u{1f389}", ":tada:"),
            ] {
                let mine = state
                    .identity
                    .map(|author| message.reacted_by(emoji, author))
                    .unwrap_or(false);
                if ui.button(label).clicked() {
                    actions.push(Action::ToggleReaction {
                        guild_id: guild_of(state, message),
                        target: message.event_id,
                        emoji: emoji.to_string(),
                        mine,
                    });
                    ui.close();
                }
            }
            if is_self && ui.button("[edit]").clicked() {
                actions.push(Action::BeginEdit {
                    event_id: message.event_id,
                    body: message.body.clone(),
                });
                ui.close();
            }
            if is_self && ui.button("[delete]").clicked() {
                actions.push(Action::DeleteMessage {
                    guild_id: guild_of(state, message),
                    target: message.event_id,
                });
                ui.close();
            }
        }
    });
    response
}

/// The fixed 18 px message control band.
///
/// The band is allocated on every frame, so a pointer entering or leaving it
/// changes its fill and never its height. Reply, react and more share one
/// micro-text row; the menu buttons use a flat `Button` so no submenu arrow
/// changes the band's geometry.
fn message_control_band(
    ui: &mut egui::Ui,
    state: &AppState,
    message: &MessageView,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 18.0), Sense::hover());
    let hovered = ui.ctx().rect_contains_pointer(ui.layer_id(), rect);
    let painter = ui.painter();
    if hovered {
        painter.rect_filled(rect, egui::CornerRadius::ZERO, t.hover);
    }
    painter.rect_stroke(
        rect,
        egui::CornerRadius::ZERO,
        Stroke::new(1.0, t.border),
        egui::StrokeKind::Inside,
    );

    let inner = rect.shrink2(egui::vec2(6.0, 0.0));
    let mut band = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(egui::Layout::left_to_right(Align::Center)),
    );
    band.spacing_mut().item_spacing.x = 8.0;

    if !message.deleted
        && band
            .add(
                egui::Button::new(RichText::new("[ reply ]").size(10.0).color(t.muted))
                    .frame(false),
            )
            .clicked()
    {
        actions.push(Action::Reply {
            event_id: message.event_id,
        });
    }

    if !message.deleted {
        let react = egui::Button::new(RichText::new("[ react ]").size(10.0).color(t.muted))
            .frame(false);
        let _ = egui::containers::menu::MenuButton::from_button(react).ui(&mut band, |ui| {
            for (emoji, label) in [
                ("\u{1f44d}", "+1"),
                ("\u{1f602}", ":joy:"),
                ("\u{1f389}", ":tada:"),
            ] {
                let mine = state
                    .identity
                    .map(|author| message.reacted_by(emoji, author))
                    .unwrap_or(false);
                if ui.button(label).clicked() {
                    actions.push(Action::ToggleReaction {
                        guild_id: guild_of(state, message),
                        target: message.event_id,
                        emoji: emoji.to_string(),
                        mine,
                    });
                    ui.close();
                }
            }
        });
    }

    let more =
        egui::Button::new(RichText::new("[ more ]").size(10.0).color(t.muted)).frame(false);
    let _ = egui::containers::menu::MenuButton::from_button(more).ui(&mut band, |ui| {
        if !message.deleted && ui.button("[ reply ]").clicked() {
            actions.push(Action::Reply {
                event_id: message.event_id,
            });
            ui.close();
        }
        if state.is_self(message.author) && !message.deleted {
            if ui.button("[ edit ]").clicked() {
                actions.push(Action::BeginEdit {
                    event_id: message.event_id,
                    body: message.body.clone(),
                });
                ui.close();
            }
            if ui.button("[ delete ]").clicked() {
                actions.push(Action::DeleteMessage {
                    guild_id: guild_of(state, message),
                    target: message.event_id,
                });
                ui.close();
            }
        }
        if ui.button("[ copy id ]").clicked() {
            ui.ctx().copy_text(message.event_id.to_string());
            ui.close();
        }
    });
}

/// A small deterministic identicon. The pattern is mirrored left to right so
/// it reads as a tag rather than noise, and the author colour keeps the same
/// person recognisable across channels.
fn avatar_tile(
    ui: &mut egui::Ui,
    author: AuthorId,
    color: Color32,
    presence: PresenceStatus,
    t: &theme::Palette,
) -> egui::Response {
    let size = t.avatar;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, egui::CornerRadius::ZERO, t.input);
    painter.rect_stroke(
        rect,
        egui::CornerRadius::ZERO,
        Stroke::new(1.0, color.gamma_multiply(0.5)),
        egui::StrokeKind::Inside,
    );

    let bits = identicon(author.as_bytes());
    let pad = (size * 0.12).round().max(2.0);
    let cell = (size - pad * 2.0) / 5.0;
    for row in 0..5 {
        for column in 0..3 {
            if !bits[row * 3 + column] {
                continue;
            }
            for x in [column, 4 - column] {
                let cell_rect = egui::Rect::from_min_size(
                    egui::pos2(
                        rect.left() + pad + x as f32 * cell,
                        rect.top() + pad + row as f32 * cell,
                    ),
                    egui::vec2((cell - 1.0).max(1.0), (cell - 1.0).max(1.0)),
                );
                painter.rect_filled(cell_rect, egui::CornerRadius::ZERO, color);
            }
        }
    }

    // A presence pip in the corner, with a backing square so it stays legible
    // over an identicon cell.
    let pip = (size * 0.22).clamp(6.0, 11.0);
    let pip_rect = egui::Rect::from_min_size(
        egui::pos2(rect.right() - pip - 2.0, rect.bottom() - pip - 2.0),
        egui::vec2(pip, pip),
    );
    painter.rect_filled(pip_rect.expand(1.0), egui::CornerRadius::ZERO, t.input);
    painter.rect_filled(
        pip_rect,
        egui::CornerRadius::ZERO,
        t.presence_color(presence),
    );
    response.on_hover_text(format!("{} [{}]", author, theme::presence_glyph(presence)))
}

/// Fifteen bits of stable noise from an author id, one per identicon cell in
/// the left three columns of a five-cell-wide tile.
fn identicon(seed: &[u8]) -> [bool; 15] {
    let mut hash = 0x811c_9dc5u32;
    for byte in seed {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    let mut bits = [false; 15];
    for bit in &mut bits {
        hash ^= hash << 13;
        hash ^= hash >> 17;
        hash ^= hash << 5;
        *bit = hash & 1 == 1;
    }
    if !bits.iter().any(|set| *set) {
        bits[7] = true;
    }
    bits
}

/// The guild is implied by the selected channel for ordinary messages; a
/// caller that has a guild id can use it directly.
/// Size of an attachment as a short label, matching the terminal frontend's
/// units so the two do not disagree about the same file.
fn file_size(bytes: u64) -> String {
    ovvc_core::human_size(bytes)
}

fn guild_of(state: &AppState, _message: &MessageView) -> GuildId {
    state.selected_guild.unwrap_or(GuildId([0; 32]))
}

fn render_body(ui: &mut egui::Ui, body: &str, t: &theme::Palette) {
    let width = ui.available_width().max(80.0);
    for line in markdown::parse(body) {
        let mut job = LayoutJob::default();
        job.wrap.max_width = width;
        for span in line.spans {
            let style = span.style;
            let mut format = TextFormat {
                font_id: FontId::new(t.body, FontFamily::Monospace),
                color: if style.link {
                    t.cyan
                } else if style.mention {
                    t.accent
                } else {
                    t.text
                },
                background: if style.code {
                    t.code_bg
                } else {
                    Color32::TRANSPARENT
                },
                italics: style.italic,
                underline: if style.link {
                    Stroke::new(1.0, t.cyan)
                } else {
                    Stroke::NONE
                },
                strikethrough: if style.strike {
                    Stroke::new(1.0, t.muted)
                } else {
                    Stroke::NONE
                },
                ..Default::default()
            };
            if style.bold {
                // The bundled monospace face has no bold weight, so brightness
                // and a little tracking stand in for it.
                format.extra_letter_spacing = 0.3;
                format.color = t.bright;
            }
            if style.spoiler {
                // egui cannot reveal a span on hover inside one LayoutJob, so
                // the text stays dimmed rather than pretending to hide it.
                format.color = t.faint;
                format.background = t.code_bg;
            }
            if style.channel {
                format.color = t.cyan;
                format.underline = Stroke::new(1.0, t.cyan);
            }
            job.append(&span.text, 0.0, format);
        }
        if job.text.is_empty() {
            ui.add_space(3.0);
        } else {
            ui.add(egui::Label::new(job).wrap());
        }
    }
}

// --- composer --------------------------------------------------------------

fn draw_composer(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    egui::Panel::bottom("composer")
        .frame(
            egui::Frame::new()
                .fill(t.bg)
                .stroke(Stroke::new(1.0, t.border))
                .inner_margin(egui::Margin::symmetric(12, 6)),
        )
        .show(ui, |ui| {
            let now = now_ms();
            let typing = state
                .selected_channel
                .map(|channel| state.typing_in(channel, now))
                .unwrap_or_default();
            if !typing.is_empty() {
                let names: Vec<String> = typing
                    .iter()
                    .map(|author| state.display_name(*author))
                    .collect();
                let text = match names.len() {
                    1 => format!("-- {} is typing", names[0]),
                    2 => format!("-- {} and {} are typing", names[0], names[1]),
                    _ => format!("-- {} others are typing", names.len()),
                };
                ui.label(RichText::new(text).size(11.0).color(t.muted));
            }

            if let Some((_, author, body)) = state.reply_to.clone() {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "| re <{}>: {}",
                            state.display_name(author),
                            elide(&markdown::plain(&body), 70)
                        ))
                        .size(11.0)
                        .color(t.muted),
                    );
                    if ui.button("[x]").clicked() {
                        state.reply_to = None;
                    }
                });
            }
            if let Some((_, body)) = state.editing.clone() {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("| edit: {}", elide(&body, 70)))
                            .size(11.0)
                            .color(t.yellow),
                    );
                    if ui.button("[x]").clicked() {
                        state.editing = None;
                        state.composer.clear();
                    }
                });
            }

            // Files leave the composer only when the daemon confirms the
            // post, so a pending send is a real busy state, not a hint.
            if !state.pending_attachments.is_empty() {
                let files: usize = state.pending_attachments.values().map(Vec::len).sum();
                ui.horizontal(|ui| {
                    crate::loading::spinner(ui);
                    ui.label(
                        RichText::new(format!(
                            "-- sending {files} attachment{} --",
                            if files == 1 { "" } else { "s" }
                        ))
                        .size(11.0)
                        .color(t.muted),
                    );
                });
            }

            if !state.staged_attachments.is_empty() {
                let staged: Vec<(String, u64)> = state
                    .staged_attachments
                    .iter()
                    .map(|staged| (staged.attachment.name.clone(), staged.attachment.size))
                    .collect();
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(4.0, 3.0);
                    for (index, (name, size)) in staged.into_iter().enumerate() {
                        ui.label(
                            RichText::new(format!("[file: {name}  {}]", file_size(size)))
                                .size(11.0)
                                .color(t.cyan),
                        );
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("[x]").size(11.0).color(t.muted),
                                )
                                .frame(false),
                            )
                            .clicked()
                        {
                            actions.push(Action::ClearStagedAttachment { index });
                        }
                    }
                });
            }

            ui.horizontal(|ui| {
                let open = state.attach_open;
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new("[ attach ]")
                                .size(11.0)
                                .color(if open { t.bright } else { t.muted }),
                        )
                        .frame(false),
                    )
                    .on_hover_text("read a file and send it with the next message")
                    .clicked()
                {
                    state.attach_open = !state.attach_open;
                }
                if state.attach_open {
                    let width = (ui.available_width() - 64.0).max(120.0);
                    let response = ui.add_sized(
                        [width, 20.0],
                        egui::TextEdit::singleline(&mut state.attach_path)
                            .frame(field_frame(t))
                            .hint_text("/path/to/file"),
                    );
                    let add = ui
                        .add(
                            egui::Button::new(
                                RichText::new("[ add ]").size(11.0).color(t.bright),
                            )
                            .frame(false),
                        )
                        .clicked();
                    let entered = response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter));
                    let path = state.attach_path.trim().to_string();
                    if (add || entered) && !path.is_empty() {
                        actions.push(Action::StageAttachment {
                            path: std::path::PathBuf::from(path),
                        });
                    }
                }
            });

            ui.horizontal(|ui| {
                ui.label(RichText::new(">").size(15.0).strong().color(t.accent));
                let width = (ui.available_width() - 76.0).max(120.0);
                let response = ui.add_sized(
                    [width, 42.0],
                    egui::TextEdit::multiline(&mut state.composer)
                        .frame(field_frame(t))
                        .hint_text("message")
                        .desired_rows(1)
                        .desired_width(width),
                );
                let has_focus = response.has_focus();
                if has_focus {
                    let pasted = ui.input(|input| {
                        input
                            .events
                            .iter()
                            .filter_map(|event| match event {
                                egui::Event::Paste(text) => Some(text.lines().count()),
                                _ => None,
                            })
                            .max()
                    });
                    if let Some(lines) = pasted
                        && lines > 3
                    {
                        state.notice = Some(format!(
                            "pasted {lines} lines; enter sends the whole block, shift+enter adds a line"
                        ));
                    }
                }
                let send = ui.input(|input| {
                    has_focus && input.key_pressed(egui::Key::Enter) && !input.modifiers.shift
                });
                let clicked = primary_button(ui, "[ send ]", t).clicked();
                if send || clicked {
                    submit_composer(state, actions);
                }
            });
            ui.label(
                RichText::new("enter: send   shift+enter: newline")
                    .size(10.0)
                    .color(t.faint),
            );
        });
}

fn field_frame(t: &theme::Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(t.input)
        .stroke(Stroke::new(1.0, t.border))
        .inner_margin(egui::Margin::symmetric(6, 3))
}

fn primary_button(ui: &mut egui::Ui, label: &str, t: &theme::Palette) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(label).size(12.0).color(t.select_fg)).fill(t.select_bg))
}

fn submit_composer(state: &mut AppState, actions: &mut Vec<Action>) {
    let body = state.composer.trim().to_string();
    let Some(guild_id) = state.selected_guild else {
        return;
    };
    let Some(channel_id) = state.selected_channel else {
        return;
    };
    // An edit replaces the text of an existing message, and attachments are
    // fixed when a message is signed, so a staged file waits for the next new
    // message instead of being silently dropped here.
    if let Some((target, _)) = state.editing.clone() {
        if body.is_empty() {
            return;
        }
        actions.push(Action::EditMessage {
            guild_id,
            target,
            body,
        });
        state.editing = None;
        state.composer.clear();
        return;
    }
    let attachments = state.staged_attachments.clone();
    if body.is_empty() && attachments.is_empty() {
        return;
    }
    let mentions = state.mentions_in(&body);
    let reply_to = state.reply_to.as_ref().map(|(event_id, _, _)| *event_id);
    if attachments.is_empty() {
        actions.push(Action::SendMessage {
            guild_id,
            channel_id,
            body,
            reply_to,
            mentions,
        });
    } else {
        // The chips stay until the daemon confirms the post, and the pending
        // copy keyed by this send's id is what tells the reply handler which
        // ones it may clear. A second send gets its own entry, so two posts in
        // flight cannot clear each other's files.
        let send_id = state.next_send_id;
        state.next_send_id = state.next_send_id.wrapping_add(1);
        state
            .pending_attachments
            .insert(send_id, attachments.clone());
        actions.push(Action::SendRichMessage {
            send_id,
            guild_id,
            channel_id,
            body,
            reply_to,
            mentions,
            attachments,
        });
    }
    state.reply_to = None;
    state.composer.clear();
}

// --- dialogs ---------------------------------------------------------------

fn draw_dialogs(
    ctx: &egui::Context,
    state: &mut AppState,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    // A profile target belongs to the profile dialog. Opening any other
    // overlay from the top bar or a sidebar button replaces that dialog, so
    // drop the target and its drafts before the new window draws.
    if state.dialog != Dialog::Profile
        && (state.profile_guild.is_some() || state.profile_author.is_some())
    {
        state.close_profile();
    }
    if state.dialog == Dialog::None {
        return;
    }
    // Escape closes the overlay, checked before the window runs so a focused
    // text field inside it cannot consume the key first. The profile dialog
    // goes through `close_profile` so a reply still in flight is dropped.
    if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
        if state.dialog == Dialog::Profile {
            state.close_profile();
        } else {
            state.dialog = Dialog::None;
        }
        return;
    }
    let title = match state.dialog {
        Dialog::CreateGuild => "create commune",
        Dialog::CreateChannel => "create channel",
        Dialog::CreateCategory => "create category",
        Dialog::ChannelSettings => "channel settings",
        Dialog::Join => "join commune",
        Dialog::Invite => "invite",
        Dialog::Profile => "profile",
        Dialog::None => return,
    };
    let mut open = true;
    egui::Window::new(format!("-- {title} --"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(match state.dialog {
            Dialog::Profile => 460.0,
            _ => 420.0,
        })
        .show(ctx, |ui| {
            match state.dialog {
                Dialog::CreateGuild => {
                    ui.label(RichText::new("commune name").size(12.0).color(t.muted));
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut state.dialog_input).frame(field_frame(t)),
                    );
                    if primary_button(ui, "[ create ]", t).clicked()
                        || (response.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                    {
                        let name = state.dialog_input.trim().to_string();
                        if !name.is_empty() {
                            actions.push(Action::CreateGuild { name });
                        }
                    }
                }
                Dialog::CreateChannel => {
                    ui.label(RichText::new("channel name").size(12.0).color(t.muted));
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut state.dialog_input).frame(field_frame(t)),
                    );
                    ui.add_space(4.0);
                    kind_picker(ui, state, t);
                    ui.add_space(4.0);
                    category_picker(ui, state, t);
                    ui.add_space(6.0);
                    if primary_button(ui, "[ create ]", t).clicked()
                        || (response.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                    {
                        let name = state.dialog_input.trim().to_string();
                        if !name.is_empty()
                            && let Some(guild_id) = state.selected_guild
                        {
                            actions.push(Action::CreateChannelIn {
                                guild_id,
                                name,
                                kind: state.channel_kind,
                                parent: state.channel_parent,
                            });
                        }
                    }
                    if state.channel_kind == ChannelKind::Forum {
                        ui.label(
                            RichText::new(
                                "forum channels exist in the tree but have no thread UI yet",
                            )
                            .size(10.0)
                            .color(t.faint),
                        );
                    }
                }
                Dialog::CreateCategory => {
                    ui.label(RichText::new("category name").size(12.0).color(t.muted));
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut state.dialog_input).frame(field_frame(t)),
                    );
                    if primary_button(ui, "[ create ]", t).clicked()
                        || (response.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                    {
                        let name = state.dialog_input.trim().to_string();
                        if !name.is_empty()
                            && let Some(guild_id) = state.selected_guild
                        {
                            actions.push(Action::CreateCategory { guild_id, name });
                        }
                    }
                }
                Dialog::ChannelSettings => {
                    let Some(channel_id) = state.channel_target else {
                        ui.label(RichText::new("no channel selected").color(t.faint));
                        return;
                    };
                    let Some(channel) = state.channel_view(channel_id).cloned() else {
                        ui.label(RichText::new("that channel no longer exists").color(t.faint));
                        return;
                    };
                    ui.label(
                        RichText::new(format!(
                            "{} {}  ({})",
                            channel.kind.glyph(),
                            channel.name,
                            channel.kind.label()
                        ))
                        .size(12.0)
                        .color(t.bright),
                    );
                    ui.label(
                        RichText::new(format!("identity {}", channel.id))
                            .size(10.0)
                            .color(t.faint),
                    );
                    ui.separator();
                    ui.label(RichText::new("name").size(12.0).color(t.muted));
                    ui.add(
                        egui::TextEdit::singleline(&mut state.rename_input).frame(field_frame(t)),
                    );
                    ui.add_space(4.0);
                    category_picker(ui, state, t);
                    ui.add_space(4.0);
                    let muted = state.is_muted_channel(channel_id);
                    if ui
                        .button(if muted {
                            "[ unmute channel ]"
                        } else {
                            "[ mute channel ]"
                        })
                        .clicked()
                    {
                        let muted = !muted;
                        state.set_channel_muted(channel_id, muted);
                        actions.push(Action::SetChannelMuted {
                            channel_id,
                            muted,
                        });
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        if primary_button(ui, "[ apply ]", t).clicked()
                            && let Some(guild_id) = state.selected_guild
                        {
                            let name = state.rename_input.trim().to_string();
                            if !name.is_empty() && name != channel.name {
                                actions.push(Action::RenameChannel {
                                    guild_id,
                                    channel_id,
                                    name,
                                });
                            }
                            if state.channel_parent != channel.parent {
                                actions.push(Action::MoveChannel {
                                    guild_id,
                                    channel_id,
                                    parent: state.channel_parent,
                                });
                            }
                            state.dialog = Dialog::None;
                        }
                        if ui.button("[ close ]").clicked() {
                            state.dialog = Dialog::None;
                        }
                    });
                }
                Dialog::Join => {
                    ui.label(
                        RichText::new(
                            "paste the invite your friend sent, then press [ join ]. the code is the whole invitation.",
                        )
                        .size(11.0)
                        .color(t.text),
                    );
                    ui.add(
                        egui::TextEdit::multiline(&mut state.join_code)
                            .frame(field_frame(t))
                            .desired_rows(3)
                            .desired_width(f32::INFINITY)
                            .hint_text("paste an invite code"),
                    );
                    if primary_button(ui, "[ join ]", t).clicked() {
                        let code = state.join_code.trim().to_string();
                        if !code.is_empty() {
                            actions.push(Action::JoinGuild { code });
                        }
                    }
                    ui.label(
                        RichText::new(
                            "your friend's commune must be online while you join. after that, this machine remembers the commune and brings it back online next launch.",
                        )
                        .size(9.0)
                        .color(t.faint),
                    );
                }
                Dialog::Invite => {
                    let can_invite = state
                        .selected_guild
                        .is_some_and(|guild| state.can_invite(guild));
                    let addresses = state.invite_addresses();
                    ui.label(
                        RichText::new(
                            "this code is the whole invitation. send it to your friend, then have them open ovvc, choose [ join a friend ] in setup, paste it, and press [ join ].",
                        )
                        .size(11.0)
                        .color(t.text),
                    );
                    ui.add_space(4.0);
                    if state.invite_code.is_empty() {
                        ui.label(
                            RichText::new(
                                "ovvc puts one of these addresses into the code. your friend's app dials the first one it can reach.",
                            )
                            .size(10.0)
                            .color(t.muted),
                        );
                        if !can_invite {
                            ui.label(
                                RichText::new(
                                    "take the commune online from settings > network first, then come back. a wildcard or loopback address is not put into an invite.",
                                )
                                .size(10.0)
                                .color(t.yellow),
                            );
                        } else if addresses.is_empty() {
                            ui.label(
                                RichText::new(
                                    "no shareable address yet: wait a moment for the daemon to report a listener, or set the public invite address in settings > network.",
                                )
                                .size(10.0)
                                .color(t.yellow),
                            );
                        }
                        if !addresses.is_empty() {
                            for address in &addresses {
                                ui.label(
                                    RichText::new(format!("  {address}"))
                                        .size(9.0)
                                        .color(t.muted),
                                );
                            }
                        }
                        ui.add_space(4.0);
                        let create = ui.add_enabled(
                            can_invite,
                            egui::Button::new(
                                RichText::new("[ create invite ]").size(12.0).color(
                                    if can_invite { t.select_fg } else { t.faint },
                                ),
                            )
                            .fill(if can_invite { t.select_bg } else { t.panel }),
                        );
                        if create.clicked()
                            && let Some(guild_id) = state.selected_guild
                        {
                            actions.push(Action::CreateInvite { guild_id });
                        }
                        ui.label(
                            RichText::new(
                                "same network: a local address is enough. different networks: forward TCP 41000 on the router and set the public invite address in settings > network.",
                            )
                            .size(9.0)
                            .color(t.faint),
                        );
                    } else {
                        ui.add(
                            egui::TextEdit::multiline(&mut state.invite_code)
                                .frame(field_frame(t))
                                .desired_rows(3)
                                .desired_width(f32::INFINITY),
                        );
                        ui.horizontal(|ui| {
                            if ui.button("[ copy ]").clicked() {
                                ui.ctx().copy_text(state.invite_code.clone());
                            }
                            if ui.button("[ create another ]").clicked()
                                && can_invite
                                && let Some(guild_id) = state.selected_guild
                            {
                                actions.push(Action::CreateInvite { guild_id });
                            }
                        });
                        ui.label(
                            RichText::new(
                                "the commune must stay online while your friend joins. the code is a bearer code: anyone holding it can join.",
                            )
                            .size(9.0)
                            .color(t.faint),
                        );
                    }
                    ui.add_space(4.0);
                    if ui.button("[ join a friend instead ]").clicked() {
                        state.join_code.clear();
                        state.dialog = Dialog::Join;
                    }
                }
                Dialog::Profile => profile_panel(ui, state, actions, t),
                Dialog::None => {}
            }
        });
    if !open {
        // The X follows the same path as Escape, so a profile reply still in
        // flight is dropped rather than landing on a closed dialog.
        if state.dialog == Dialog::Profile {
            state.close_profile();
        } else {
            state.dialog = Dialog::None;
        }
    }
}

fn kind_picker(ui: &mut egui::Ui, state: &mut AppState, t: &theme::Palette) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(RichText::new("kind").size(11.0).color(t.muted));
        for kind in [
            ChannelKind::Text,
            ChannelKind::Announcement,
            ChannelKind::Voice,
            ChannelKind::Forum,
        ] {
            let mut button = egui::Button::new(
                RichText::new(format!("[{} {}]", kind.glyph(), kind.label()))
                    .size(11.0)
                    .color(if state.channel_kind == kind {
                        t.select_fg
                    } else {
                        t.muted
                    }),
            );
            if state.channel_kind == kind {
                button = button.fill(t.select_bg);
            }
            if ui.add(button).clicked() {
                state.channel_kind = kind;
            }
        }
    });
}

fn category_picker(ui: &mut egui::Ui, state: &mut AppState, t: &theme::Palette) {
    let categories = state.categories.clone();
    let current = categories
        .iter()
        .find(|category| Some(category.id) == state.channel_parent)
        .map(|category| category.name.clone())
        .unwrap_or_else(|| "root".to_string());
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(RichText::new("category").size(11.0).color(t.muted));
        egui::ComboBox::from_id_salt("category-picker")
            .selected_text(RichText::new(format!("-- {current} --")).size(11.0))
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(state.channel_parent.is_none(), "root")
                    .clicked()
                {
                    state.channel_parent = None;
                }
                for category in &categories {
                    if ui
                        .selectable_label(state.channel_parent == Some(category.id), &category.name)
                        .clicked()
                    {
                        state.channel_parent = Some(category.id);
                    }
                }
            });
    });
}

/// The built-in palettes as bracket buttons, used by the settings tab and the
/// getting-started window so both offer the same list.
///
/// Switching repaints the whole window immediately: the palette lives in the
/// egui context, and `theme::apply` reinstalls the style. The choice is written
/// to `theme.toml`, so the next launch starts in the same one.
fn theme_picker(ui: &mut egui::Ui, state: &mut AppState, t: &theme::Palette) {
    let current = theme::active(ui.ctx());
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for preset in theme::Preset::all() {
            let selected = current == *preset;
            let mut button = egui::Button::new(
                RichText::new(format!("[ {} ]", preset.label()))
                    .size(11.0)
                    .color(if selected { t.select_fg } else { t.text }),
            );
            if selected {
                button = button.fill(t.select_bg);
            }
            if ui.add(button).on_hover_text(preset.description()).clicked() {
                theme::apply(ui.ctx(), *preset);
                if let Err(error) = theme::save_preset(*preset) {
                    state.error = Some(format!("could not save the theme: {error:#}"));
                }
            }
        }
    });
    ui.label(
        RichText::new(current.description())
            .size(10.0)
            .color(t.muted),
    );
    ui.label(
        RichText::new(format!(
            "saved to {}",
            theme::theme_path()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "the built-in palette".to_string())
        ))
        .size(10.0)
        .color(t.faint),
    );
}

/// The member profile window, including its edit mode for your own profile.
///
/// The read-only section shows the server-resolved fields; editing only turns
/// about, pronouns and links into drafts. Nothing is sent until `[ save ]`
/// validates and trims them, so a half-typed link stays local.
fn profile_panel(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    let Some(author) = state.profile_author else {
        ui.label(
            RichText::new("no profile selected")
                .size(12.0)
                .color(t.faint),
        );
        return;
    };
    let is_self = state.is_self(author);
    let fallback_name = state.display_name(author);
    let profile = state.profile.clone();
    let name = profile
        .as_ref()
        .map(|profile| profile.display_name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or(fallback_name);
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(if is_self {
                format!("{name} (you)")
            } else {
                name
            })
            .size(14.0)
            .strong()
            .color(if is_self { t.accent } else { t.bright }),
        );
        ui.label(
            RichText::new(format!("id {}", short_id(&author.to_string())))
                .size(10.0)
                .color(t.faint),
        );
    });

    if let Some(profile) = &profile {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(
                RichText::new(format!("joined {}", format_date(profile.joined_at)))
                    .size(10.0)
                    .color(t.muted),
            );
            ui.label(
                RichText::new(format!("messages {}", profile.message_count))
                    .size(10.0)
                    .color(t.muted),
            );
            if profile.last_activity > 0 {
                ui.label(
                    RichText::new(format!(
                        "last activity {} {}",
                        format_date(profile.last_activity),
                        format_time(profile.last_activity)
                    ))
                    .size(10.0)
                    .color(t.muted),
                );
            } else {
                ui.label(
                    RichText::new("last activity never")
                        .size(10.0)
                        .color(t.faint),
                );
            }
        });
        if profile.updated_at == 0 {
            ui.label(
                RichText::new("this profile has never been published")
                    .size(10.0)
                    .color(t.faint),
            );
        }
    } else if state.profile_loading || state.profile_stale {
        ui.horizontal(|ui| {
            crate::loading::spinner(ui);
            ui.label(
                RichText::new("-- loading profile --")
                    .size(12.0)
                    .color(t.muted),
            );
        });
    } else {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("-- profile unavailable --")
                    .size(12.0)
                    .color(t.yellow),
            );
            if ui.button("[ retry ]").clicked() {
                state.profile_stale = true;
            }
        });
    }

    ui.separator();

    if state.profile_editing {
        ui.label(RichText::new("about").size(12.0).color(t.muted));
        ui.add(
            egui::TextEdit::multiline(&mut state.profile_about)
                .frame(field_frame(t))
                .desired_rows(4)
                .desired_width(f32::INFINITY)
                .hint_text("a short introduction"),
        );
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("pronouns").size(12.0).color(t.muted));
            ui.add(
                egui::TextEdit::singleline(&mut state.profile_pronouns)
                    .frame(field_frame(t))
                    .desired_width(180.0)
                    .hint_text("they/them"),
            );
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new("links (one http(s) link per line)")
                .size(12.0)
                .color(t.muted),
        );
        ui.add(
            egui::TextEdit::multiline(&mut state.profile_links)
                .frame(field_frame(t))
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .hint_text("https://example.com"),
        );
        ui.label(
            RichText::new(format!(
                "about {} / {MAX_PROFILE_ABOUT_BYTES} bytes, up to {MAX_PROFILE_LINKS} links of {MAX_PROFILE_LINK_BYTES} bytes",
                state.profile_about.len()
            ))
            .size(10.0)
            .color(t.faint),
        );
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let saving = state.profile_saving;
            let save = if saving {
                ui.add_enabled(
                    false,
                    egui::Button::new(RichText::new("[ saving... ]").size(12.0).color(t.muted)),
                )
            } else {
                primary_button(ui, "[ save ]", t)
            };
            if save.clicked() {
                match state.profile_save_request() {
                    Ok(action) => {
                        state.profile_saving = true;
                        actions.push(action);
                    }
                    // The daemon validates again; this first pass keeps a
                    // half-typed field from ever leaving the window.
                    Err(problem) => state.error = Some(problem),
                }
            }
            if ui.button("[ cancel ]").clicked() {
                state.cancel_profile_edit();
            }
        });
        return;
    }

    if let Some(profile) = &profile {
        section_header(ui, "about", t);
        if profile.about.trim().is_empty() {
            ui.label(RichText::new("(no about)").size(12.0).color(t.faint));
        } else {
            ui.add(
                egui::Label::new(
                    RichText::new(profile.about.clone())
                        .size(t.body)
                        .color(t.text),
                )
                .wrap(),
            );
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("pronouns").size(11.0).color(t.muted));
            ui.label(
                RichText::new(if profile.pronouns.trim().is_empty() {
                    "(none)".to_string()
                } else {
                    profile.pronouns.clone()
                })
                .size(11.0)
                .color(t.bright),
            );
        });
        section_header(ui, "links", t);
        if profile.links.is_empty() {
            ui.label(RichText::new("(none)").size(11.0).color(t.faint));
        } else {
            // This frontend has no link-opening helper, so links are shown as
            // text instead of shelling out to a browser.
            for link in &profile.links {
                ui.add(
                    egui::Label::new(RichText::new(link.clone()).size(11.0).color(t.cyan)).wrap(),
                );
            }
        }
    }

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if is_self {
            let can_edit = profile.is_some();
            if ui
                .add_enabled(
                    can_edit,
                    egui::Button::new(
                        RichText::new("[ edit profile ]")
                            .size(12.0)
                            .color(if can_edit { t.bright } else { t.faint }),
                    ),
                )
                .clicked()
            {
                state.begin_profile_edit();
            }
        } else {
            let context = state.profile_guild.or(state.selected_guild);
            if let Some(context) = context {
                let opening = state.dm_opening == Some(author);
                let label = if opening {
                    "[ opening... ]"
                } else {
                    "[ message ]"
                };
                if ui
                    .add_enabled(
                        !opening,
                        egui::Button::new(RichText::new(label).size(12.0).color(if opening {
                            t.muted
                        } else {
                            t.bright
                        })),
                    )
                    .clicked()
                {
                    state.dm_opening = Some(author);
                    actions.push(Action::OpenDirectMessage {
                        context,
                        peer: author,
                    });
                }
            }
        }
        if ui.button("[ close ]").clicked() {
            state.close_profile();
        }
    });
}

fn floor_is_empty(view: &PolicyView) -> bool {
    let floor = &view.client_floor;
    floor.discovery.is_none()
        && floor.transport.is_none()
        && floor.padding.is_none()
        && floor.author_identity.is_none()
        && floor.timestamps.is_none()
        && floor.hardware_keys.is_none()
        && floor.plugins.is_none()
}

/// One row per policy axis: guild value, client floor, effective value and
/// whether the client floor raised it.
fn policy_rows(view: &PolicyView) -> Vec<(&'static str, String, String, String, bool)> {
    let guild = &view.guild;
    let floor = &view.client_floor;
    let effective = &view.effective;
    let raised = |axis: &str| view.raised_by_client.iter().any(|name| name == axis);
    vec![
        (
            "discovery",
            format!("{:?}", guild.discovery),
            floor
                .discovery
                .map(|value| format!("{value:?}"))
                .unwrap_or_else(|| "-".to_string()),
            format!("{:?}", effective.discovery),
            raised("discovery"),
        ),
        (
            "transport",
            format!("{:?}", guild.transport),
            floor
                .transport
                .map(|value| format!("{value:?}"))
                .unwrap_or_else(|| "-".to_string()),
            format!("{:?}", effective.transport),
            raised("transport"),
        ),
        (
            "padding",
            format!("{:?}", guild.padding),
            floor
                .padding
                .map(|value| format!("{value:?}"))
                .unwrap_or_else(|| "-".to_string()),
            format!("{:?}", effective.padding),
            raised("padding"),
        ),
        (
            "author_identity",
            format!("{:?}", guild.author_identity),
            floor
                .author_identity
                .map(|value| format!("{value:?}"))
                .unwrap_or_else(|| "-".to_string()),
            format!("{:?}", effective.author_identity),
            raised("author_identity"),
        ),
        (
            "timestamps",
            format!("{:?}", guild.timestamps),
            floor
                .timestamps
                .map(|value| format!("{value:?}"))
                .unwrap_or_else(|| "-".to_string()),
            format!("{:?}", effective.timestamps),
            raised("timestamps"),
        ),
        (
            "hardware_keys",
            format!("{:?}", guild.hardware_keys),
            floor
                .hardware_keys
                .map(|value| format!("{value:?}"))
                .unwrap_or_else(|| "-".to_string()),
            format!("{:?}", effective.hardware_keys),
            raised("hardware_keys"),
        ),
        (
            "plugins",
            format!("{:?}", guild.plugins),
            floor
                .plugins
                .map(|value| format!("{value:?}"))
                .unwrap_or_else(|| "-".to_string()),
            format!("{:?}", effective.plugins),
            raised("plugins"),
        ),
    ]
}

// --- full-page views -------------------------------------------------------

/// A 34 px page header with a close control. The app top bar is not drawn
/// underneath a full-page view, so each page owns its own header.
fn page_header(
    ui: &mut egui::Ui,
    id: &str,
    title: &str,
    close_label: &str,
    t: &theme::Palette,
) -> bool {
    let mut close = false;
    egui::Panel::top(egui::Id::unique(id))
        .exact_size(34.0)
        .frame(
            egui::Frame::new()
                .fill(t.panel)
                .stroke(Stroke::new(1.0, t.border))
                .inner_margin(egui::Margin::symmetric(10, 5)),
        )
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.label(RichText::new(title).size(14.0).strong().color(t.bright));
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new(close_label).size(11.0).color(t.muted),
                            )
                            .frame(false),
                        )
                        .clicked()
                    {
                        close = true;
                    }
                });
            });
        });
    close
}

fn settings_label(
    ui: &mut egui::Ui,
    label: &str,
    tag: Option<&str>,
    t: &theme::Palette,
) -> egui::Response {
    ui.add_space(5.0);
    ui.horizontal(|ui| {
        let response = ui.label(RichText::new(label).size(11.0).color(t.muted));
        if let Some(tag) = tag {
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(tag).size(9.0).color(t.faint));
            });
        }
        response
    })
    .inner
}

fn row_divider(ui: &mut egui::Ui, t: &theme::Palette) {
    ui.add_space(4.0);
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 1.0),
        Sense::hover(),
    );
    ui.painter()
        .hline(rect.x_range(), rect.center().y, Stroke::new(1.0, t.border));
}

fn settings_value(ui: &mut egui::Ui, value: &str, t: &theme::Palette) {
    ui.label(RichText::new(value).size(12.0).color(t.text));
}

/// Change the global level and persist it. Only a real change pushes an
/// action, so a click that lands on the current level does not write a file.
fn set_detail_level(state: &mut AppState, actions: &mut Vec<Action>, level: DetailLevel) {
    if state.detail_level == level {
        return;
    }
    state.detail_level = level;
    actions.push(Action::SetDetailLevel(level));
}

/// The horizontal detail slider. It is a global control, not a per-tab
/// setting, and it keeps its three labels and square thumb at every width.
fn detail_slider(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    let width = ui.available_width().min(300.0);
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, 30.0), Sense::click_and_drag());
    let track_y = rect.top() + 8.0;
    let left = rect.left() + 4.0;
    let right = rect.right() - 4.0;
    let current = state.detail_level.index();
    let thumb_x = left + (right - left) * (current as f32 / 2.0);

    let painter = ui.painter();
    painter.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(left, track_y - 2.0),
            egui::pos2(right, track_y + 2.0),
        ),
        egui::CornerRadius::ZERO,
        t.border,
    );
    painter.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(left, track_y - 2.0),
            egui::pos2(thumb_x, track_y + 2.0),
        ),
        egui::CornerRadius::ZERO,
        t.accent,
    );
    for index in 0..3 {
        let x = left + (right - left) * (index as f32 / 2.0);
        painter.rect_filled(
            egui::Rect::from_center_size(egui::pos2(x, track_y), egui::vec2(2.0, 8.0)),
            egui::CornerRadius::ZERO,
            if index as usize <= current { t.accent } else { t.border },
        );
    }
    painter.rect_filled(
        egui::Rect::from_center_size(egui::pos2(thumb_x, track_y), egui::vec2(8.0, 12.0)),
        egui::CornerRadius::ZERO,
        t.accent,
    );
    for (index, level) in DetailLevel::ALL.iter().enumerate() {
        let x = left + (right - left) * (index as f32 / 2.0);
        painter.text(
            egui::pos2(x, rect.bottom()),
            if index == 0 {
                Align2::LEFT_BOTTOM
            } else if index == 2 {
                Align2::RIGHT_BOTTOM
            } else {
                Align2::CENTER_BOTTOM
            },
            level.label(),
            FontId::monospace(10.0),
            if *level == state.detail_level {
                t.accent
            } else {
                t.faint
            },
        );
    }

    response.widget_info(|| {
        egui::WidgetInfo::slider(
            true,
            state.detail_level.index() as f64,
            "detail level slider",
        )
    });
    if response.clicked() {
        response.request_focus();
    }
    if response.clicked() || response.dragged() {
        if let Some(pos) = response.interact_pointer_pos() {
            let fraction = ((pos.x - left) / (right - left)).clamp(0.0, 1.0);
            let index = (fraction * 2.0).round() as usize;
            set_detail_level(state, actions, DetailLevel::from_index(index));
        }
    }
    if response.has_focus() {
        let (left_key, right_key) = ui.input(|input| {
            (
                input.key_pressed(egui::Key::ArrowLeft),
                input.key_pressed(egui::Key::ArrowRight),
            )
        });
        if left_key {
            set_detail_level(
                state,
                actions,
                DetailLevel::from_index(current.saturating_sub(1)),
            );
        }
        if right_key {
            set_detail_level(
                state,
                actions,
                DetailLevel::from_index((current + 1).min(2)),
            );
        }
    }
    response.on_hover_text("detail level (global): simple, advanced, expert");
}

fn level_rail(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
    width: f32,
) {
    ui.vertical(|ui| {
        ui.set_min_width(width);
        ui.set_max_width(width);
        ui.label(
            RichText::new("detail level (global)")
                .size(13.0)
                .strong()
                .color(t.bright),
        );
        detail_slider(ui, state, actions, t);
        ui.add_space(10.0);
        for (level, text) in [
            (DetailLevel::Simple, "identity, avatar, bio, presence"),
            (
                DetailLevel::Advanced,
                "+ pronouns, connection, theme, notifications, quiet hours",
            ),
            (
                DetailLevel::Expert,
                "+ trust, transport, roles, media keys, diagnostics",
            ),
        ] {
            ui.label(
                RichText::new(format!("{:<10} {text}", level.label()))
                    .size(10.0)
                    .color(if state.detail_level == level {
                        t.bright
                    } else {
                        t.faint
                    }),
            );
        }
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "the level changes which detail is visible globally, as opposed to changing its values.",
            )
            .size(10.0)
            .color(t.muted),
        );
        ui.label(
            RichText::new(
                "simple and advanced keep the clean detail; expert adds one connection block per screen. the level persists.",
            )
            .size(10.0)
            .color(t.muted),
        );
        ui.add_space(8.0);
        section_header(ui, "keyboard", t);
        ui.label(
            RichText::new(
                "tab / shift+tab move focus\nleft / right change tab\nenter opens the focused tab, esc closes",
            )
            .size(10.0)
            .color(t.muted),
        );
        ui.add_space(8.0);
        egui::Frame::new()
            .fill(t.bg)
            .stroke(Stroke::new(1.0, t.border))
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.set_min_width((width - 16.0).max(40.0));
                ui.label(
                    RichText::new("TUI level control")
                        .size(10.0)
                        .color(t.bright),
                );
                ui.label(
                    RichText::new(format!(
                        "level: [ {} ] advanced expert",
                        state.detail_level.label()
                    ))
                    .size(10.0)
                    .color(t.muted),
                );
                ui.label(
                    RichText::new("changed with [ and ] or l")
                        .size(10.0)
                        .color(t.faint),
                );
            });
        ui.add_space(8.0);
        ui.label(
            RichText::new(format!(
                "at {} tab / {} level",
                state.settings_tab.label(),
                state.detail_level.label()
            ))
            .size(9.0)
            .color(t.faint),
        );
    });
}

fn presence_buttons(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for status in [
            PresenceStatus::Online,
            PresenceStatus::Away,
            PresenceStatus::DoNotDisturb,
            PresenceStatus::Offline,
        ] {
            let selected = status == state.own_presence;
            let label = format!(
                "{} {}",
                theme::presence_glyph(status),
                presence_label(status)
            );
            let mut button = egui::Button::new(
                RichText::new(label)
                    .size(11.0)
                    .color(if selected { t.select_fg } else { t.muted }),
            );
            if selected {
                button = button.fill(t.select_bg);
            }
            if ui.add(button).clicked()
                && let Some(guild_id) = state.selected_guild
            {
                state.own_presence = status;
                actions.push(Action::SetPresence { guild_id, status });
            }
        }
    });
}

fn e2ee_epoch(state: &AppState) -> Option<u32> {
    state
        .voice
        .snapshots
        .values()
        .map(|snapshot| snapshot.key_epoch)
        .chain(state.voice.audio.as_ref().map(|audio| audio.epoch))
        .max()
        .filter(|epoch| *epoch > 0)
}

fn connection_summary(state: &AppState) -> String {
    let online = state
        .selected_guild
        .is_some_and(|guild| state.is_online(guild));
    let peers = state.online_peers();
    let transport = match &state.connection {
        Some(connection) if connection.connected && !connection.peers.is_empty() => "direct",
        Some(connection) if connection.connected => "connected",
        _ => "-",
    };
    let epoch = e2ee_epoch(state)
        .map(|epoch| format!("e2ee epoch {epoch}"))
        .unwrap_or_else(|| "e2ee epoch -".to_string());
    format!(
        "net:{} · {peers} peers · {transport} · {epoch}",
        if online { "online" } else { "offline" }
    )
}

fn notifications_rows(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    if ui
        .checkbox(
            &mut state.notification_settings.desktop,
            "desktop notifications",
        )
        .changed()
    {
        actions.push(Action::SetDesktopNotifications(
            state.notification_settings.desktop,
        ));
    }
    if ui
        .checkbox(&mut state.notification_settings.dnd, "do not disturb")
        .changed()
    {
        actions.push(Action::SetDoNotDisturb(state.notification_settings.dnd));
    }
    ui.label(
        RichText::new("dnd and quiet hours suppress in-app alerts as well as desktop popups")
            .size(9.0)
            .color(t.faint),
    );
    ui.horizontal(|ui| {
        let start = ui.add(
            egui::TextEdit::singleline(&mut state.quiet_start)
                .frame(field_frame(t))
                .desired_width(64.0)
                .hint_text("22:00"),
        );
        ui.label(RichText::new("to").size(11.0).color(t.muted));
        let end = ui.add(
            egui::TextEdit::singleline(&mut state.quiet_end)
                .frame(field_frame(t))
                .desired_width(64.0)
                .hint_text("07:00"),
        );
        if (start.changed() || end.changed()) && state.commit_quiet_hours() {
            actions.push(Action::SetQuietHours(
                state.notification_settings.quiet_hours,
            ));
        }
    });
    ui.label(
        RichText::new("quiet hours are UTC; the daemon has no timezone database")
            .size(9.0)
            .color(t.faint),
    );
}

fn settings_list(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
    width: f32,
) {
    ui.vertical(|ui| {
        ui.set_min_width(width);
        ui.set_max_width(width);
        ui.label(
            RichText::new(state.settings_tab.label())
                .size(14.0)
                .strong()
                .color(t.bright),
        );
        let description = match state.settings_tab {
            SettingsTab::User => "identity and profile; the level rail decides which rows are visible",
            SettingsTab::VoiceVideo => "voice devices, push-to-talk and the media path",
            SettingsTab::Network => "listen and dial addresses, relay and diagnostics",
            SettingsTab::Preferences => "notifications and appearance; theme is written to theme.toml",
        };
        ui.label(RichText::new(description).size(10.0).color(t.muted));
        ui.add_space(4.0);

        match state.settings_tab {
            SettingsTab::User => {
                settings_label(ui, "avatar", Some("simple"), t);
                ui.horizontal(|ui| {
                    let author = state.identity.unwrap_or(ovvc_core::AuthorId([0; 32]));
                    let mut preview = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(egui::Rect::from_min_size(
                                ui.cursor().min,
                                egui::vec2(24.0, 24.0),
                            ))
                            .layout(egui::Layout::left_to_right(Align::Center)),
                    );
                    avatar_tile(
                        &mut preview,
                        author,
                        t.accent,
                        state.own_presence,
                        t,
                    );
                    ui.add_space(26.0);
                    ui.label(RichText::new("initials are used").size(10.0).color(t.muted));
                });
                row_divider(ui, t);

                settings_label(ui, "display name", Some("simple"), t);
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut state.dialog_input)
                            .frame(field_frame(t))
                            .desired_width(260.0)
                            .hint_text("enter a display name"),
                    );
                    if primary_button(ui, "[ save name ]", t).clicked()
                        && let Some(guild_id) = state.selected_guild
                    {
                        let name = state.dialog_input.trim().to_string();
                        if !name.is_empty() {
                            actions.push(Action::SetDisplayName {
                                guild_id,
                                display_name: name,
                            });
                        }
                    }
                });
                ui.label(
                    RichText::new("shown instead of your key; the expert level keeps the signing detail")
                        .size(9.0)
                        .color(t.faint),
                );
                row_divider(ui, t);

                if state.detail_level >= DetailLevel::Advanced {
                    settings_label(ui, "pronouns", Some("advanced"), t);
                    let pronouns = state
                        .profile
                        .as_ref()
                        .map(|profile| profile.pronouns.clone())
                        .unwrap_or_default();
                    settings_value(
                        ui,
                        if pronouns.trim().is_empty() {
                            "(not set; edit your profile)"
                        } else {
                            &pronouns
                        },
                        t,
                    );
                    row_divider(ui, t);
                }

                settings_label(ui, "bio", Some("simple"), t);
                let bio = state
                    .profile
                    .as_ref()
                    .map(|profile| profile.about.clone())
                    .unwrap_or_default();
                ui.add(
                    egui::Label::new(
                        RichText::new(if bio.trim().is_empty() {
                            "(no about set; edit your profile)".to_string()
                        } else {
                            bio
                        })
                        .size(11.0)
                        .color(t.text),
                    )
                    .wrap(),
                );
                ui.label(
                    RichText::new(format!(
                        "{MAX_PROFILE_ABOUT_BYTES} bytes, up to {MAX_PROFILE_LINKS} links of {MAX_PROFILE_LINK_BYTES} bytes"
                    ))
                    .size(9.0)
                    .color(t.faint),
                );
                row_divider(ui, t);

                settings_label(ui, "presence", Some("simple"), t);
                presence_buttons(ui, state, actions, t);
                row_divider(ui, t);

                if state.detail_level >= DetailLevel::Advanced {
                    settings_label(ui, "connection", Some("advanced"), t);
                    settings_value(ui, &connection_summary(state), t);
                    row_divider(ui, t);

                    settings_label(ui, "theme", Some("advanced"), t);
                    theme_picker(ui, state, t);
                    row_divider(ui, t);

                    settings_label(ui, "notifications", Some("advanced"), t);
                    notifications_rows(ui, state, actions, t);
                    row_divider(ui, t);
                }

                if state.detail_level == DetailLevel::Expert {
                    settings_label(ui, "media key epoch", Some("expert"), t);
                    settings_value(
                        ui,
                        &e2ee_epoch(state)
                            .map(|epoch| format!("epoch {epoch}"))
                            .unwrap_or_else(|| "no active call key".to_string()),
                        t,
                    );
                    row_divider(ui, t);

                    settings_label(ui, "diagnostics", Some("expert"), t);
                    if ui.button("[ open diagnostics ]").clicked() {
                        state.diagnostics_confirm_shutdown = false;
                        state.screen = ScreenView::Diagnostics;
                    }
                    row_divider(ui, t);
                }

                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "display name and bio are shown instead of your key. the level changes visibility, not values.",
                    )
                    .size(9.0)
                    .color(t.faint),
                );
            }
            SettingsTab::VoiceVideo => {
                settings_label(ui, "input device", Some("simple"), t);
                settings_value(ui, "default microphone", t);
                row_divider(ui, t);

                settings_label(ui, "output device", Some("simple"), t);
                settings_value(ui, "default speakers", t);
                row_divider(ui, t);

                settings_label(ui, "push-to-talk", Some("simple"), t);
                ui.horizontal(|ui| {
                    let on = state.voice.push_to_talk;
                    if ui
                        .button(if on { "[ ptt: on ]" } else { "[ ptt: off ]" })
                        .clicked()
                    {
                        state.voice.push_to_talk = !on;
                        if let (Some(guild_id), Some(channel_id)) =
                            (state.voice.guild, state.voice.channel)
                        {
                            actions.push(Action::SetVoiceState {
                                guild_id,
                                channel_id,
                                muted: state.voice.muted,
                                deafened: state.voice.deafened,
                                push_to_talk: state.voice.push_to_talk,
                            });
                        }
                    }
                    ui.label(RichText::new("key: F8").size(10.0).color(t.faint));
                });
                row_divider(ui, t);

                if state.detail_level == DetailLevel::Expert {
                    settings_label(ui, "e2ee epoch", Some("expert"), t);
                    settings_value(
                        ui,
                        &e2ee_epoch(state)
                            .map(|epoch| format!("epoch {epoch} active"))
                            .unwrap_or_else(|| "no active call".to_string()),
                        t,
                    );
                    row_divider(ui, t);

                    if let Some(audio) = &state.voice.audio {
                        settings_label(ui, "media backend", Some("expert"), t);
                        settings_value(
                            ui,
                            &format!(
                                "backend={} peers={} latency={}ms xruns={}",
                                audio.backend, audio.peers, audio.latency_ms, audio.xruns
                            ),
                            t,
                        );
                        row_divider(ui, t);
                    }
                }

                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "the daemon opens the default microphone and speakers; there is no per-window device selection.",
                    )
                    .size(9.0)
                    .color(t.faint),
                );
            }
            SettingsTab::Network => {
                let selected = state.selected_guild;
                let online = selected.is_some_and(|guild| state.is_online(guild));
                settings_label(ui, "state", Some("simple"), t);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(if online { "net:online" } else { "net:offline" })
                            .size(11.0)
                            .color(if online { t.green } else { t.red }),
                    );
                    if let Some(guild_id) = selected {
                        if online {
                            if ui.button("[ go offline ]").clicked() {
                                actions.push(Action::Disconnect { guild_id });
                            }
                        } else if ui.button("[ go online ]").clicked() {
                            actions.push(Action::Connect { guild_id });
                        }
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("[ open network map ]").clicked() {
                        state.screen = ScreenView::NetworkMap;
                    }
                    if ui.button("[ open diagnostics ]").clicked() {
                        state.diagnostics_confirm_shutdown = false;
                        state.screen = ScreenView::Diagnostics;
                    }
                });
                row_divider(ui, t);

                settings_label(ui, "invite a friend", Some("simple"), t);
                ui.label(
                    RichText::new(
                        "this machine listens on TCP 41000 when the commune is online. the invite contains the address your friend dials.",
                    )
                    .size(10.0)
                    .color(t.muted),
                );
                if let Some(guild_id) = selected {
                    let ready = state.can_invite(guild_id);
                    if ui
                        .add_enabled(
                            ready,
                            egui::Button::new(RichText::new("[ open invite ]").size(11.0).color(
                                if ready { t.accent } else { t.faint },
                            )),
                        )
                        .on_hover_text(if ready {
                            "create and copy the invite code"
                        } else {
                            "take the commune online first"
                        })
                        .clicked()
                    {
                        state.invite_code.clear();
                        state.dialog = Dialog::Invite;
                    }
                }
                ui.label(
                    RichText::new(
                        "same network: the local address in the invite is enough. different networks: forward TCP 41000 on the router and set the public invite address below.",
                    )
                    .size(9.0)
                    .color(t.faint),
                );
                row_divider(ui, t);

                if state.detail_level >= DetailLevel::Advanced {
                    let label = settings_label(ui, "listen address", Some("advanced"), t);
                    ui.add(
                        egui::TextEdit::singleline(&mut state.network_listen)
                            .desired_width(320.0)
                            .frame(field_frame(t)),
                    )
                    .labelled_by(label.id);
                    ui.label(
                        RichText::new(
                            "keep the fixed port 41000 so the router forward and the invite keep pointing at the same place.",
                        )
                        .size(9.0)
                        .color(t.faint),
                    );
                    row_divider(ui, t);

                    let label = settings_label(ui, "public invite address", Some("advanced"), t);
                    ui.add(
                        egui::TextEdit::singleline(&mut state.network_public)
                            .hint_text("/ip4/<public ip>/tcp/41000 or /dns4/<name>/tcp/41000")
                            .desired_width(320.0)
                            .frame(field_frame(t)),
                    )
                    .labelled_by(label.id);
                    ui.label(
                        RichText::new(
                            "used first in the invite. the daemon cannot discover the router's public address, and this router has no UPnP or NAT-PMP, so enter it yourself.",
                        )
                        .size(9.0)
                        .color(t.faint),
                    );
                    row_divider(ui, t);

                    let label = settings_label(ui, "dial address", Some("advanced"), t);
                    ui.add(
                        egui::TextEdit::singleline(&mut state.network_dial)
                            .hint_text("optional, for a peer that cannot be discovered")
                            .desired_width(320.0)
                            .frame(field_frame(t)),
                    )
                    .labelled_by(label.id);
                    row_divider(ui, t);

                    let label = settings_label(ui, "relay address", Some("advanced"), t);
                    ui.add(
                        egui::TextEdit::singleline(&mut state.network_relay)
                            .hint_text("optional, /ip4/<relay>/tcp/4001/p2p/<peer id>")
                            .desired_width(320.0)
                            .frame(field_frame(t)),
                    )
                    .labelled_by(label.id);
                    ui.label(
                        RichText::new(
                            "the daemon supports a relay circuit. ovvc runs no relay service, so leave this empty unless you have a relay to point at.",
                        )
                        .size(9.0)
                        .color(t.faint),
                    );
                    row_divider(ui, t);

                    settings_label(ui, "peers", Some("advanced"), t);
                    if let Some(connection) = &state.connection {
                        let connected = connection
                            .peers
                            .iter()
                            .filter(|peer| peer.connected)
                            .count();
                        settings_value(ui, &format!("{connected} connected"), t);
                    } else {
                        settings_value(ui, "(not reported yet)", t);
                    }
                    row_divider(ui, t);
                }

                if state.detail_level == DetailLevel::Expert {
                    settings_label(ui, "listen addresses", Some("expert"), t);
                    if let Some(connection) = &state.connection {
                        if connection.listen_addresses.is_empty() {
                            ui.label(RichText::new("(none yet)").size(10.0).color(t.faint));
                        }
                        for address in &connection.listen_addresses {
                            ui.label(RichText::new(address).size(10.0).color(t.muted));
                        }
                    } else {
                        ui.label(RichText::new("(not reported yet)").size(10.0).color(t.faint));
                    }
                    row_divider(ui, t);

                    settings_label(ui, "peer identities", Some("expert"), t);
                    if let Some(connection) = &state.connection {
                        for peer in &connection.peers {
                            ui.label(
                                RichText::new(format!(
                                    "{} {}",
                                    if peer.connected { "*" } else { "-" },
                                    peer.peer
                                ))
                                .size(10.0)
                                .color(if peer.connected { t.text } else { t.faint }),
                            );
                        }
                    } else {
                        ui.label(RichText::new("(none yet)").size(10.0).color(t.faint));
                    }
                    row_divider(ui, t);

                    settings_label(ui, "e2ee epoch", Some("expert"), t);
                    settings_value(
                        ui,
                        &e2ee_epoch(state)
                            .map(|epoch| format!("epoch {epoch}"))
                            .unwrap_or_else(|| "-".to_string()),
                        t,
                    );
                    row_divider(ui, t);

                    settings_label(ui, "policy (read-only)", Some("expert"), t);
                    match &state.policy {
                        None => settings_value(ui, "not loaded yet", t),
                        Some(view) => {
                            ui.label(
                                RichText::new(format!(
                                    "guild {} / {}",
                                    view.guild.profile.name(),
                                    view.guild.topology.name()
                                ))
                                .size(10.0)
                                .color(t.text),
                            );
                            for (axis, guild, client, effective, raised) in policy_rows(view) {
                                ui.label(
                                    RichText::new(format!(
                                        "{axis:<16} guild {guild:<14} floor {client:<14} effective {effective}{}",
                                        if raised { " *" } else { "" }
                                    ))
                                    .size(9.0)
                                    .color(if raised { t.yellow } else { t.muted }),
                                );
                            }
                            let floor = if floor_is_empty(view) {
                                "none".to_string()
                            } else {
                                "set".to_string()
                            };
                            ui.label(
                                RichText::new(format!("client floor: {floor}"))
                                    .size(9.0)
                                    .color(t.faint),
                            );
                            ui.label(
                                RichText::new("this is a read-only view")
                                    .size(9.0)
                                    .color(t.faint),
                            );
                        }
                    }
                    row_divider(ui, t);
                }

                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "a configured relay is used for a circuit; ovvc itself runs no relay service.",
                    )
                    .size(9.0)
                    .color(t.faint),
                );
            }
            SettingsTab::Preferences => {
                settings_label(ui, "theme", Some("simple"), t);
                theme_picker(ui, state, t);
                row_divider(ui, t);

                settings_label(ui, "setup", Some("simple"), t);
                if ui.button("[ open setup again ]").clicked() {
                    state.wizard_step = Some(WizardStep::Identity);
                    state.screen = ScreenView::Chat;
                }
                ui.label(
                    RichText::new(
                        "runs the first-run steps again without deleting any commune or message.",
                    )
                    .size(9.0)
                    .color(t.faint),
                );
                row_divider(ui, t);

                if state.detail_level >= DetailLevel::Advanced {
                    settings_label(ui, "notifications", Some("advanced"), t);
                    notifications_rows(ui, state, actions, t);
                    row_divider(ui, t);

                    settings_label(ui, "member panel default", Some("advanced"), t);
                    let mut members = state.show_members;
                    if ui.checkbox(&mut members, "open").changed() {
                        state.show_members = members;
                    }
                    row_divider(ui, t);
                }

                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "desktop notifications and quiet hours are stored by the daemon; the GUI reads the daemon copy when settings opens.",
                    )
                    .size(9.0)
                    .color(t.faint),
                );
                ui.label(
                    RichText::new(format!(
                        "theme is saved to {}; the next launch starts in the same preset.",
                        theme::theme_path()
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "the built-in palette".to_string())
                    ))
                    .size(9.0)
                    .color(t.faint),
                );
            }
        }
    });
}

fn draw_settings_page(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    if ui.ctx().input(|input| input.key_pressed(egui::Key::Escape)) {
        state.screen = ScreenView::Chat;
        return;
    }
    // Arrow keys change tabs only when no text field has focus, so editing a
    // name with the caret does not jump to another tab.
    let focus = ui.ctx().memory(|memory| memory.focused());
    if focus.is_none() {
        let (left, right) = ui.input(|input| {
            (
                input.key_pressed(egui::Key::ArrowLeft),
                input.key_pressed(egui::Key::ArrowRight),
            )
        });
        if left || right {
            let current = SettingsTab::ALL
                .iter()
                .position(|tab| *tab == state.settings_tab)
                .unwrap_or(0);
            let next = if left {
                current.saturating_sub(1)
            } else {
                (current + 1).min(SettingsTab::ALL.len() - 1)
            };
            state.settings_tab = SettingsTab::ALL[next];
        }
    }

    if page_header(ui, "settings-header", "settings", "[x]", t) {
        state.screen = ScreenView::Chat;
        return;
    }

    egui::Panel::top("settings-tabs")
        .exact_size(44.0)
        .frame(
            egui::Frame::new()
                .fill(t.panel)
                .stroke(Stroke::new(1.0, t.border))
                .inner_margin(egui::Margin::symmetric(10, 6)),
        )
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                let compact = ui.available_width() < 640.0;
                let tab_width = if compact {
                    ((ui.available_width() - 16.0) / SettingsTab::ALL.len() as f32).max(38.0)
                } else {
                    104.0
                };
                for tab in SettingsTab::ALL {
                    let selected = state.settings_tab == tab;
                    let text = if compact {
                        tab.icon().to_string()
                    } else {
                        format!("{} {}", tab.icon(), tab.label())
                    };
                    let mut button = egui::Button::new(
                        RichText::new(text)
                            .size(11.0)
                            .color(if selected { t.select_fg } else { t.muted }),
                    );
                    if selected {
                        button = button.fill(t.select_bg);
                    }
                    if ui.add_sized([tab_width, 28.0], button).clicked() {
                        state.settings_tab = tab;
                    }
                    ui.add_space(4.0);
                }
                if !compact {
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(
                                "tab / shift+tab: focus  left / right: tab  esc: close",
                            )
                            .size(9.0)
                            .color(t.faint),
                        );
                    });
                }
            });
        });

    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(t.chat)
                .stroke(Stroke::new(1.0, t.border))
                .inner_margin(egui::Margin::symmetric(12, 8)),
        )
        .show(ui, |ui| {
            let wide = ui.available_width() >= 900.0;
            if wide {
                let rail = 380.0_f32.min(ui.available_width() * 0.4);
                let list = (ui.available_width() - rail - 16.0).max(240.0);
                ui.horizontal_top(|ui| {
                    settings_list(ui, state, actions, t, list);
                    ui.add_space(16.0);
                    level_rail(ui, state, actions, t, rail);
                });
            } else {
                let width = ui.available_width();
                settings_list(ui, state, actions, t, width);
                ui.add_space(12.0);
                level_rail(ui, state, actions, t, width);
            }
        });
}

fn setup_step_content(
    ui: &mut egui::Ui,
    state: &mut AppState,
    _actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    let Some(step) = state.wizard_step else {
        return;
    };
    ui.label(
        RichText::new(format!(
            "step {} / {}  {}",
            step.index(),
            WizardStep::ALL.len(),
            step.label()
        ))
        .size(14.0)
        .strong()
        .color(t.bright),
    );
    ui.add_space(8.0);

    match step {
        WizardStep::Identity => {
            settings_label(ui, "display name", Some("required"), t);
            ui.add(
                egui::TextEdit::singleline(&mut state.wizard_name)
                    .frame(field_frame(t))
                    .desired_width(320.0)
                    .hint_text("enter a display name"),
            );
            row_divider(ui, t);

            settings_label(ui, "identity key", Some("read only"), t);
            settings_value(
                ui,
                &state
                    .identity
                    .map(|author| short_id(&author.to_string()))
                    .unwrap_or_else(|| "not generated yet".to_string()),
                t,
            );
            ui.label(
                RichText::new(
                    "your key is generated locally. nothing is sent anywhere during setup.",
                )
                .size(9.0)
                .color(t.faint),
            );
            row_divider(ui, t);
            ui.label(
                RichText::new("next is disabled until a display name is entered. no name or presence is prefilled.")
                    .size(9.0)
                    .color(t.faint),
            );
        }
        WizardStep::Commune => {
            ui.label(
                RichText::new(
                    "this is your first commune. start one here, or join the one a friend invited you to.",
                )
                .size(10.0)
                .color(t.muted),
            );
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                for choice in [WizardCommune::Create, WizardCommune::Join] {
                    let selected = state.wizard_commune == choice;
                    let mut button = egui::Button::new(
                        RichText::new(format!("[ {} ]", choice.label()))
                            .size(12.0)
                            .color(if selected { t.select_fg } else { t.text }),
                    );
                    if selected {
                        button = button.fill(t.select_bg);
                    }
                    if ui.add(button).clicked() {
                        state.wizard_commune = choice;
                    }
                }
            });
            ui.add_space(8.0);
            match state.wizard_commune {
                WizardCommune::Create => {
                    settings_label(ui, "commune name", Some("required"), t);
                    ui.add(
                        egui::TextEdit::singleline(&mut state.wizard_commune_name)
                            .frame(field_frame(t))
                            .desired_width(320.0)
                            .hint_text("for example: late night devs"),
                    );
                    row_divider(ui, t);
                    ui.label(
                        RichText::new(
                            "the commune is created on this machine and starts with a #general channel. it is not sent to a server.",
                        )
                        .size(9.0)
                        .color(t.faint),
                    );
                    ui.label(
                        RichText::new(
                            "after you finish, ovvc brings it online and shows the invite to send a friend.",
                        )
                        .size(9.0)
                        .color(t.faint),
                    );
                }
                WizardCommune::Join => {
                    settings_label(ui, "invite code", Some("required"), t);
                    ui.add(
                        egui::TextEdit::multiline(&mut state.wizard_join_code)
                            .frame(field_frame(t))
                            .desired_rows(3)
                            .desired_width(f32::INFINITY)
                            .hint_text("paste the invite your friend sent"),
                    );
                    row_divider(ui, t);
                    ui.label(
                        RichText::new(
                            "the friend's app must be running and their commune online while you join. the code is the whole invitation.",
                        )
                        .size(9.0)
                        .color(t.faint),
                    );
                }
            }
        }
        WizardStep::Profile => {
            settings_label(ui, "avatar", Some("optional"), t);
            ui.horizontal(|ui| {
                let author = state.identity.unwrap_or(ovvc_core::AuthorId([0; 32]));
                let mut preview = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(egui::Rect::from_min_size(
                            ui.cursor().min,
                            egui::vec2(40.0, 40.0),
                        ))
                        .layout(egui::Layout::left_to_right(Align::Center)),
                );
                avatar_tile(
                    &mut preview,
                    author,
                    t.accent,
                    PresenceStatus::Offline,
                    t,
                );
                ui.add_space(42.0);
                ui.label(RichText::new("initials are used").size(11.0).color(t.muted));
            });
            ui.label(
                RichText::new("avatars are initials in v0.1; the TUI always uses initials.")
                    .size(9.0)
                    .color(t.faint),
            );
            row_divider(ui, t);

            settings_label(ui, "bio and links", Some("after setup"), t);
            settings_value(ui, "set them from your profile editor", t);
            ui.label(
                RichText::new("setup does not publish a profile; the profile editor does.")
                    .size(9.0)
                    .color(t.faint),
            );
            row_divider(ui, t);
        }
        WizardStep::Theme => {
            ui.label(
                RichText::new("no theme is selected until you choose one")
                    .size(10.0)
                    .color(t.muted),
            );
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                for (index, preset) in theme::Preset::all().iter().enumerate() {
                    let selected = state.wizard_theme_index == Some(index);
                    let mut button = egui::Button::new(
                        RichText::new(format!("[ {} ]", preset.label()))
                            .size(12.0)
                            .color(if selected { t.select_fg } else { t.text }),
                    );
                    if selected {
                        button = button.fill(t.select_bg);
                    }
                    if ui.add(button).on_hover_text(preset.description()).clicked() {
                        state.wizard_theme_index = Some(index);
                        // The preview follows the current selection only after
                        // a choice is made.
                        theme::apply(ui.ctx(), *preset);
                    }
                }
            });
            ui.add_space(6.0);
            match state
                .wizard_theme_index
                .and_then(|index| theme::Preset::all().get(index))
            {
                Some(preset) => {
                    ui.label(
                        RichText::new(preset.description())
                            .size(10.0)
                            .color(t.muted),
                    );
                }
                None => {
                    ui.label(
                        RichText::new("no theme selected")
                            .size(10.0)
                            .color(t.faint),
                    );
                }
            }
            row_divider(ui, t);
        }
        WizardStep::VoiceVideo => {
            settings_label(ui, "input device", Some("daemon default"), t);
            settings_value(ui, "default microphone", t);
            row_divider(ui, t);

            settings_label(ui, "output device", Some("daemon default"), t);
            settings_value(ui, "default speakers", t);
            row_divider(ui, t);

            settings_label(ui, "push-to-talk", Some("off by default"), t);
            settings_value(ui, "off, key: F8, change it in the call panel", t);
            row_divider(ui, t);
            ui.label(
                RichText::new("the daemon opens the default audio device; the call panel owns the live voice controls.")
                    .size(9.0)
                    .color(t.faint),
            );
        }
        WizardStep::Network => {
            settings_label(ui, "listen address", Some("fixed port"), t);
            ui.add(
                egui::TextEdit::singleline(&mut state.network_listen)
                    .frame(field_frame(t))
                    .desired_width(320.0)
                    .hint_text("/ip4/0.0.0.0/tcp/41000"),
            );
            ui.label(
                RichText::new(
                    "ovvc listens on TCP 41000. a friend on the same network can reach that address directly.",
                )
                .size(9.0)
                .color(t.faint),
            );
            row_divider(ui, t);

            settings_label(ui, "public invite address", Some("optional"), t);
            ui.add(
                egui::TextEdit::singleline(&mut state.network_public)
                    .frame(field_frame(t))
                    .desired_width(320.0)
                    .hint_text("/ip4/203.0.113.5/tcp/41000 or /dns4/example.net/tcp/41000"),
            );
            ui.label(
                RichText::new(
                    "a friend outside your network dials this. forward TCP 41000 on the router and put the public IP or DNS name here. this router has no UPnP or NAT-PMP, so the forward is manual.",
                )
                .size(9.0)
                .color(t.faint),
            );
            row_divider(ui, t);

            settings_label(ui, "dial address", Some("optional"), t);
            ui.add(
                egui::TextEdit::singleline(&mut state.network_dial)
                    .frame(field_frame(t))
                    .desired_width(320.0)
                    .hint_text("optional, for a peer that cannot be discovered"),
            );
            row_divider(ui, t);

            settings_label(ui, "relay address", Some("optional"), t);
            ui.add(
                egui::TextEdit::singleline(&mut state.network_relay)
                    .frame(field_frame(t))
                    .desired_width(320.0)
                    .hint_text("/ip4/<relay>/tcp/4001/p2p/<peer id>"),
            );
            ui.label(
                RichText::new(
                    "the daemon can reserve a circuit on a relay. ovvc does not run a relay service, so leave this empty unless you have one.",
                )
                .size(9.0)
                .color(t.faint),
            );
            row_divider(ui, t);

            settings_label(ui, "invite policy", None, t);
            settings_value(ui, "bearer code, anyone holding it can join", t);
            row_divider(ui, t);
        }
        WizardStep::Review => {
            let theme_name = state
                .wizard_theme_index
                .and_then(|index| theme::Preset::all().get(index))
                .map(|preset| preset.label().to_string())
                .unwrap_or_else(|| "(not selected)".to_string());
            let name = if state.wizard_name.trim().is_empty() {
                "(not set)".to_string()
            } else {
                state.wizard_name.trim().to_string()
            };
            let commune = match state.wizard_commune {
                WizardCommune::Create => {
                    let name = state.wizard_commune_name.trim();
                    if name.is_empty() {
                        "(no commune name)".to_string()
                    } else {
                        format!("create {name}")
                    }
                }
                WizardCommune::Join => {
                    if state.wizard_join_code.trim().is_empty() {
                        "(no invite pasted)".to_string()
                    } else {
                        "join a friend's commune".to_string()
                    }
                }
            };
            for (label, value) in [
                ("identity", name),
                ("commune", commune),
                ("avatar", "initials".to_string()),
                ("theme", theme_name),
                (
                    "voice",
                    "default microphone, default speakers, ptt off".to_string(),
                ),
                (
                    "network",
                    format!(
                        "listen {}, relay {}",
                        state.network_listen,
                        if state.network_relay.trim().is_empty() {
                            "none"
                        } else {
                            "set"
                        }
                    ),
                ),
            ] {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("{label:<10}"))
                            .size(11.0)
                            .color(t.muted),
                    );
                    ui.label(RichText::new(value).size(11.0).color(t.text));
                });
            }
            row_divider(ui, t);
            ui.label(
                RichText::new(
                    "the review lists every choice, including the ones left off, so nothing is hidden.",
                )
                .size(9.0)
                .color(t.faint),
            );
            ui.label(
                RichText::new(
                    "finish is enabled when the identity and commune steps are complete.",
                )
                .size(9.0)
                .color(t.faint),
            );
        }
    }
}

fn draw_setup_page(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    let Some(step) = state.wizard_step else {
        return;
    };
    if ui.ctx().input(|input| input.key_pressed(egui::Key::Escape)) {
        state.wizard_step = None;
        state.screen = ScreenView::Chat;
        actions.push(Action::CancelSetup);
        return;
    }

    let mut close = false;
    egui::Panel::top("setup-header")
        .exact_size(34.0)
        .frame(
            egui::Frame::new()
                .fill(t.panel)
                .stroke(Stroke::new(1.0, t.border))
                .inner_margin(egui::Margin::symmetric(10, 5)),
        )
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.label(RichText::new("setup").size(14.0).strong().color(t.bright));
                ui.label(
                    RichText::new(format!(
                        "{} / {}",
                        step.index(),
                        WizardStep::ALL.len()
                    ))
                    .size(11.0)
                    .color(t.muted),
                );
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("[x] cancel").size(11.0).color(t.muted),
                            )
                            .frame(false),
                        )
                        .clicked()
                    {
                        close = true;
                    }
                });
            });
        });
    if close {
        state.wizard_step = None;
        state.screen = ScreenView::Chat;
        actions.push(Action::CancelSetup);
        return;
    }

    egui::Panel::bottom("setup-footer")
        .exact_size(44.0)
        .frame(
            egui::Frame::new()
                .fill(t.panel)
                .stroke(Stroke::new(1.0, t.border))
                .inner_margin(egui::Margin::symmetric(10, 6)),
        )
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                let back_enabled = step.index() > 1;
                if ui
                    .add_enabled(back_enabled, egui::Button::new("[ back ]"))
                    .clicked()
                {
                    state.wizard_step = Some(step.previous());
                }
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if step == WizardStep::Review {
                        let can_finish = state.wizard_step_valid(WizardStep::Identity)
                            && state.wizard_step_valid(WizardStep::Commune);
                        if ui
                            .add_enabled(can_finish, egui::Button::new("[ finish ]"))
                            .clicked()
                        {
                            if let Some(index) = state.wizard_theme_index
                                && let Some(preset) = theme::Preset::all().get(index).copied()
                            {
                                theme::apply(ui.ctx(), preset);
                                if let Err(error) = theme::save_preset(preset) {
                                    state.error =
                                        Some(format!("could not save the theme: {error:#}"));
                                }
                            }
                            actions.push(Action::FinishWizard);
                        }
                    } else {
                        let can_next = state.wizard_step_valid(step);
                        if ui
                            .add_enabled(can_next, egui::Button::new("[ next ]"))
                            .clicked()
                        {
                            state.wizard_step = Some(step.next());
                        }
                    }
                });
            });
        });

    egui::Panel::left("setup-steps")
        .exact_size(260.0)
        .frame(
            egui::Frame::new()
                .fill(t.panel)
                .stroke(Stroke::new(1.0, t.border))
                .inner_margin(egui::Margin::symmetric(10, 8)),
        )
        .show(ui, |ui| {
            ui.label(
                RichText::new("nothing is assumed")
                    .size(10.0)
                    .color(t.faint),
            );
            ui.add_space(6.0);
            for candidate in WizardStep::ALL {
                let marker = if candidate == step {
                    "[>]"
                } else if candidate.index() < step.index() {
                    "[x]"
                } else {
                    "[ ]"
                };
                let color = if candidate == step {
                    t.bright
                } else if candidate.index() < step.index() {
                    t.accent
                } else {
                    t.faint
                };
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(format!(
                                "{marker} {}  {}",
                                candidate.index(),
                                candidate.label()
                            ))
                            .size(12.0)
                            .color(color),
                        )
                        .frame(false),
                    )
                    .clicked()
                {
                    state.wizard_step = Some(candidate);
                }
            }
            ui.add_space(6.0);
            ui.label(
                RichText::new("steps select by keyboard or mouse\nnext validates the current step")
                    .size(9.0)
                    .color(t.faint),
            );
        });

    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(t.chat)
                .stroke(Stroke::new(1.0, t.border))
                .inner_margin(egui::Margin::symmetric(16, 10)),
        )
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    setup_step_content(ui, state, actions, t);
                });
        });
}

fn map_node(
    painter: &egui::Painter,
    rect: egui::Rect,
    title: &str,
    subtitle: &str,
    accent: Color32,
    fill: Color32,
    border: Color32,
) {
    painter.rect_filled(rect, egui::CornerRadius::ZERO, fill);
    painter.rect_stroke(
        rect,
        egui::CornerRadius::ZERO,
        Stroke::new(1.0, border),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.center_top() + egui::vec2(0.0, 7.0),
        Align2::CENTER_TOP,
        title,
        FontId::monospace(11.0),
        accent,
    );
    if !subtitle.is_empty() {
        painter.text(
            rect.center_bottom() - egui::vec2(0.0, 7.0),
            Align2::CENTER_BOTTOM,
            subtitle,
            FontId::monospace(9.0),
            border,
        );
    }
}

fn draw_network_map_page(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    if ui.ctx().input(|input| input.key_pressed(egui::Key::Escape)) {
        state.screen = ScreenView::Settings;
        return;
    }
    if page_header(
        ui,
        "network-map-header",
        "network map: swarms and peers",
        "[x]",
        t,
    ) {
        state.screen = ScreenView::Settings;
        return;
    }

    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(t.bg)
                .stroke(Stroke::new(1.0, t.border))
                .inner_margin(egui::Margin::symmetric(12, 8)),
        )
        .show(ui, |ui| {
            let (canvas_area, panel_area) = {
                let width = ui.available_width();
                let panel_width = 230.0_f32.min(width * 0.36);
                let canvas = egui::Rect::from_min_size(
                    ui.cursor().min,
                    egui::vec2(width - panel_width - 12.0, ui.available_height()),
                );
                let panel = egui::Rect::from_min_size(
                    egui::pos2(canvas.right() + 12.0, canvas.top()),
                    egui::vec2(panel_width, canvas.height()),
                );
                (canvas, panel)
            };

            let (canvas, _) = ui.allocate_exact_size(canvas_area.size(), Sense::hover());
            let painter = ui.painter().with_clip_rect(canvas);
            let online = state
                .selected_guild
                .is_some_and(|guild| state.is_online(guild));
            let name = state
                .identity
                .map(|author| state.display_name(author))
                .unwrap_or_else(|| "anonymous".to_string());
            let epoch = e2ee_epoch(state);

            let center_size = egui::vec2(160.0, 64.0);
            let center_rect = egui::Rect::from_center_size(
                canvas.center() - egui::vec2(0.0, 10.0),
                center_size,
            );
            map_node(
                &painter,
                center_rect,
                &format!("you / {name}"),
                &format!(
                    "this device · {} · {}",
                    if online { "direct" } else { "offline" },
                    epoch
                        .map(|epoch| format!("e2ee {epoch}"))
                        .unwrap_or_else(|| "e2ee -".to_string())
                ),
                t.accent,
                t.panel,
                t.accent,
            );

            let mut swarm_nodes: Vec<(String, String, egui::Rect)> = Vec::new();
            let swarms: Vec<_> = state
                .guilds
                .iter()
                .filter(|guild| !state.is_direct_guild(guild.id))
                .take(4)
                .collect();
            let positions = [
                egui::pos2(canvas.left() + canvas.width() * 0.17, canvas.top() + canvas.height() * 0.16),
                egui::pos2(canvas.left() + canvas.width() * 0.80, canvas.top() + canvas.height() * 0.16),
                egui::pos2(canvas.left() + canvas.width() * 0.17, canvas.top() + canvas.height() * 0.72),
                egui::pos2(canvas.left() + canvas.width() * 0.80, canvas.top() + canvas.height() * 0.72),
            ];
            for (index, guild) in swarms.iter().enumerate() {
                let rect = egui::Rect::from_center_size(positions[index], egui::vec2(180.0, 64.0));
                let selected = state.selected_guild == Some(guild.id);
                let peers = if selected { state.members.len() } else { 0 };
                let subtitle = if selected {
                    format!(
                        "{peers} peers · {}",
                        if state.is_online(guild.id) {
                            "direct"
                        } else {
                            "offline"
                        }
                    )
                } else {
                    "direct".to_string()
                };
                map_node(
                    &painter,
                    rect,
                    &format!("ring · {}", guild.name),
                    &subtitle,
                    if selected { t.accent } else { t.text },
                    t.panel,
                    if selected { t.accent } else { t.border },
                );
                painter.line_segment(
                    [center_rect.center(), rect.center()],
                    Stroke::new(1.0, t.accent_dim),
                );
                swarm_nodes.push((guild.name.clone(), subtitle, rect));
            }

            for (index, conversation) in state.direct_messages.iter().take(1).enumerate() {
                let rect = egui::Rect::from_center_size(
                    positions[(index + 2).min(3)],
                    egui::vec2(180.0, 64.0),
                );
                let label = format!("dm: {}", state.dm_name(conversation));
                map_node(
                    &painter,
                    rect,
                    &label,
                    "direct · e2ee",
                    t.cyan,
                    t.panel,
                    t.border,
                );
                painter.line_segment(
                    [center_rect.center(), rect.center()],
                    Stroke::new(1.0, t.accent_dim),
                );
                swarm_nodes.push((label, "direct · e2ee".to_string(), rect));
            }

            if let Some((_, _, swarm_rect)) = swarm_nodes.first() {
                let members: Vec<_> = state
                    .members
                    .iter()
                    .filter(|member| Some(member.author) != state.identity)
                    .take(3)
                    .collect();
                for (index, member) in members.iter().enumerate() {
                    let offset = egui::vec2(
                        -40.0 + index as f32 * 52.0,
                        -52.0 - index as f32 * 8.0,
                    );
                    let rect = egui::Rect::from_center_size(
                        swarm_rect.center() + offset,
                        egui::vec2(96.0, 40.0),
                    );
                    map_node(
                        &painter,
                        rect,
                        &member.display_name,
                        "peer",
                        t.text,
                        t.panel,
                        t.border,
                    );
                    painter.line_segment(
                        [swarm_rect.center(), rect.center()],
                        Stroke::new(1.0, t.border),
                    );
                }
            }

            let legend = [
                ("ring: swarm", t.muted),
                ("dot: peer", t.muted),
                ("solid: direct", t.green),
                ("you: this device", t.accent),
            ];
            let mut y = canvas.bottom() - 12.0 - legend.len() as f32 * 13.0;
            for (text, color) in legend {
                painter.text(
                    egui::pos2(canvas.left() + 4.0, y),
                    Align2::LEFT_TOP,
                    text,
                    FontId::monospace(9.0),
                    color,
                );
                y += 13.0;
            }

            let mut panel_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(panel_area)
                    .layout(egui::Layout::top_down(Align::Min)),
            );
            panel_ui.set_width(panel_area.width());
            panel_ui.label(
                RichText::new("connection")
                    .size(12.0)
                    .strong()
                    .color(t.bright),
            );
            panel_ui.label(
                RichText::new(if online { "net:online" } else { "net:offline" })
                    .size(11.0)
                    .color(if online { t.green } else { t.red }),
            );
            panel_ui.label(
                RichText::new(format!("{} peers", state.online_peers()))
                    .size(11.0)
                    .color(t.muted),
            );
            panel_ui.label(
                RichText::new(match &state.connection {
                    Some(connection) if connection.connected => "direct",
                    _ => "no link",
                })
                .size(11.0)
                .color(t.muted),
            );
            panel_ui.label(
                RichText::new(
                    epoch
                        .map(|epoch| format!("e2ee epoch {epoch}"))
                        .unwrap_or_else(|| "e2ee epoch -".to_string()),
                )
                .size(11.0)
                .color(t.muted),
            );
            if panel_ui.button("[ refresh ]").clicked() {
                actions.push(Action::Refresh);
            }
            panel_ui.add_space(6.0);
            section_header(&mut panel_ui, "swarms", t);
            for (title, subtitle, _) in &swarm_nodes {
                panel_ui.label(RichText::new(title).size(11.0).color(t.text));
                panel_ui.label(RichText::new(subtitle).size(9.0).color(t.faint));
            }
            if state.detail_level == DetailLevel::Expert {
                panel_ui.add_space(6.0);
                section_header(&mut panel_ui, "this device", t);
                if let Some(connection) = &state.connection {
                    if let Some(peer) = &connection.peer_id {
                        panel_ui.label(
                            RichText::new(format!("id {peer}"))
                                .size(9.0)
                                .color(t.muted),
                        );
                    }
                    for address in &connection.listen_addresses {
                        panel_ui.label(RichText::new(address).size(9.0).color(t.muted));
                    }
                } else {
                    panel_ui.label(
                        RichText::new("no connection snapshot")
                            .size(9.0)
                            .color(t.faint),
                    );
                }
            }
            panel_ui.add_space(6.0);
            panel_ui.label(
                RichText::new(
                    "the circle is the readable form of the same data as the TUI tree. solid links are direct.",
                )
                .size(9.0)
                .color(t.faint),
            );
        });
}

fn draw_diagnostics_page(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut Vec<Action>,
    t: &theme::Palette,
) {
    if ui.ctx().input(|input| input.key_pressed(egui::Key::Escape)) {
        state.diagnostics_confirm_shutdown = false;
        state.screen = ScreenView::Settings;
        return;
    }
    if page_header(ui, "diagnostics-header", "diagnostics", "[x] close", t) {
        state.diagnostics_confirm_shutdown = false;
        state.screen = ScreenView::Settings;
        return;
    }

    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(t.chat)
                .stroke(Stroke::new(1.0, t.border))
                .inner_margin(egui::Margin::symmetric(12, 8)),
        )
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("diagnostics stay out of the main chat")
                            .size(10.0)
                            .color(t.muted),
                    );
                    ui.label(
                        RichText::new(
                            "the alert strip keeps one line; the log path, classes and detail live here. expert settings link to this screen.",
                        )
                        .size(9.0)
                        .color(t.faint),
                    );
                    ui.add_space(6.0);
                    section_header(ui, "error classes", t);
                    for (label, colour, text) in [
                        ("! bug", t.red, "a failure that names the daemon log"),
                        ("! warning", t.yellow, "an operational condition, not a failure banner"),
                        ("! security", t.magenta, "a security event"),
                        ("! policy", t.cyan, "a policy view or refusal"),
                        ("! error", t.bright, "a user-facing failure"),
                    ] {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(label).size(10.0).strong().color(colour));
                            ui.label(RichText::new(text).size(10.0).color(t.muted));
                        });
                    }
                    ui.add_space(6.0);
                    section_header(ui, "current alert", t);
                    match (&state.error, &state.notice) {
                        (Some(error), _) => {
                            let (kind, colour) =
                                alert_style(state.error_class, t);
                            ui.label(
                                RichText::new(format!("{kind}  {error}"))
                                    .size(10.0)
                                    .color(colour),
                            );
                            if let Some(log) = &state.error_log {
                                ui.label(
                                    RichText::new(format!("log {log}"))
                                        .size(9.0)
                                        .color(t.muted),
                                );
                            }
                        }
                        (None, Some(notice)) => {
                            ui.label(
                                RichText::new(format!("notice  {notice}"))
                                    .size(10.0)
                                    .color(t.accent),
                            );
                        }
                        (None, None) => {
                            ui.label(
                                RichText::new("-- no current alert --")
                                    .size(10.0)
                                    .color(t.faint),
                            );
                        }
                    }
                    ui.add_space(6.0);
                    section_header(ui, "daemon", t);
                    ui.label(
                        RichText::new(format!(
                            "version   {}",
                            state.daemon_version.as_deref().unwrap_or("unknown")
                        ))
                        .size(10.0)
                        .color(t.text),
                    );
                    if let Some(status) = &state.status {
                        ui.label(
                            RichText::new(format!("data      {}", status.data_dir))
                                .size(10.0)
                                .color(t.muted),
                        );
                        ui.label(
                            RichText::new(format!("socket    {}", status.socket))
                                .size(10.0)
                                .color(t.muted),
                        );
                    }
                    if let Some(log) = &state.error_log {
                        ui.label(
                            RichText::new(format!("log       {log}"))
                                .size(10.0)
                                .color(t.muted),
                        );
                    } else {
                        ui.label(
                            RichText::new("log       (no bug has named a path this session)")
                                .size(10.0)
                                .color(t.faint),
                        );
                    }
                    ui.add_space(6.0);
                    section_header(ui, "network", t);
                    let online = state
                        .selected_guild
                        .is_some_and(|guild| state.is_online(guild));
                    ui.label(
                        RichText::new(if online { "net:online" } else { "net:offline" })
                            .size(10.0)
                            .color(if online { t.green } else { t.red }),
                    );
                    ui.label(
                        RichText::new(format!("{} peers", state.online_peers()))
                            .size(10.0)
                            .color(t.muted),
                    );
                    if let Some(connection) = &state.connection {
                        for address in &connection.listen_addresses {
                            ui.label(RichText::new(address).size(9.0).color(t.muted));
                        }
                        for peer in &connection.peers {
                            ui.label(
                                RichText::new(format!(
                                    "{} {}",
                                    if peer.connected { "*" } else { "-" },
                                    peer.peer
                                ))
                                .size(9.0)
                                .color(t.muted),
                            );
                        }
                    }
                    ui.label(
                        RichText::new(
                            epoch_line(state),
                        )
                        .size(10.0)
                        .color(t.muted),
                    );
                    ui.add_space(6.0);
                    section_header(ui, "policy (read-only)", t);
                    match &state.policy {
                        None => {
                            ui.label(
                                RichText::new("not loaded yet")
                                    .size(10.0)
                                    .color(t.faint),
                            );
                        }
                        Some(view) => {
                            ui.label(
                                RichText::new(format!(
                                    "guild {} / {}",
                                    view.guild.profile.name(),
                                    view.guild.topology.name()
                                ))
                                .size(10.0)
                                .color(t.text),
                            );
                            for (axis, guild, client, effective, raised) in policy_rows(view) {
                                ui.label(
                                    RichText::new(format!(
                                        "{axis:<16} guild {guild:<14} floor {client:<14} effective {effective}{}",
                                        if raised { " *" } else { "" }
                                    ))
                                    .size(9.0)
                                    .color(if raised { t.yellow } else { t.muted }),
                                );
                            }
                            ui.label(
                                RichText::new("this is a read-only view")
                                    .size(9.0)
                                    .color(t.faint),
                            );
                        }
                    }
                    ui.add_space(6.0);
                    section_header(ui, "actions", t);
                    ui.horizontal_wrapped(|ui| {
                        if let Some(log) = &state.error_log
                            && ui.button("[ copy log path ]").clicked()
                        {
                            ui.ctx().copy_text(log.clone());
                            state.notice = Some("log path copied".to_string());
                        }
                        if ui.button("[ refresh ]").clicked() {
                            actions.push(Action::Refresh);
                        }
                        if state.diagnostics_confirm_shutdown {
                            if ui.button("[ confirm shutdown ]").clicked() {
                                state.diagnostics_confirm_shutdown = false;
                                actions.push(Action::ShutdownDaemon);
                            }
                            if ui.button("[ cancel ]").clicked() {
                                state.diagnostics_confirm_shutdown = false;
                            }
                        } else if ui.button("[ shutdown daemon ]").clicked() {
                            state.diagnostics_confirm_shutdown = true;
                        }
                    });
                    ui.label(
                        RichText::new(
                            "diagnostics is the only place for the log path and the policy read-model note. the main chat stays clean.",
                        )
                        .size(9.0)
                        .color(t.faint),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("TUI / diagnostics: /status /config /policy, same data, text only")
                            .size(9.0)
                            .color(t.faint),
                    );
                });
        });
}

fn epoch_line(state: &AppState) -> String {
    e2ee_epoch(state)
        .map(|epoch| format!("e2ee epoch {epoch}"))
        .unwrap_or_else(|| "e2ee epoch -".to_string())
}

// --- helpers ---------------------------------------------------------------

fn elide(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push_str("...");
    out
}

/// The bundled monospace faces have no emoji glyphs, so the common reactions
/// fall back to their IRC shortcode. Exotic emoji still render as tofu, which
/// is no worse than any other terminal.
fn reaction_label(emoji: &str) -> String {
    match emoji {
        "\u{1f44d}" => "+1".to_string(),
        "\u{1f44e}" => "-1".to_string(),
        "\u{1f602}" => ":joy:".to_string(),
        "\u{1f389}" => ":tada:".to_string(),
        "\u{2764}\u{fe0f}" | "\u{2764}" => "<3".to_string(),
        other => other.to_string(),
    }
}

fn format_time(timestamp: u64) -> String {
    match time::OffsetDateTime::from_unix_timestamp(timestamp as i64) {
        Ok(time) => {
            let clock = time.time();
            format!("{:02}:{:02}", clock.hour(), clock.minute())
        }
        Err(_) => "--:--".to_string(),
    }
}

/// The date part of the transcript's day rules. UTC on purpose: every peer
/// sees the same rule for the same signed timestamp.
fn format_date(timestamp: u64) -> String {
    match time::OffsetDateTime::from_unix_timestamp(timestamp as i64) {
        Ok(time) => format!(
            "{:04}-{:02}-{:02}",
            time.year(),
            u8::from(time.month()),
            time.day()
        ),
        Err(_) => "---- -- --".to_string(),
    }
}

fn day_rule(ui: &mut egui::Ui, day: &str, t: &theme::Palette) {
    ui.add_space(5.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let side = ((ui.available_width() - 96.0) / 2.0).max(6.0);
        let (left, _) = ui.allocate_exact_size(egui::vec2(side, 1.0), Sense::hover());
        ui.painter()
            .hline(left.x_range(), left.center().y, Stroke::new(1.0, t.border));
        ui.label(RichText::new(day).size(t.small).color(t.faint));
        let (right, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), Sense::hover());
        if right.width() > 2.0 {
            ui.painter().hline(
                right.x_range(),
                right.center().y,
                Stroke::new(1.0, t.border),
            );
        }
    });
    ui.add_space(4.0);
}

fn new_messages_rule(ui: &mut egui::Ui, t: &theme::Palette) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(RichText::new("new messages").size(t.small).color(t.yellow));
        let (line, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), Sense::hover());
        if line.width() > 2.0 {
            ui.painter()
                .hline(line.x_range(), line.center().y, Stroke::new(1.0, t.yellow));
        }
    });
    ui.add_space(4.0);
}

/// Present so tests can build a minimal state and a caller can see the full
/// palette in one place. The UI itself uses the installed palette.
pub fn author_color(author: AuthorId) -> Color32 {
    theme::Palette::default().author_color(author.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(transport: &str, rtt_ms: u32, packets_lost: i64, relayed: bool) -> VoiceAudioStats {
        VoiceAudioStats {
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
            transport: transport.to_string(),
            rtt_ms,
            packets_lost,
            relayed,
            e2ee: true,
            epoch: 1,
            e2ee_errors: 0,
        }
    }

    #[test]
    fn alert_styles_separate_a_bug_from_an_operational_warning() {
        let palette = theme::Palette::default();
        let (bug_label, bug_colour) = alert_style(Some(ErrorClass::Bug), &palette);
        assert_eq!(bug_label, "! bug");
        assert_eq!(bug_colour, palette.red);

        let (warning_label, warning_colour) =
            alert_style(Some(ErrorClass::Operational), &palette);
        assert_eq!(warning_label, "! warning");
        assert_eq!(warning_colour, palette.yellow);
        assert_ne!(bug_label, warning_label);

        // Security and user conditions are distinct from both.
        assert_eq!(
            alert_style(Some(ErrorClass::Security), &palette).0,
            "! security"
        );
        assert_eq!(alert_style(Some(ErrorClass::User), &palette).0, "! error");

        assert_eq!(
            alert_message(
                "an impossible state",
                Some(ErrorClass::Bug),
                Some("/tmp/ovvc/daemon.log")
            ),
            "an impossible state (log: /tmp/ovvc/daemon.log)"
        );
        assert_eq!(
            alert_message(
                "peer 3f2a went offline",
                Some(ErrorClass::Operational),
                Some("/tmp/ovvc/daemon.log")
            ),
            "peer 3f2a went offline",
            "only a bug names the log"
        );
    }

    #[test]
    fn the_voice_link_line_shows_transport_quality_and_relay_state() {
        assert_eq!(
            voice_link_line(&stats("webrtc", 35, 12, false)),
            "link: webrtc  rtt=35ms  lost=12  direct"
        );
        assert_eq!(
            voice_link_line(&stats("webrtc", 80, 3, true)),
            "link: webrtc  rtt=80ms  lost=3  relayed"
        );
    }

    #[test]
    fn the_libp2p_fallback_marks_rtp_counters_and_relay_state_unavailable() {
        assert_eq!(
            voice_link_line(&stats("libp2p", 0, 0, false)),
            "link: libp2p  rtt=-  lost=-  relay=unknown"
        );
    }

    #[test]
    fn the_voice_e2ee_line_shows_the_epoch_and_names_a_missing_key() {
        let audio = stats("webrtc", 35, 0, false);
        assert_eq!(voice_e2ee_line(&audio), "e2ee: active  epoch=1");
        assert_eq!(voice_e2ee_error(&audio), None);

        // No keyring, or a peer key that is missing, means no media at all.
        let missing = VoiceAudioStats {
            e2ee: false,
            epoch: 0,
            ..stats("webrtc", 35, 0, false)
        };
        let line = voice_e2ee_line(&missing);
        assert!(line.contains("no media key"), "{line}");
        assert!(line.contains("epoch=0"), "{line}");

        // A keyring exists but one peer has not published a key for this
        // epoch, so the panel names the peer instead of the whole call.
        let peer_missing = VoiceAudioStats {
            e2ee: false,
            epoch: 2,
            ..stats("webrtc", 35, 0, false)
        };
        let line = voice_e2ee_line(&peer_missing);
        assert!(line.contains("a peer has no usable media key"), "{line}");
        assert!(line.contains("epoch=2"), "{line}");

        let failing = VoiceAudioStats {
            e2ee_errors: 3,
            ..audio
        };
        assert_eq!(
            voice_e2ee_error(&failing).as_deref(),
            Some(
                "! e2ee: 3 media frame(s) dropped for a missing key, wrong epoch, replay or failed tag"
            )
        );
    }

    #[test]
    fn a_star_topology_names_the_media_path_and_key_rule() {
        let root = AuthorId([1; 32]);
        let peer = AuthorId([2; 32]);
        let star = ovvc_control::CallTopology::Star {
            hub: root,
            leaves: vec![peer],
        };
        assert_eq!(voice_topology(&ovvc_control::CallTopology::Mesh), "mesh");
        assert_eq!(voice_topology(&star), "star");
        assert_eq!(
            voice_media_line(&star),
            "media: star  1 leaf  keys: leaf->hub",
            "the panel must report the star media path, not a pending mesh"
        );
        assert!(!voice_media_line(&star).contains("forwarding pending"));
        assert!(!voice_media_line(&star).contains("keys: all-pairs"));
        assert_eq!(
            voice_media_line(&ovvc_control::CallTopology::Mesh),
            "media: mesh  keys: all-pairs"
        );
    }
}
