//! Phosphor terminal rendering for the TUI.
//!
//! This is the same layout language as the GUI: a top bar, guild tabs, a
//! channel sidebar, a member panel, a status bar, square hairline boxes,
//! bracketed controls and reverse-video selections. The palette comes from
//! `ovvc-theme`, so `theme.toml` and `OVVC_THEME` restyle both frontends.
//! Markdown, dates, presence glyphs, attachment chips and unread rules are
//! shared with the GUI as well, which keeps the two transcripts agreeing about
//! the same signed event.

use ovvc_core::{ErrorClass, markdown};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::{App, DetailLevel, Focus, Overlay, Screen, SettingsTab, Step};
use crate::history::{anchored_scroll_back, badge};
use crate::theme::{self, Palette};

const CLOCK: &[time::format_description::FormatItem<'static>] =
    time::macros::format_description!("[hour]:[minute]");

fn surface(color: Color) -> Style {
    Style::new().bg(color)
}

fn muted(t: &Palette) -> Style {
    Style::new().fg(t.muted)
}

fn faint(t: &Palette) -> Style {
    Style::new().fg(t.faint)
}

/// Draw a frame.
///
/// `app` is mutable because the draw pass is the only place that knows how the
/// transcript wrapped at the current width, so it is where a pending
/// jump-to-message request is turned into a scroll offset.
pub fn draw(frame: &mut Frame, app: &mut App) {
    match app.screen {
        Screen::Onboarding => draw_onboarding(frame, app),
        Screen::Chat => draw_chat(frame, app),
        Screen::Settings => draw_settings(frame, app),
        Screen::Setup => draw_setup(frame, app),
        Screen::NetworkMap => draw_network_map(frame, app),
        Screen::Diagnostics => draw_diagnostics(frame, app),
    }

    if matches!(app.screen, Screen::Onboarding | Screen::Chat)
        && let Some(overlay) = &app.overlay
    {
        draw_overlay(frame, overlay);
    }
}

// --- chat ---------------------------------------------------------------

fn draw_chat(frame: &mut Frame, app: &mut App) {
    let t = theme::palette();
    // The error strip and the call line each take a row while they have
    // something to say. One row for the top border and one for the general
    // status line are always present.
    let status_height =
        1 + 1 + u16::from(app.error.is_some()) + u16::from(app.voice_status_line().is_some());
    let [top, tabs, body, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(8),
        Constraint::Length(status_height),
    ])
    .areas(frame.area());

    draw_top_bar(frame, app, top, t);
    draw_guild_tabs(frame, app, tabs, t);

    let show_members = app.show_members;
    let columns = Layout::horizontal([
        Constraint::Length(26),
        Constraint::Min(30),
        Constraint::Length(if show_members { 22 } else { 4 }),
    ])
    .split(body);

    draw_channel_sidebar(frame, app, columns[0], t);
    if show_members {
        draw_members(frame, app, columns[2], t);
    } else {
        draw_members_collapsed(frame, app, columns[2], t);
    }

    let typing = app
        .current_channel()
        .map(|channel| app.typing_in(channel.id))
        .unwrap_or_default();
    let composer_height = 4 + u16::from(app.reply.is_some()) + u16::from(!typing.is_empty());
    let [header, header_rule, messages, composer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(composer_height),
    ])
    .areas(columns[1]);

    draw_channel_header(frame, app, header, t);
    draw_hairline(frame, header_rule, t);
    draw_messages(frame, app, messages);
    draw_composer(frame, app, composer, t, &typing);

    draw_status(frame, app, status, t);
}

fn draw_top_bar(frame: &mut Frame, _app: &App, area: Rect, t: &Palette) {
    // A healthy daemon needs no badge. Only an unavailable one is named, so
    // the bar does not spend space stating that things are working.
    // The brand keeps its `//` at every connection state. `[members]` and
    // `[config]` moved to the member rail as `[M]` and `[T]`, and a daemon
    // problem is named once in the status bar.
    let left = Line::from(vec![
        Span::styled(
            " ovvc",
            Style::new().fg(t.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" // ", Style::new().fg(t.faint)),
    ]);
    frame.render_widget(Paragraph::new(left).style(surface(t.panel)), area);
}

fn draw_guild_tabs(frame: &mut Frame, app: &App, area: Rect, t: &Palette) {
    let [content, rule] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    let mut spans = vec![Span::raw(" ")];
    let communes = app.commune_indices();
    for (position, index) in communes.iter().enumerate() {
        let guild = &app.guilds[*index];
        let unread = app.guild_unread(guild.id);
        let mut label = format!("[{}] {}", position + 1, guild.name);
        if unread > 0 {
            label.push_str(&format!(" ({unread})"));
        }
        let style = if *index == app.guild_index {
            Style::new().fg(t.bright).bg(t.accent_dim)
        } else if unread > 0 {
            Style::new().fg(t.bright)
        } else {
            Style::new().fg(t.text)
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
    }
    if communes.is_empty() {
        spans.push(Span::styled("-- no communes --", faint(t)));
    }
    spans.push(Span::styled("[+]", muted(t)));
    spans.push(Span::raw(" "));
    spans.push(Span::styled("[join]", muted(t)));

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(surface(t.bg)),
        content,
    );
    frame.render_widget(
        Paragraph::new("─".repeat(rule.width as usize)).style(Style::new().fg(t.border).bg(t.bg)),
        rule,
    );
}

fn draw_channel_sidebar(frame: &mut Frame, app: &App, area: Rect, t: &Palette) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::new().fg(t.border))
        .style(surface(t.panel));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let content = inner;
    let width = content.width as usize;

    let direct = app.current_direct();
    let guild_name = app
        .current_guild()
        .map(|guild| guild.name.to_lowercase())
        .unwrap_or_else(|| "commune".to_string());
    let title = match &direct {
        Some(name) => format!(" -- dm:{} --", name.to_lowercase()),
        None => format!(" -- {guild_name} --"),
    };
    let mut lines = vec![Line::from(Span::styled(title, Style::new().fg(t.muted)))];
    if direct.is_some() {
        lines.push(Line::from(Span::styled(
            " direct conversation",
            Style::new().fg(t.faint),
        )));
    }
    // Invite and join no longer sit under the commune name. The guild tab and
    // the dialogs keep both actions.
    lines.push(Line::raw(""));
    lines.push(section_header(width, "channels", t));

    if app.channels.is_empty() {
        lines.push(Line::from(Span::styled(" -- no channels --", faint(t))));
    }
    let mut parent = None;
    for (index, channel) in app.channels.iter().enumerate() {
        if channel.parent != parent {
            parent = channel.parent;
            if let Some(parent) = parent {
                let name = app
                    .categories
                    .iter()
                    .find(|category| category.id == parent)
                    .map(|category| category.name.to_lowercase())
                    .unwrap_or_else(|| "category".to_string());
                lines.push(Line::from(Span::styled(
                    format!("  -- {name} --"),
                    Style::new().fg(t.muted),
                )));
            }
        }
        let selected = index == app.channel_index;
        let unread = app.unread.get(&channel.id).copied().unwrap_or(0);
        let muted = app.notifications.muted_channels.contains(&channel.id);
        let base = if selected {
            Style::new().fg(t.select_fg).bg(t.select_bg)
        } else if unread > 0 {
            Style::new().fg(t.bright)
        } else if muted {
            Style::new().fg(t.faint)
        } else {
            Style::new().fg(t.muted)
        };
        let badge_style = if selected {
            base
        } else {
            Style::new().fg(t.yellow)
        };
        let glyph = channel.kind.glyph();
        let indent = if channel.parent.is_some() { "   " } else { " " };
        let mut name = channel.name.clone();
        if muted {
            name.push_str(" [m]");
        }
        if channel.kind == ovvc_client::ChannelKind::Voice
            && let Some(count) = app.voice_sidebar_hint(channel.id)
        {
            name.push_str(&format!(" [{count}]"));
        }
        lines.push(row_line(
            format!("{indent}{glyph} {name}"),
            badge(unread),
            width,
            base,
            badge_style,
        ));
    }

    // Direct conversations keep their own group so they are never mistaken
    // for communes in the tab strip or the channel list above.
    lines.push(Line::raw(""));
    lines.push(section_header(width, "direct messages", t));
    if app.direct_messages.is_empty() {
        lines.push(Line::from(Span::styled(" -- none --", faint(t))));
    } else {
        for conversation in &app.direct_messages {
            let selected = app.current_guild().map(|guild| guild.id) == Some(conversation.guild.id);
            let unread = app.guild_unread(conversation.guild.id);
            let base = if selected {
                Style::new().fg(t.select_fg).bg(t.select_bg)
            } else if unread > 0 {
                Style::new().fg(t.bright)
            } else {
                Style::new().fg(t.muted)
            };
            let badge_style = if selected {
                base
            } else {
                Style::new().fg(t.yellow)
            };
            lines.push(row_line(
                format!(" dm:{}", conversation.peer.display_name),
                badge(unread),
                width,
                base,
                badge_style,
            ));
        }
    }

    frame.render_widget(Paragraph::new(lines).style(surface(t.panel)), content);
}

fn draw_members_collapsed(frame: &mut Frame, app: &App, area: Rect, t: &Palette) {
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::new().fg(t.border))
        .style(surface(t.panel));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled("[M]", muted(t))),
            Line::from(Span::styled(
                app.members.len().to_string(),
                faint(t),
            )),
        ])
        .style(surface(t.panel)),
        inner,
    );
}

/// The compact current-user card. It owns identity and self presence only.
fn current_user_card_lines(app: &App, width: usize, t: &Palette) -> Vec<Line<'static>> {
    let border = Style::new().fg(t.border);
    let inner = width.saturating_sub(2);
    let title = " current user ";
    let top = format!(
        "+{}{}+",
        title,
        "-".repeat(inner.saturating_sub(title.chars().count()))
    );
    let name = if app.profile.is_empty() {
        "anonymous".to_string()
    } else {
        app.profile.clone()
    };
    let initial = name
        .chars()
        .find(|character| character.is_alphanumeric())
        .map(|character| character.to_ascii_uppercase())
        .unwrap_or('?');
    let content_width = width.saturating_sub(4);
    let left = clamp(format!("[{initial}] {name}"), content_width.saturating_sub(4));
    let pad = content_width
        .saturating_sub(left.chars().count() + 3)
        .max(1);
    let mut lines = vec![
        Line::from(Span::styled(top, border)),
        Line::from(Span::styled(
            format!("| {left}{}[T] |", " ".repeat(pad)),
            Style::new().fg(t.text),
        )),
    ];
    let presence = if app.daemon_online {
        format!(
            "{} {}",
            theme::presence_glyph(app.own_presence),
            presence_name(app.own_presence)
        )
    } else {
        format!(
            "{} offline  ! daemon",
            theme::presence_glyph(ovvc_control::PresenceStatus::Offline)
        )
    };
    let color = if app.daemon_online {
        t.presence_color(app.own_presence)
    } else {
        t.red
    };
    let presence = clamp(presence, content_width);
    let pad = content_width.saturating_sub(presence.chars().count());
    lines.push(Line::from(vec![
        Span::styled(
            format!("| {presence}{} |", " ".repeat(pad)),
            Style::new().fg(color),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        format!("+{}+", "-".repeat(inner)),
        border,
    )));
    lines
}

fn strip_roles(markers: &str) -> String {
    markers
        .replace("[hub]", "")
        .replace("[master]", "")
        .replace("[backup]", "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The direct/relayed label for a media link. Only WebRTC measures whether a
/// candidate pair goes through TURN, so the libp2p fallback reports unknown
/// rather than claiming a direct path it never observed.
fn link_state(audio: &ovvc_control::VoiceAudioStats) -> &'static str {
    if audio.transport == "webrtc" {
        if audio.relayed { "relayed" } else { "direct" }
    } else {
        "relay=unknown"
    }
}

fn draw_members(frame: &mut Frame, app: &App, area: Rect, t: &Palette) {
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::new().fg(t.border))
        .style(surface(t.panel));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = inner.width as usize;

    // The current-user card sits at the top of the rail, where the sidebar's
    // bottom-left identity used to be.
    let mut lines = current_user_card_lines(app, width, t);

    // A selected voice channel with a live call shows the call roster instead
    // of the guild roster; selecting a text channel keeps the panel it had.
    if let Some(roster) = app.voice_roster() {
        lines.push(section_header(width, &format!("call {}", roster.capacity), t));
        if app.detail_level == DetailLevel::Expert {
            // One expert connection block owns topology, roles, audio, link
            // and E2EE. The roster rows show flags only.
            lines.push(Line::from(Span::styled(
                format!(" {}", roster.topology),
                faint(t),
            )));
            lines.push(Line::from(Span::styled(
                format!(" {}", roster.media),
                faint(t),
            )));
            lines.push(Line::from(Span::styled(
                format!(" {}", roster.hub),
                faint(t),
            )));
            if let Some(audio) = &app.voice.audio {
                lines.push(Line::from(Span::styled(
                    format!(
                        " audio: backend={} peers={} latency={}ms xruns={}",
                        audio.backend, audio.peers, audio.latency_ms, audio.xruns
                    ),
                    faint(t),
                )));
                lines.push(Line::from(Span::styled(
                    format!(
                        " link: {} rtt={}ms lost={} {}",
                        audio.transport,
                        audio.rtt_ms,
                        audio.packets_lost,
                        link_state(audio)
                    ),
                    faint(t),
                )));
                lines.push(Line::from(Span::styled(
                    format!(
                        " e2ee: {} epoch={} · trust signed",
                        if audio.e2ee && audio.epoch > 0 {
                            "active"
                        } else {
                            "none"
                        },
                        audio.epoch
                    ),
                    if audio.e2ee && audio.epoch > 0 {
                        Style::new().fg(t.green)
                    } else {
                        Style::new().fg(t.red)
                    },
                )));
            }
        } else {
            // Simple and advanced keep the link and a compact security state,
            // and hide the raw roles and audio counters.
            if let Some(audio) = &app.voice.audio {
                lines.push(Line::from(Span::styled(
                    format!(
                        " link: {} rtt={}ms lost={} {}",
                        audio.transport,
                        audio.rtt_ms,
                        audio.packets_lost,
                        link_state(audio)
                    ),
                    faint(t),
                )));
                lines.push(Line::from(Span::styled(
                    if audio.e2ee && audio.epoch > 0 {
                        " e2ee: active".to_string()
                    } else {
                        " ! e2ee: no media key".to_string()
                    },
                    if audio.e2ee && audio.epoch > 0 {
                        Style::new().fg(t.green)
                    } else {
                        Style::new().fg(t.red)
                    },
                )));
            }
        }
        if roster.rows.is_empty() {
            lines.push(Line::from(Span::styled(" -- empty --", faint(t))));
        }
        for row in &roster.rows {
            let markers = if app.detail_level == DetailLevel::Expert {
                row.markers.clone()
            } else {
                strip_roles(&row.markers)
            };
            let color = if row.speaking { t.bright } else { t.text };
            let mut spans = vec![Span::raw(" ")];
            if !markers.is_empty() {
                spans.push(Span::styled(
                    format!("{markers} "),
                    Style::new().fg(if row.speaking { t.bright } else { t.faint }),
                ));
            }
            spans.push(Span::styled(
                display_name(app, row.author),
                Style::new().fg(color),
            ));
            lines.push(Line::from(spans));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .style(surface(t.panel))
                .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }

    let header = if app.current_direct().is_some() {
        "direct message".to_string()
    } else {
        format!("members {}", app.members.len())
    };
    lines.push(Line::from(vec![
        Span::styled(" [M] ", muted(t)),
        Span::styled(header, muted(t)),
    ]));
    for member in &app.members {
        let status = app.presence_for(member.author);
        let color = if status == ovvc_control::PresenceStatus::Offline {
            t.faint
        } else {
            t.text
        };
        let name = member_name(app, member);
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                theme::presence_glyph(status),
                Style::new().fg(t.presence_color(status)),
            ),
            Span::raw(" "),
            Span::styled(name, Style::new().fg(color)),
        ]));
    }

    if app.detail_level == DetailLevel::Expert {
        lines.push(Line::raw(""));
        lines.push(section_header(width, "connection", t));
        let epoch = app
            .voice
            .audio
            .as_ref()
            .map(|audio| audio.epoch)
            .filter(|epoch| *epoch > 0)
            .map(|epoch| format!("key epoch {epoch}"))
            .unwrap_or_else(|| "key epoch -".to_string());
        lines.push(Line::from(Span::styled(
            format!(" trust signed · {epoch}"),
            faint(t),
        )));
        let transport = if app.online_guild.is_some() {
            "direct"
        } else {
            "-"
        };
        lines.push(Line::from(Span::styled(
            format!(" transport {transport}"),
            faint(t),
        )));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .style(surface(t.panel))
            .wrap(Wrap { trim: true }),
        inner,
    );
}

fn presence_name(status: ovvc_control::PresenceStatus) -> &'static str {
    match status {
        ovvc_control::PresenceStatus::Online => "online",
        ovvc_control::PresenceStatus::Away => "away",
        ovvc_control::PresenceStatus::DoNotDisturb => "dnd",
        ovvc_control::PresenceStatus::Offline => "offline",
    }
}

fn member_name(app: &App, member: &ovvc_client::MemberInfo) -> String {
    let mut name = if member.author == app.client.author_id() && !app.profile.is_empty() {
        app.profile.clone()
    } else {
        member.display_name.clone()
    };
    if member.author == app.client.author_id() {
        name.push_str(" (you)");
    }
    name
}

fn draw_channel_header(frame: &mut Frame, app: &App, area: Rect, t: &Palette) {
    let [left, right] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(18)]).areas(area);
    let mut spans = vec![Span::raw(" ")];
    if let Some(name) = app.current_direct() {
        spans.push(Span::styled(
            format!("dm:{name}"),
            Style::new().fg(t.bright).add_modifier(Modifier::BOLD),
        ));
        if let Some(channel) = app.current_channel() {
            spans.push(Span::styled(
                format!("  {} {}", channel.kind.glyph(), channel.name),
                muted(t),
            ));
        }
    } else if let Some(channel) = app.current_channel() {
        spans.push(Span::styled(
            format!("{} {}", channel.kind.glyph(), channel.name),
            Style::new().fg(t.bright).add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::styled("# welcome", faint(t)));
    }
    // The member list owns the participant count. The channel header does not
    // repeat it.
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(surface(t.chat)),
        left,
    );

    let unread = app
        .current_channel()
        .map(|channel| app.unread.get(&channel.id).copied().unwrap_or(0))
        .unwrap_or(0);
    let right_line = if unread > 0 {
        Line::from(vec![
            Span::styled(format!("[{unread} unread]"), Style::new().fg(t.yellow)),
            Span::raw(" "),
        ])
    } else {
        Line::raw("")
    };
    frame.render_widget(
        Paragraph::new(right_line)
            .alignment(Alignment::Right)
            .style(surface(t.chat)),
        right,
    );
}

fn draw_hairline(frame: &mut Frame, area: Rect, t: &Palette) {
    frame.render_widget(
        Paragraph::new("─".repeat(area.width as usize)).style(Style::new().fg(t.border).bg(t.chat)),
        area,
    );
}

fn draw_messages(frame: &mut Frame, app: &mut App, area: Rect) {
    let t = theme::palette();
    frame.render_widget(Block::default().style(surface(t.chat)), area);
    let inner = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(2),
        height: area.height,
    };

    let rendered = message_lines(app, inner.width as usize);
    let height = inner.height as usize;
    let total = rendered.lines.len();

    // A pending jump is resolved here, where the wrapped layout is known.
    if let Some(target) = app.jump_to.take() {
        match rendered
            .anchors
            .iter()
            .find(|(id, _)| *id == target)
            .map(|(_, line)| *line)
        {
            Some(anchor) => {
                // Put the message at the top of the window (or as close as the
                // end of the transcript allows).
                app.scroll_back = total.saturating_sub(anchor.saturating_add(height));
            }
            None => {
                app.status = "that message is no longer loaded; try /history".to_string();
            }
        }
    }

    // After prepending older history, put the message that was at the top of
    // the window back on the same row. `scroll_back` is measured from the
    // bottom, so without this the view would drift when the reader is at the
    // very top.
    if let Some((id, row)) = app.anchor_to.take()
        && let Some((_, line)) = rendered.anchors.iter().find(|(anchor, _)| *anchor == id)
    {
        app.scroll_back = anchored_scroll_back(total, height, *line, row as usize);
    }

    let end = total.saturating_sub(app.scroll_back.min(total));
    let start = end.saturating_sub(height);

    let mut visible: Vec<Line> = rendered
        .lines
        .into_iter()
        .skip(start)
        .take(height)
        .collect();
    let mut pad = 0;
    if visible.len() < height && start == 0 {
        pad = height - visible.len();
        let mut padded = vec![Line::raw(""); pad];
        padded.append(&mut visible);
        visible = padded;
    }

    // Remember the first message on screen so a prepend can anchor on it.
    app.top_anchor = rendered
        .anchors
        .iter()
        .find(|(_, line)| *line >= start)
        .map(|(id, line)| (*id, (line - start + pad) as u16));

    frame.render_widget(Paragraph::new(visible).style(surface(t.chat)), inner);
}

struct Rendered {
    lines: Vec<Line<'static>>,
    /// First rendered line of each message, in transcript order. Used to turn
    /// a jump request into a scroll position.
    anchors: Vec<(ovvc_client::EventId, usize)>,
}

fn message_lines(app: &App, width: usize) -> Rendered {
    let t = theme::palette();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut anchors: Vec<(ovvc_client::EventId, usize)> = Vec::new();
    let me = app.client.author_id();
    let mut previous_day: Option<String> = None;

    if let Some(channel) = app.current_channel()
        && !channel.kind.is_textual()
    {
        lines.push(Line::from(Span::styled(
            format!(
                "  -- {} channel: no {} UI yet --",
                channel.kind.label(),
                if channel.kind == ovvc_client::ChannelKind::Voice {
                    "call"
                } else {
                    "thread"
                }
            ),
            Style::new().fg(t.yellow),
        )));
        lines.push(Line::raw(""));
    }
    if app.history_loading {
        // The spinner is on the transcript itself so the reader can see the
        // older page is in flight, not only a changed status-bar word.
        let mut spans = vec![Span::raw("  ")];
        spans.extend(crate::spinner::bracket(t, &app.spinner));
        spans.push(Span::styled(" loading older messages", muted(t)));
        lines.push(center_line_widget(Line::from(spans), width));
        lines.push(Line::raw(""));
    } else if app.history_more {
        lines.push(center_line("[ load older ]", muted(t), width));
        lines.push(Line::raw(""));
    }

    // Edits and deletes are separate events; fold them so the transcript shows
    // the same final state on every peer.
    let entries = ovvc_client::timeline(&app.messages).unwrap_or_default();
    for entry in entries {
        match entry {
            ovvc_client::TimelineEntry::Message {
                event,
                body,
                edited,
                reply_to,
                mentions,
                reactions,
            } => {
                let day = format_date(event.body.timestamp);
                if previous_day.as_deref() != Some(day.as_str()) {
                    lines.push(day_rule(width, &day, t));
                    previous_day = Some(day);
                }
                if let Ok(id) = event.id()
                    && app.unread_divider == Some(id)
                    && !lines.is_empty()
                {
                    lines.push(new_messages_line(width, t));
                }
                let entry_start = lines.len();
                if let Ok(id) = event.id() {
                    anchors.push((id, entry_start));
                }
                if let Some(reply) = reply_to {
                    let target_name = display_name(app, reply.event.body.author);
                    let snippet = match &reply.body {
                        Some(target_body) => {
                            let available = width
                                .saturating_sub(12 + target_name.chars().count())
                                .max(8);
                            elide(&markdown::plain(target_body).replace('\n', " "), available)
                        }
                        None => "deleted message".to_string(),
                    };
                    lines.push(Line::from(Span::styled(
                        format!("  | re <{target_name}>: {snippet}"),
                        muted(t),
                    )));
                }
                let mentioned_me = mentions.contains(&me);
                lines.push(message_header(
                    app,
                    width,
                    event.body.author,
                    event.body.timestamp,
                    edited,
                    mentioned_me,
                    t,
                ));
                for line in render_body(&body, width.saturating_sub(2)) {
                    lines.push(indent(line, "  "));
                }
                for attachment in event.body.kind.attachments() {
                    lines.push(Line::from(Span::styled(
                        format!(
                            "  [file: {}  {}]",
                            attachment.name,
                            crate::attach::human_size(attachment.size)
                        ),
                        muted(t),
                    )));
                }
                if !reactions.is_empty() {
                    let mut spans = vec![Span::raw("  ")];
                    for (index, reaction) in reactions.iter().enumerate() {
                        if index > 0 {
                            spans.push(Span::raw(" "));
                        }
                        let mine = reaction.authors.contains(&me);
                        let style = if mine {
                            Style::new().fg(t.bright).bg(t.accent_dim)
                        } else {
                            muted(t)
                        };
                        spans.push(Span::styled(
                            format!(
                                "[{} {}]",
                                reaction_label(&reaction.emoji),
                                reaction.authors.len()
                            ),
                            style,
                        ));
                    }
                    lines.push(Line::from(spans));
                }
                // The control row is reserved for every message, so showing
                // the reply controls for the selected target does not reflow
                // the transcript.
                if let Ok(id) = event.id() {
                    lines.push(control_line(app, id, t));
                } else {
                    lines.push(Line::raw(""));
                }
            }
            ovvc_client::TimelineEntry::Deleted { event } => {
                let day = format_date(event.body.timestamp);
                if previous_day.as_deref() != Some(day.as_str()) {
                    lines.push(day_rule(width, &day, t));
                    previous_day = Some(day);
                }
                if let Ok(id) = event.id()
                    && app.unread_divider == Some(id)
                    && !lines.is_empty()
                {
                    lines.push(new_messages_line(width, t));
                }
                if let Ok(id) = event.id() {
                    anchors.push((id, lines.len()));
                }
                lines.push(message_header(
                    app,
                    width,
                    event.body.author,
                    event.body.timestamp,
                    false,
                    false,
                    t,
                ));
                lines.push(Line::from(Span::styled(
                    "  [message deleted]",
                    Style::new().fg(t.faint).add_modifier(Modifier::ITALIC),
                )));
                if let Ok(id) = event.id() {
                    lines.push(control_line(app, id, t));
                } else {
                    lines.push(Line::raw(""));
                }
            }
            ovvc_client::TimelineEntry::Other(event) => {
                let day = format_date(event.body.timestamp);
                if previous_day.as_deref() != Some(day.as_str()) {
                    lines.push(day_rule(width, &day, t));
                    previous_day = Some(day);
                }
                match &event.body.kind {
                    ovvc_client::EventKind::GuildCreated { name } => {
                        lines.push(system_line(format!("commune \"{name}\" was created"), t));
                    }
                    ovvc_client::EventKind::ChannelCreated { name } => {
                        lines.push(system_line(format!("channel #{name} was created"), t));
                    }
                    ovvc_client::EventKind::MemberJoined { display_name } => {
                        lines.push(system_line(format!("{display_name} joined"), t));
                    }
                    ovvc_client::EventKind::MemberRenamed { display_name } => {
                        lines.push(system_line(format!("{display_name} changed their name"), t));
                    }
                    ovvc_client::EventKind::MemberLeft => {
                        lines.push(system_line(
                            format!("{} left", event.body.author.short()),
                            t,
                        ));
                    }
                    // Message and modification kinds are handled above.
                    _ => {}
                }
            }
        }
    }

    if lines.is_empty() {
        lines.push(Line::raw(""));
        lines.push(center_line("-- no messages --", muted(t), width));
        lines.push(center_line(
            "say something to start the channel",
            faint(t),
            width,
        ));
    }

    Rendered { lines, anchors }
}

/// The header line of one message: presence, author, its id, edit and mention
/// markers on the left, clock on the right. This mirrors the GUI row, where the
/// avatar carries presence and the clock sits at the far edge.
fn message_header(
    app: &App,
    width: usize,
    author: ovvc_client::AuthorId,
    timestamp: u64,
    edited: bool,
    mentioned_me: bool,
    t: &Palette,
) -> Line<'static> {
    let name = display_name(app, author);
    let is_self = author == app.client.author_id();
    let color = if is_self {
        t.accent
    } else {
        t.author_color(author.as_bytes())
    };
    let status = app.presence_for(author);
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            theme::presence_glyph(status),
            Style::new().fg(t.presence_color(status)),
        ),
        Span::raw(" "),
        Span::styled(name, Style::new().fg(color).add_modifier(Modifier::BOLD)),
    ];
    if edited {
        spans.push(Span::styled(" [edited]", faint(t)));
    }
    if mentioned_me {
        spans.push(Span::styled(" [mention]", Style::new().fg(t.yellow)));
    }

    let stamp = clock(timestamp);
    let used: usize = spans.iter().map(|span| span.content.chars().count()).sum();
    let pad = width.saturating_sub(used + stamp.chars().count()).max(1);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(stamp, muted(t)));
    Line::from(spans)
}

/// One reserved row under every message. The controls appear only for the
/// current reply target, but the row is always present so the transcript does
/// not jump when the target changes.
fn control_line(app: &App, id: ovvc_client::EventId, t: &Palette) -> Line<'static> {
    if app.reply.as_ref().is_some_and(|reply| reply.target == id) {
        Line::from(Span::styled(
            "  [ reply ] [ react ] [ more ]",
            muted(t),
        ))
    } else {
        Line::raw("")
    }
}

fn system_line(text: String, t: &Palette) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {text}"),
        Style::new().fg(t.faint).add_modifier(Modifier::ITALIC),
    ))
}

fn day_rule(width: usize, day: &str, t: &Palette) -> Line<'static> {
    let label = format!(" {day} ");
    let remaining = width.saturating_sub(label.chars().count());
    let left = remaining / 2;
    let right = remaining - left;
    Line::from(vec![
        Span::styled("─".repeat(left), Style::new().fg(t.border)),
        Span::styled(label, Style::new().fg(t.faint)),
        Span::styled("─".repeat(right), Style::new().fg(t.border)),
    ])
}

fn new_messages_line(width: usize, t: &Palette) -> Line<'static> {
    let label = " new messages ";
    let remaining = width.saturating_sub(label.chars().count() + 1);
    Line::from(vec![
        Span::styled(label, Style::new().fg(t.yellow)),
        Span::styled("─".repeat(remaining), Style::new().fg(t.yellow)),
    ])
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect, t: &Palette) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::new().fg(t.border))
        .style(surface(t.panel));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let voice = app.voice_status_line();

    // A daemon error and a live call each get their own row. The call line no
    // longer shares a row with the online/place/channel prefix, which is what
    // pushed the E2EE badge past the right edge at 80 to 140 columns.
    let mut heights = Vec::with_capacity(3);
    if app.error.is_some() {
        heights.push(Constraint::Length(1));
    }
    if voice.is_some() {
        heights.push(Constraint::Length(1));
    }
    heights.push(Constraint::Length(1));
    let rows = Layout::vertical(heights).split(inner);
    let mut row = 0;

    if let Some(message) = &app.error {
        let (label, colour) = match app.error_class {
            Some(ErrorClass::Bug) => (" ! bug ", t.red),
            Some(ErrorClass::Operational) => (" ! warn ", t.yellow),
            Some(ErrorClass::Security) => (" ! security ", t.magenta),
            Some(ErrorClass::Policy) => (" ! policy ", t.yellow),
            Some(ErrorClass::User) => (" ! error ", t.red),
            None => (" ! error ", t.red),
        };
        let text = match (app.error_class, &app.error_log) {
            (Some(ErrorClass::Bug), Some(path)) => format!("{message} (log: {path})"),
            _ => message.clone(),
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(label, Style::new().fg(colour)),
                Span::styled(text, Style::new().fg(t.text)),
            ]))
            .style(surface(t.panel)),
            rows[row],
        );
        row += 1;
    }

    if let Some(voice) = voice {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {voice} "),
                Style::new().fg(t.bright),
            )))
            .style(surface(t.panel)),
            rows[row],
        );
        row += 1;
    }

    let status_area = rows[row];
    let left = status_area;

    let (place, place_color) = match app.current_guild() {
        Some(guild) => match app.direct_peer_name(guild.id) {
            Some(name) => (format!(" dm:{name} "), t.muted),
            None => (format!(" commune:{} ", guild.name), t.muted),
        },
        None => (" commune:- ".to_string(), t.faint),
    };
    let mut left_spans = Vec::new();
    // A busy state (connecting, loading, syncing) is the only status prefix
    // the bar needs; a healthy daemon has no separate online word.
    let spinner_label = if app.connecting {
        Some("connecting")
    } else if app.history_loading {
        Some("loading history")
    } else if app.syncing && app.spinner_active() {
        Some("syncing")
    } else {
        None
    };
    if let Some(label) = spinner_label {
        left_spans.push(Span::raw(" "));
        left_spans.extend(crate::spinner::bracket(t, &app.spinner));
        left_spans.push(Span::styled(format!(" {label} "), muted(t)));
        left_spans.push(Span::styled("|", Style::new().fg(t.border)));
    }
    // The status line is the single home for global online and peer state.
    let online = app.daemon_online && app.online_guild.is_some();
    left_spans.push(Span::styled(
        format!(" {} ", if online { "net:online" } else { "net:offline" }),
        Style::new().fg(if online { t.green } else { t.red }),
    ));
    if !app.daemon_online {
        left_spans.push(Span::styled(" ! daemon ", Style::new().fg(t.red)));
    }
    if online {
        left_spans.push(Span::styled(
            format!("{} peers ", app.online_peers.len()),
            muted(t),
        ));
    }
    left_spans.push(Span::styled("|", Style::new().fg(t.border)));
    left_spans.push(Span::styled(place, Style::new().fg(place_color)));
    left_spans.push(Span::styled("|", Style::new().fg(t.border)));
    left_spans.push(Span::styled(
        match app.current_channel() {
            Some(channel) => format!(" {}{} ", channel.kind.glyph(), channel.name),
            None => " channel:- ".to_string(),
        },
        Style::new().fg(if app.current_channel().is_some() {
            t.muted
        } else {
            t.faint
        }),
    ));
    if !app.status.is_empty() {
        left_spans.push(Span::styled("|", Style::new().fg(t.border)));
        left_spans.push(Span::styled(
            format!(" {} ", app.status),
            Style::new().fg(t.text),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(left_spans)).style(surface(t.panel)),
        left,
    );

}

fn draw_composer(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    t: &Palette,
    typing: &[ovvc_client::AuthorId],
) {
    let focused = app.focus == Focus::Composer;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(if focused { t.accent } else { t.border }))
        .style(surface(t.bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut constraints = Vec::new();
    if !typing.is_empty() {
        constraints.push(Constraint::Length(1));
    }
    if app.reply.is_some() {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(1));
    constraints.push(Constraint::Length(1));
    let rows = Layout::vertical(constraints).split(inner);
    let mut row = 0;

    if !typing.is_empty() {
        let names: Vec<String> = typing
            .iter()
            .map(|author| display_name(app, *author))
            .collect();
        let text = match names.len() {
            1 => format!("-- {} is typing", names[0]),
            2 => format!("-- {} and {} are typing", names[0], names[1]),
            _ => format!("-- {} others are typing", names.len()),
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, muted(t)))).style(surface(t.bg)),
            rows[row],
        );
        row += 1;
    }

    if let Some(reply) = &app.reply {
        let preview = elide(&markdown::plain(&reply.body).replace('\n', " "), 70);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(
                    "| re <{}>: {preview}  (esc: cancel)",
                    display_name(app, reply.author)
                ),
                muted(t),
            )))
            .style(surface(t.bg)),
            rows[row],
        );
        row += 1;
    }

    let input = if app.current_channel().is_none() {
        Line::from(Span::styled(
            " join or create a commune to start chatting",
            muted(t),
        ))
    } else if let Some((_, name)) = &app.pending_dm {
        // `/dm` chose a conversation whose log has not synced. Say so where a
        // person would otherwise expect their message to go to the public
        // channel that is still selected.
        Line::from(Span::styled(
            format!(" DM with {name} is syncing; messages are paused"),
            muted(t),
        ))
    } else if app.input.is_empty() {
        Line::from(vec![
            Span::styled("> ", Style::new().fg(t.accent).add_modifier(Modifier::BOLD)),
            Span::styled("message", Style::new().fg(t.faint)),
        ])
    } else {
        let mut spans = vec![Span::styled(
            "> ",
            Style::new().fg(t.accent).add_modifier(Modifier::BOLD),
        )];
        spans.extend(app.input.line().spans);
        Line::from(spans)
    };
    frame.render_widget(Paragraph::new(input).style(surface(t.bg)), rows[row]);
    row += 1;

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "enter: send   esc: clear   /help",
            Style::new().fg(t.faint),
        )))
        .style(surface(t.bg)),
        rows[row],
    );
}

// --- onboarding ---------------------------------------------------------

fn draw_onboarding(frame: &mut Frame, app: &App) {
    let t = theme::palette();
    let area = frame.area();
    frame.render_widget(Block::default().style(surface(t.bg)), area);

    let mut lines: Vec<Line<'static>> = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "  ovvc",
            Style::new().fg(t.accent).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled("  a terminal for peers", faint(t))),
        Line::raw(""),
    ];

    match app.onboarding.step {
        Step::Name => {
            lines.push(Line::from(Span::styled(
                "  What should people call you?",
                Style::new().fg(t.bright).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::raw(""));
            lines.push(prompt_line(&app.onboarding.name, "", t));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "  enter: continue · saved locally, change it any time",
                muted(t),
            )));
        }
        Step::Privacy => {
            lines.push(Line::from(Span::styled(
                "  Privacy and security",
                Style::new().fg(t.bright).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "  telemetry       ovvc collects none",
                Style::new().fg(t.text),
            )));
            lines.push(Line::from(Span::styled(
                "  invites         bearer codes: anyone holding one can join",
                Style::new().fg(t.text),
            )));
            lines.push(Line::from(Span::styled(
                "  addresses       direct, or through a relay you configure",
                Style::new().fg(t.text),
            )));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "  enter: continue · esc: back",
                muted(t),
            )));
        }
        Step::Choice => {
            lines.push(Line::from(Span::styled(
                format!("  Hi {}. How do you want to start?", app.profile),
                Style::new().fg(t.bright).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::raw(""));

            let options = [
                ("create commune", "start fresh, invite friends"),
                ("join commune", "paste an invite code"),
                ("skip for now", "set up later with /new"),
            ];
            for (index, (title, subtitle)) in options.iter().enumerate() {
                let selected = index == app.onboarding.choice;
                let base = if selected {
                    Style::new().fg(t.select_fg).bg(t.select_bg)
                } else {
                    Style::new().fg(t.text)
                };
                let sub = if selected {
                    Style::new().fg(t.select_fg).bg(t.select_bg)
                } else {
                    muted(t)
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        if selected { "  > " } else { "    " },
                        Style::new().fg(t.accent),
                    ),
                    Span::styled(format!("[{}] ", index + 1), base),
                    Span::styled(format!("{title:<16}"), base),
                    Span::styled((*subtitle).to_string(), sub),
                ]));
                lines.push(Line::raw(""));
            }
            lines.push(Line::from(Span::styled(
                "  up/down then enter · or press 1, 2, 3",
                muted(t),
            )));
        }
        Step::CreateGuild => {
            lines.push(Line::from(Span::styled(
                "  Name your commune",
                Style::new().fg(t.bright).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::raw(""));
            lines.push(prompt_line(&app.onboarding.text, "e.g. late night devs", t));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "  enter: create · esc: back",
                muted(t),
            )));
        }
        Step::JoinGuild => {
            lines.push(Line::from(Span::styled(
                "  Paste an invite code",
                Style::new().fg(t.bright).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::raw(""));
            lines.push(prompt_line(
                &app.onboarding.text,
                "the long code from a friend",
                t,
            ));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "  enter: verify and join · esc: back",
                muted(t),
            )));
        }
        Step::Joining => {
            lines.push(Line::from(Span::styled(
                "  Verifying the invite...",
                Style::new().fg(t.bright).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::raw(""));
            let mut connecting = vec![Span::raw("  ")];
            connecting.extend(crate::spinner::bracket(t, &app.spinner));
            connecting.push(Span::styled(
                " connecting to the commune host and syncing history.",
                muted(t),
            ));
            lines.push(Line::from(connecting));
            lines.push(Line::from(Span::styled(
                "  this can take a few seconds on the first join.",
                muted(t),
            )));
        }
    }

    if let Some(error) = &app.onboarding.error {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!("  {error}"),
            Style::new().fg(t.red),
        )));
    }

    let height = (lines.len() as u16 + 2)
        .min(area.height.saturating_sub(2))
        .max(6);
    let width = area.width.saturating_sub(4).min(64);
    let box_area = centered(area, width, height);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.accent))
        .title(Span::styled(
            "-- welcome --",
            Style::new().fg(t.accent).add_modifier(Modifier::BOLD),
        ))
        .style(surface(t.panel));
    let inner = block.inner(box_area);
    frame.render_widget(block, box_area);

    frame.render_widget(
        Paragraph::new(lines)
            .style(surface(t.panel))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

fn prompt_line(input: &crate::input::Input, placeholder: &str, t: &Palette) -> Line<'static> {
    let mut spans = vec![Span::styled(
        "  > ",
        Style::new().fg(t.accent).add_modifier(Modifier::BOLD),
    )];
    if input.is_empty() {
        spans.push(Span::styled("▏", Style::new().fg(t.accent)));
        if !placeholder.is_empty() {
            spans.push(Span::styled(
                format!(" {placeholder}"),
                Style::new().fg(t.faint),
            ));
        }
    } else {
        spans.extend(input.line().spans);
    }
    Line::from(spans)
}

// --- overlay ------------------------------------------------------------

fn draw_overlay(frame: &mut Frame, overlay: &Overlay) {
    let t = theme::palette();
    let area = frame.area();
    let width = area.width.saturating_sub(6).min(76);
    let content_width = (width as usize).saturating_sub(3).max(1);
    let body_lines: usize = overlay
        .body
        .iter()
        .map(|line| wrap(line, content_width).len())
        .sum();
    let height = (body_lines as u16 + 4)
        .min(area.height.saturating_sub(2))
        .max(6);
    let rect = centered(area, width, height);

    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.accent))
        .title(Span::styled(
            format!("-- {} --", overlay.title),
            Style::new().fg(t.accent).add_modifier(Modifier::BOLD),
        ))
        .style(surface(t.panel));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let mut lines: Vec<Line> = overlay
        .body
        .iter()
        .map(|line| Line::from(Span::styled(format!(" {line}"), Style::new().fg(t.text))))
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(" [ esc: close ]", muted(t))));

    frame.render_widget(
        Paragraph::new(lines)
            .style(surface(t.panel))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

// --- full-page views ---------------------------------------------------

fn page_lines(header: &str, t: &Palette) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::styled(format!(" {header}"), Style::new().fg(t.bright).add_modifier(Modifier::BOLD)),
        Span::styled("   [x]", muted(t)),
    ])]
}

fn draw_full_page(frame: &mut Frame, app: &App, title: &str, body: Vec<Line<'static>>, footer: &str) {
    let t = theme::palette();
    let area = frame.area();
    frame.render_widget(Block::default().style(surface(t.bg)), area);
    let [header, rule, content, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(1),
    ])
    .areas(area);
    let mut header_lines = page_lines(title, &t);
    if !app.status.is_empty() {
        header_lines[0].spans.push(Span::styled(
            format!("   {}", app.status),
            muted(&t),
        ));
    }
    frame.render_widget(Paragraph::new(header_lines).style(surface(t.panel)), header);
    frame.render_widget(
        Paragraph::new("─".repeat(rule.width as usize))
            .style(Style::new().fg(t.border).bg(t.bg)),
        rule,
    );
    frame.render_widget(
        Paragraph::new(body).style(surface(t.panel)).wrap(Wrap { trim: false }),
        content,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(format!(" {footer}"), muted(&t))))
            .style(surface(t.bg)),
        foot,
    );
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn settings_body(app: &App, t: &Palette) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![
            Span::raw(" "),
            Span::styled("detail level: ", muted(t)),
            Span::styled(format!("[ {} ]", app.detail_level.label()), Style::new().fg(t.select_fg).bg(t.select_bg)),
            Span::styled("  advanced   expert", muted(t)),
        ]),
        Line::raw(""),
    ];
    let add = |lines: &mut Vec<Line<'static>>, text: String| {
        lines.push(Line::from(Span::styled(format!(" {text}"), Style::new().fg(t.text))));
    };
    match app.settings_tab {
        SettingsTab::User => {
            let name = if app.profile.is_empty() { "anonymous" } else { &app.profile };
            add(&mut lines, "avatar          initials".to_string());
            add(&mut lines, format!("display name    [ {name} ]"));
            add(&mut lines, "bio             [ set from /profile ]".to_string());
            add(
                &mut lines,
                format!(
                    "presence        {} {}",
                    theme::presence_glyph(app.own_presence),
                    presence_name(app.own_presence)
                ),
            );
            if app.detail_level >= DetailLevel::Advanced {
                add(
                    &mut lines,
                    format!(
                        "connection      net:{} · {} peers · direct",
                        if app.online_guild.is_some() && app.daemon_online {
                            "online"
                        } else {
                            "offline"
                        },
                        app.online_peers.len()
                    ),
                );
                add(&mut lines, "theme           open /settings preferences".to_string());
                add(
                    &mut lines,
                    format!(
                        "notifications   desktop {} · dnd {}",
                        on_off(app.notifications.desktop),
                        on_off(app.notifications.dnd)
                    ),
                );
            }
            if app.detail_level == DetailLevel::Expert {
                let epoch = app
                    .voice
                    .audio
                    .as_ref()
                    .map(|audio| audio.epoch)
                    .filter(|epoch| *epoch > 0)
                    .map(|epoch| epoch.to_string())
                    .unwrap_or_else(|| "-".to_string());
                add(&mut lines, format!("media key epoch epoch {epoch}"));
                add(&mut lines, "diagnostics     /diagnostics".to_string());
            }
        }
        SettingsTab::VoiceVideo => {
            add(&mut lines, "input device    default microphone".to_string());
            add(&mut lines, "output device   default speakers".to_string());
            add(&mut lines, "push-to-talk    off   key: F8".to_string());
            if app.detail_level == DetailLevel::Expert {
                if let Some(audio) = &app.voice.audio {
                    add(
                        &mut lines,
                        format!(
                            "audio           backend={} peers={} latency={}ms xruns={}",
                            audio.backend, audio.peers, audio.latency_ms, audio.xruns
                        ),
                    );
                }
            }
        }
        SettingsTab::Network => {
            let online = app.online_guild.is_some() && app.daemon_online;
            add(
                &mut lines,
                format!("state           net:{}", if online { "online" } else { "offline" }),
            );
            if app.detail_level >= DetailLevel::Advanced {
                let listen = app
                    .options
                    .listen
                    .iter()
                    .map(|address| address.to_string())
                    .collect::<Vec<_>>()
                    .join(" ");
                let dial = app
                    .options
                    .dial
                    .iter()
                    .map(|address| address.to_string())
                    .collect::<Vec<_>>()
                    .join(" ");
                add(
                    &mut lines,
                    format!("listen          {}", if listen.is_empty() { "(default)" } else { &listen }),
                );
                add(
                    &mut lines,
                    format!("dial            {}", if dial.is_empty() { "(none)" } else { &dial }),
                );
                add(
                    &mut lines,
                    format!(
                        "relay           {}",
                        app.options
                            .relay
                            .as_ref()
                            .map(|relay| relay.to_string())
                            .unwrap_or_else(|| "(none; ovvc runs no relay)".to_string())
                    ),
                );
                add(&mut lines, format!("peers           {}", app.online_peers.len()));
            }
            if app.detail_level == DetailLevel::Expert {
                for address in &app.listen_addrs {
                    add(&mut lines, format!("{address}"));
                }
                for peer in &app.online_peers {
                    add(&mut lines, format!("peer {peer}"));
                }
                add(&mut lines, "diagnostics     /diagnostics".to_string());
            }
        }
        SettingsTab::Preferences => {
            add(
                &mut lines,
                format!("desktop notifications  {}", on_off(app.notifications.desktop)),
            );
            add(&mut lines, format!("do not disturb         {}", on_off(app.notifications.dnd)));
            let quiet = app
                .notifications
                .quiet_hours
                .map(|quiet| {
                    format!(
                        "{:02}:{:02}..{:02}:{:02} UTC",
                        quiet.start_minute / 60,
                        quiet.start_minute % 60,
                        quiet.end_minute / 60,
                        quiet.end_minute % 60
                    )
                })
                .unwrap_or_else(|| "off".to_string());
            add(&mut lines, format!("quiet hours            {quiet}"));
            add(
                &mut lines,
                format!(
                    "theme                  {}",
                    crate::theme::theme_path()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "built-in".to_string())
                ),
            );
            if app.detail_level >= DetailLevel::Advanced {
                add(&mut lines, format!("member panel           {}", on_off(app.show_members)));
            }
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        " tab: next   shift+tab: previous   left/right: switch tab",
        muted(t),
    )));
    lines.push(Line::from(Span::styled(
        " [ / ] or l: detail level   enter: open   esc: close",
        muted(t),
    )));
    lines
}

fn draw_settings(frame: &mut Frame, app: &App) {
    let t = theme::palette();
    let area = frame.area();
    frame.render_widget(Block::default().style(surface(t.bg)), area);
    let [header, tabs, rule, body] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(4),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(page_lines("settings", &t)).style(surface(t.panel)),
        header,
    );
    let mut tab_spans = vec![Span::raw(" ")];
    for tab in SettingsTab::ALL {
        let selected = app.settings_tab == tab;
        tab_spans.push(Span::styled(
            format!("{} {}", tab.icon(), tab.label()),
            if selected {
                Style::new().fg(t.select_fg).bg(t.select_bg)
            } else {
                muted(&t)
            },
        ));
        tab_spans.push(Span::raw("  "));
    }
    frame.render_widget(
        Paragraph::new(Line::from(tab_spans)).style(surface(t.panel)),
        tabs,
    );
    frame.render_widget(
        Paragraph::new("─".repeat(rule.width as usize))
            .style(Style::new().fg(t.border).bg(t.bg)),
        rule,
    );
    frame.render_widget(
        Paragraph::new(settings_body(app, &t))
            .style(surface(t.panel))
            .wrap(Wrap { trim: false }),
        body,
    );
}

fn draw_setup(frame: &mut Frame, app: &App) {
    let t = theme::palette();
    let mut lines = vec![Line::from(Span::styled(
        format!(" setup {}/6", app.setup_step),
        Style::new().fg(t.bright).add_modifier(Modifier::BOLD),
    ))];
    lines.push(Line::raw(""));
    for (index, label) in [
        "identity",
        "avatar and bio",
        "theme",
        "voice",
        "network",
        "review",
    ]
    .iter()
    .enumerate()
    {
        let step = index + 1;
        let marker = if step == app.setup_step {
            "[>]"
        } else if step < app.setup_step {
            "[x]"
        } else {
            "[ ]"
        };
        lines.push(Line::from(Span::styled(
            format!(" {marker} {step} {label}"),
            if step == app.setup_step {
                Style::new().fg(t.bright)
            } else if step < app.setup_step {
                Style::new().fg(t.accent)
            } else {
                faint(&t)
            },
        )));
    }
    lines.push(Line::raw(""));
    match app.setup_step {
        1 => {
            lines.push(Line::from(Span::styled(
                " display name",
                Style::new().fg(t.bright),
            )));
            lines.push(prompt_line(&app.input, "enter a display name", &t));
            lines.push(Line::from(Span::styled(
                " pronouns        optional",
                muted(&t),
            )));
            lines.push(Line::from(Span::styled(
                format!(" identity key    {}   read only", app.client.author_id().short()),
                faint(&t),
            )));
            lines.push(Line::from(Span::styled(
                " your key is generated locally. nothing is sent anywhere during setup.",
                faint(&t),
            )));
        }
        2 => {
            lines.push(Line::from(Span::styled(" avatar          initials", Style::new().fg(t.text))));
            lines.push(Line::from(Span::styled(" bio             [ set from /profile ]", Style::new().fg(t.text))));
            lines.push(Line::from(Span::styled(" no avatar is chosen; initials are used, as in the TUI member list.", faint(&t))));
        }
        3 => {
            lines.push(Line::from(Span::styled(" no theme is selected until you choose one", muted(&t))));
            for (index, preset) in ovvc_theme::Preset::all().iter().enumerate() {
                let selected = app.setup_theme == Some(index);
                lines.push(Line::from(Span::styled(
                    format!(
                        " {} [ {} ]",
                        index + 1,
                        preset.label()
                    ),
                    if selected {
                        Style::new().fg(t.select_fg).bg(t.select_bg)
                    } else {
                        Style::new().fg(t.text)
                    },
                )));
            }
            lines.push(Line::from(Span::styled(
                " the choice is saved for the next launch; the terminal palette is installed at start.",
                faint(&t),
            )));
        }
        4 => {
            lines.push(Line::from(Span::styled(" input device      default microphone", Style::new().fg(t.text))));
            lines.push(Line::from(Span::styled(" output device     default speakers", Style::new().fg(t.text))));
            lines.push(Line::from(Span::styled(" push-to-talk      off   key: F8", Style::new().fg(t.text))));
        }
        5 => {
            let listen = app
                .options
                .listen
                .first()
                .map(|address| address.to_string())
                .unwrap_or_else(|| "/ip4/0.0.0.0/tcp/41000".to_string());
            lines.push(Line::from(Span::styled(format!(" listen address    {listen}"), Style::new().fg(t.text))));
            lines.push(Line::from(Span::styled(" fixed port        TCP 41000; forward it for a friend outside your network", faint(&t))));
            lines.push(Line::from(Span::styled(" dial address      optional", Style::new().fg(t.text))));
            lines.push(Line::from(Span::styled(" relay address     optional; ovvc runs no relay service", faint(&t))));
            lines.push(Line::from(Span::styled(" presence visible  on", Style::new().fg(t.text))));
            lines.push(Line::from(Span::styled(" invite policy     bearer code, anyone holding it can join", faint(&t))));
        }
        _ => {
            let name = if app.setup_name.is_empty() {
                "(not set)".to_string()
            } else {
                app.setup_name.clone()
            };
            let theme = app
                .setup_theme
                .and_then(|index| ovvc_theme::Preset::all().get(index))
                .map(|preset| preset.label().to_string())
                .unwrap_or_else(|| "(not selected)".to_string());
            for (label, value) in [
                ("identity", name),
                ("avatar", "initials".to_string()),
                ("theme", theme),
                ("voice", "default microphone, default speakers, ptt off".to_string()),
                ("network", "direct addresses, presence visible".to_string()),
            ] {
                lines.push(Line::from(vec![
                    Span::styled(format!(" {label:<10}"), muted(&t)),
                    Span::styled(value, Style::new().fg(t.text)),
                ]));
            }
            lines.push(Line::from(Span::styled(
                " finish lists every choice, including the ones left off.",
                faint(&t),
            )));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        " n: next   p: previous   enter: edit field   esc: cancel",
        muted(&t),
    )));
    draw_full_page(frame, app, "setup", lines, "esc: cancel · n / p: step · enter: next / finish");
}

fn draw_network_map(frame: &mut Frame, app: &App) {
    let t = theme::palette();
    let mut lines = Vec::new();
    let name = if app.profile.is_empty() { "anonymous" } else { &app.profile };
    lines.push(Line::from(Span::styled(
        format!(" you @{name}"),
        Style::new().fg(t.accent).add_modifier(Modifier::BOLD),
    )));
    for index in app.commune_indices() {
        let guild = &app.guilds[index];
        let selected = app.guild_index == index;
        let peers = if selected { app.members.len() } else { 0 };
        let link = if selected && app.online_guild.is_some() && app.daemon_online {
            "direct"
        } else {
            "-"
        };
        lines.push(Line::from(Span::styled(
            format!(" +- {}  {peers} peers  {link}", guild.name),
            Style::new().fg(if selected { t.bright } else { t.text }),
        )));
        if selected {
            for member in &app.members {
                let status = app.presence_for(member.author);
                lines.push(Line::from(Span::styled(
                    format!(
                        " |  +- [{}] {}  {}  direct",
                        member
                            .display_name
                            .chars()
                            .next()
                            .map(|character| character.to_ascii_uppercase())
                            .unwrap_or('?'),
                        member_name(app, member),
                        presence_name(status)
                    ),
                    Style::new().fg(if status == ovvc_control::PresenceStatus::Offline {
                        t.faint
                    } else {
                        t.muted
                    }),
                )));
            }
        }
    }
    for conversation in &app.direct_messages {
        lines.push(Line::from(Span::styled(
            format!(" +- dm: {}  direct", conversation.peer.display_name),
            Style::new().fg(t.text),
        )));
    }
    if app.detail_level == DetailLevel::Expert {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(" this device", Style::new().fg(t.bright))));
        lines.push(Line::from(Span::styled(
            format!(" id {}", app.client.author_id().short()),
            faint(&t),
        )));
        for address in &app.listen_addrs {
            lines.push(Line::from(Span::styled(format!(" {address}"), faint(&t))));
        }
        for peer in &app.online_peers {
            lines.push(Line::from(Span::styled(format!(" peer {peer}"), faint(&t))));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        " ring: swarm · dot: peer · solid: direct · you: this device",
        faint(&t),
    )));
    draw_full_page(frame, app, "network map: swarms and peers", lines, "esc: close · /level expert: raw ids and addresses");
}

fn draw_diagnostics(frame: &mut Frame, app: &App) {
    let t = theme::palette();
    let mut lines = vec![Line::from(Span::styled(" diagnostics stay out of the main chat", muted(&t)))];
    lines.push(Line::from(Span::styled(
        " the alert strip keeps one line; the log path, classes and detail live here.",
        faint(&t),
    )));
    lines.push(Line::raw(""));
    lines.push(section_header(48, "error classes", &t));
    for (label, colour, text) in [
        ("! bug", t.red, "a failure that names the daemon log"),
        ("! warning", t.yellow, "an operational condition"),
        ("! security", t.magenta, "a security event"),
        ("! policy", t.cyan, "a policy view or refusal"),
        ("! error", t.bright, "a user-facing failure"),
    ] {
        lines.push(Line::from(vec![
            Span::styled(format!(" {label:<12}"), Style::new().fg(colour)),
            Span::styled(text.to_string(), muted(&t)),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(section_header(48, "current alert", &t));
    match (&app.error, &app.status) {
        (Some(error), _) => lines.push(Line::from(Span::styled(
            format!(" {} {error}", app.error_class.map(|class| match class {
                ErrorClass::Bug => "! bug",
                ErrorClass::Operational => "! warning",
                ErrorClass::Security => "! security",
                ErrorClass::Policy => "! policy",
                ErrorClass::User => "! error",
            }).unwrap_or("! error")),
            Style::new().fg(t.red),
        ))),
        (None, status) if !status.is_empty() => lines.push(Line::from(Span::styled(
            format!(" {status}"),
            muted(&t),
        ))),
        _ => lines.push(Line::from(Span::styled(" -- no current alert --", faint(&t)))),
    }
    if let Some(log) = &app.error_log {
        lines.push(Line::from(Span::styled(format!(" log {log}"), faint(&t))));
    }
    lines.push(Line::raw(""));
    lines.push(section_header(48, "daemon", &t));
    lines.push(Line::from(Span::styled(
        format!(
            " version   {}",
            app.daemon_version.as_deref().unwrap_or("unknown")
        ),
        Style::new().fg(t.text),
    )));
    lines.push(Line::from(Span::styled(
        format!(" data      {}", app.data_dir.display()),
        muted(&t),
    )));
    lines.push(Line::raw(""));
    lines.push(section_header(48, "network", &t));
    let online = app.online_guild.is_some() && app.daemon_online;
    lines.push(Line::from(Span::styled(
        format!(" net:{}", if online { "online" } else { "offline" }),
        Style::new().fg(if online { t.green } else { t.red }),
    )));
    lines.push(Line::from(Span::styled(
        format!(" {} peers", app.online_peers.len()),
        muted(&t),
    )));
    for address in &app.listen_addrs {
        lines.push(Line::from(Span::styled(format!(" {address}"), faint(&t))));
    }
    if let Some(audio) = &app.voice.audio {
        lines.push(Line::from(Span::styled(
            format!(" e2ee epoch {} · trust signed", audio.epoch),
            muted(&t),
        )));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        " policy: /policy shows a read-only view of this commune's policy.",
        faint(&t),
    )));
    draw_full_page(
        frame,
        app,
        "diagnostics",
        lines,
        "esc: close · /status /config /policy: text equivalents",
    );
}

// --- message body -------------------------------------------------------

fn render_body(body: &str, width: usize) -> Vec<Line<'static>> {
    let t = theme::palette();
    let mut out = Vec::new();
    for line in markdown::parse(body) {
        if line.spans.iter().all(|span| span.text.is_empty()) {
            out.push(Line::raw(""));
            continue;
        }
        for spans in wrap_spans(&line.spans, width.max(1), line.code_block, t) {
            out.push(Line::from(spans));
        }
    }
    if out.is_empty() {
        out.push(Line::raw(""));
    }
    out
}

fn markdown_style(style: markdown::Style, code_block: bool, t: &Palette) -> Style {
    let mut out = Style::new().fg(t.text);
    if style.code || code_block {
        out = out.bg(t.code_bg);
    }
    if style.bold {
        out = out.fg(t.bright).add_modifier(Modifier::BOLD);
    }
    if style.italic {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.link {
        out = out.fg(t.cyan).add_modifier(Modifier::UNDERLINED);
    }
    if style.mention {
        out = out.fg(t.accent);
    }
    if style.strike {
        out = out.fg(t.muted).add_modifier(Modifier::CROSSED_OUT);
    }
    if style.spoiler {
        // No per-span hover in a terminal, so the text is dimmed rather than
        // pretending to be hidden.
        out = out.fg(t.faint).bg(t.code_bg);
    }
    if style.channel {
        out = out.fg(t.cyan).add_modifier(Modifier::UNDERLINED);
    }
    out
}

/// Wrap styled spans to `width` columns, splitting tokens that are too long
/// and keeping each run's style attached to its characters.
fn wrap_spans(
    spans: &[markdown::Span],
    width: usize,
    code_block: bool,
    t: &Palette,
) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut words: Vec<Vec<(char, Style)>> = Vec::new();
    let mut word: Vec<(char, Style)> = Vec::new();
    for span in spans {
        let style = markdown_style(span.style, code_block, t);
        for character in span.text.chars() {
            if character.is_whitespace() {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            } else {
                word.push((character, style));
            }
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    if words.is_empty() {
        return vec![Vec::new()];
    }

    let mut lines: Vec<Vec<(char, Style)>> = Vec::new();
    let mut current: Vec<(char, Style)> = Vec::new();
    for mut candidate in words {
        let separator = usize::from(!current.is_empty());
        if !current.is_empty() && current.len() + separator + candidate.len() <= width {
            current.push((' ', candidate[0].1));
            current.extend(candidate);
            continue;
        }
        if !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        while candidate.len() > width {
            let rest = candidate.split_off(width);
            lines.push(std::mem::take(&mut candidate));
            candidate = rest;
        }
        current = candidate;
    }
    if !current.is_empty() {
        lines.push(current);
    }

    lines.into_iter().map(merge_styles).collect()
}

fn merge_styles(chars: Vec<(char, Style)>) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let mut current = Style::default();
    for (character, style) in chars {
        if style != current {
            if !text.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut text), current));
            }
            current = style;
        }
        text.push(character);
    }
    if !text.is_empty() {
        spans.push(Span::styled(text, current));
    }
    spans
}

fn indent(line: Line<'static>, prefix: &str) -> Line<'static> {
    let mut spans = vec![Span::raw(prefix.to_string())];
    spans.extend(line.spans);
    Line::from(spans)
}

// --- helpers ------------------------------------------------------------

pub(crate) fn display_name(app: &App, author: ovvc_client::AuthorId) -> String {
    if author == app.client.author_id() && !app.profile.is_empty() {
        return app.profile.clone();
    }
    app.members
        .iter()
        .find(|member| member.author == author)
        .map(|member| member.display_name.clone())
        .unwrap_or_else(|| author.short())
}

/// The GUI's short reaction labels, so an emoji that the terminal font cannot
/// draw still reads as something.
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

fn clock(timestamp: u64) -> String {
    time::OffsetDateTime::from_unix_timestamp(timestamp as i64)
        .ok()
        .and_then(|time| time.format(CLOCK).ok())
        .unwrap_or_else(|| "--:--".to_string())
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

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

/// A muted title followed by a hairline rule to the end of the row.
fn section_header(width: usize, title: &str, t: &Palette) -> Line<'static> {
    let label = format!(" {} ", title.to_lowercase());
    let remaining = width.saturating_sub(label.chars().count());
    Line::from(vec![
        Span::styled(label, Style::new().fg(t.muted)),
        Span::styled("─".repeat(remaining), Style::new().fg(t.border)),
    ])
}

/// A left label and an optional right badge, padded to fill the row so a
/// reverse-video selection covers the whole width.
fn row_line(
    left: String,
    right: Option<String>,
    width: usize,
    left_style: Style,
    right_style: Style,
) -> Line<'static> {
    let left = clamp(left, width);
    let left_len = left.chars().count();
    let right = right.map(|right| clamp(right, width.saturating_sub(left_len + 1)));
    let right_len = right
        .as_ref()
        .map(|right| right.chars().count())
        .unwrap_or(0);
    let gap = width.saturating_sub(left_len + right_len);
    let mut spans = vec![Span::styled(left, left_style)];
    spans.push(Span::styled(" ".repeat(gap), left_style));
    if let Some(right) = right
        && !right.is_empty()
    {
        spans.push(Span::styled(right, right_style));
    }
    Line::from(spans)
}

fn center_line(text: &str, style: Style, width: usize) -> Line<'static> {
    let len = text.chars().count().min(width);
    let left = width.saturating_sub(len) / 2;
    Line::from(vec![
        Span::raw(" ".repeat(left)),
        Span::styled(text.to_string(), style),
    ])
}

/// Centre a line that is already built from styled spans, such as a spinner.
fn center_line_widget(line: Line<'static>, width: usize) -> Line<'static> {
    let len = line.width().min(width);
    let left = width.saturating_sub(len) / 2;
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::raw(" ".repeat(left)));
    spans.extend(line.spans);
    Line::from(spans)
}

fn clamp(text: String, width: usize) -> String {
    if text.chars().count() <= width {
        return text;
    }
    text.chars().take(width).collect()
}

pub(crate) fn elide(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    if max <= 3 {
        return text.chars().take(max).collect();
    }
    let mut out: String = text.chars().take(max - 3).collect();
    out.push_str("...");
    out
}

/// Wrap `text` to `width` columns, splitting words that are too long. Used for
/// dialog heights (so a long identity line does not hide the footer); the
/// message body uses [`wrap_spans`] so styling survives wrapping.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();

    for word in text.split_whitespace() {
        let mut rest = word;
        loop {
            let line_len = line.chars().count();
            let rest_len = rest.chars().count();
            let separator = usize::from(!line.is_empty());

            if line_len + separator + rest_len <= width {
                if separator == 1 {
                    line.push(' ');
                }
                line.push_str(rest);
                break;
            }

            if line.is_empty() {
                let split = char_offset(rest, width);
                lines.push(rest[..split].to_string());
                rest = &rest[split..];
                if rest.is_empty() {
                    break;
                }
            } else {
                lines.push(std::mem::take(&mut line));
            }
        }
    }

    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }

    lines
}

fn char_offset(text: &str, count: usize) -> usize {
    text.char_indices()
        .nth(count)
        .map(|(offset, _)| offset)
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    use super::{ErrorClass, centered, draw, format_date, message_lines, wrap};
    use crate::app::{App, AppOptions, Overlay};

    fn render(app: &mut App) -> String {
        render_at(app, 90, 28)
    }

    fn render_at(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, app))
            .expect("drawing the test frame");
        let cells: Vec<&str> = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        cells
            .chunks(width as usize)
            .map(|row| row.concat())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_the_overlay_and_the_error_strip() {
        let dir = tempfile::tempdir().expect("temp dir");
        let client = Arc::new(ovvc_client::Client::open_local(dir.path()).expect("client"));
        client.create_guild("terminal").expect("guild");
        std::fs::write(dir.path().join(crate::profile::FILE_NAME), "ada").expect("profile");
        let mut app =
            App::new(client, dir.path().to_path_buf(), AppOptions::default()).expect("app");
        app.overlay = Some(Overlay {
            title: "status".to_string(),
            body: vec![
                "network: offline".to_string(),
                "peers:   0".to_string(),
                "/connect brings the commune online".to_string(),
            ],
        });
        app.error = Some("daemon went away".to_string());
        app.status = "send failed: not connected".to_string();
        let screen = render(&mut app);
        assert!(screen.contains("status"), "{screen:?}");
        assert!(screen.contains("network: offline"), "{screen:?}");
        assert!(screen.contains("peers:   0"), "{screen:?}");
        assert!(screen.contains("daemon went away"), "{screen:?}");
        assert!(screen.contains("send failed: not connected"), "{screen:?}");
    }

    #[test]
    fn the_settings_screen_shows_tabs_and_gates_detail() {
        let dir = tempfile::tempdir().expect("temp dir");
        let client = Arc::new(ovvc_client::Client::open_local(dir.path()).expect("client"));
        client.create_guild("terminal").expect("guild");
        std::fs::write(dir.path().join(crate::profile::FILE_NAME), "ada").expect("profile");
        let mut app =
            App::new(client, dir.path().to_path_buf(), AppOptions::default()).expect("app");
        app.screen = crate::app::Screen::Settings;
        app.settings_tab = crate::app::SettingsTab::User;
        app.detail_level = crate::app::DetailLevel::Simple;
        let screen = render(&mut app);
        assert!(screen.contains("[U] user"), "{screen}");
        assert!(screen.contains("[P] preferences"), "{screen}");
        assert!(screen.contains("detail level"), "{screen}");
        assert!(
            !screen.contains("media key epoch"),
            "expert rows must be hidden at simple level: {screen}"
        );

        app.detail_level = crate::app::DetailLevel::Expert;
        let screen = render(&mut app);
        assert!(screen.contains("media key epoch"), "{screen}");
        assert!(
            !screen.contains("plugin"),
            "the plugin tab and its rows are not part of v0.1: {screen}"
        );
        assert!(
            !screen.contains("relay"),
            "relay-only controls are not part of v0.1: {screen}"
        );

        app.settings_tab = crate::app::SettingsTab::Preferences;
        let screen = render(&mut app);
        assert!(screen.contains("desktop notifications"), "{screen}");
    }

    #[test]
    fn the_setup_screen_lists_all_six_steps() {
        let dir = tempfile::tempdir().expect("temp dir");
        let client = Arc::new(ovvc_client::Client::open_local(dir.path()).expect("client"));
        client.create_guild("terminal").expect("guild");
        std::fs::write(dir.path().join(crate::profile::FILE_NAME), "ada").expect("profile");
        let mut app =
            App::new(client, dir.path().to_path_buf(), AppOptions::default()).expect("app");
        app.screen = crate::app::Screen::Setup;
        app.setup_step = 1;
        app.setup_name = "ada".to_string();
        app.input = crate::input::Input::with_text("ada");
        let screen = render(&mut app);
        assert!(screen.contains("[>] 1 identity"), "{screen}");
        assert!(screen.contains("[ ] 6 review"), "{screen}");
        assert!(screen.contains("display name"), "{screen}");
        assert!(screen.contains("read only"), "{screen}");
        assert!(
            !screen.contains("plugins"),
            "the plugin step is not part of v0.1: {screen}"
        );

        app.setup_step = 3;
        let screen = render(&mut app);
        assert!(screen.contains("Phosphor"), "{screen}");
        assert!(screen.contains("no theme is selected"), "{screen}");
    }

    #[test]
    fn the_network_map_and_diagnostics_screens_render() {
        let dir = tempfile::tempdir().expect("temp dir");
        let client = Arc::new(ovvc_client::Client::open_local(dir.path()).expect("client"));
        client.create_guild("terminal").expect("guild");
        std::fs::write(dir.path().join(crate::profile::FILE_NAME), "ada").expect("profile");
        let mut app =
            App::new(client, dir.path().to_path_buf(), AppOptions::default()).expect("app");

        app.screen = crate::app::Screen::NetworkMap;
        let screen = render(&mut app);
        assert!(screen.contains("network map: swarms and peers"), "{screen}");
        assert!(screen.contains("you @ada"), "{screen}");
        assert!(screen.contains("terminal"), "{screen}");

        app.screen = crate::app::Screen::Diagnostics;
        app.error = Some("attachment fetch failed".to_string());
        app.error_log = Some("/tmp/ovvc/daemon.log".to_string());
        app.error_class = Some(ErrorClass::Bug);
        let screen = render(&mut app);
        assert!(screen.contains("diagnostics"), "{screen}");
        assert!(screen.contains("! bug"), "{screen}");
        assert!(screen.contains("/tmp/ovvc/daemon.log"), "{screen}");
    }

    #[test]
    fn the_reply_control_row_is_reserved_at_rest() {
        let dir = tempfile::tempdir().expect("temp dir");
        let client = Arc::new(ovvc_client::Client::open_local(dir.path()).expect("client"));
        let guild = client.create_guild("terminal").expect("guild");
        let channel = client.default_channel(guild.id).expect("channel");
        let message = client
            .post_message(guild.id, channel.id, "the loader is eight frames")
            .expect("message");
        let me = client.author_id();
        std::fs::write(dir.path().join(crate::profile::FILE_NAME), "ada").expect("profile");
        let mut app =
            App::new(client, dir.path().to_path_buf(), AppOptions::default()).expect("app");

        let baseline = message_lines(&app, 80).lines.len();
        app.reply = Some(crate::app::ReplyTarget {
            target: message.id().expect("id"),
            author: me,
            body: "the loader is eight frames".to_string(),
        });
        let rendered = message_lines(&app, 80);
        assert_eq!(
            rendered.lines.len(),
            baseline,
            "showing the reply control must not add a row"
        );
        let text = rendered
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("[ reply ] [ react ] [ more ]"), "{text}");
    }

    #[test]
    fn the_error_strip_labels_a_bug_with_its_log_and_warns_about_operational_conditions() {
        let dir = tempfile::tempdir().expect("temp dir");
        let client = Arc::new(ovvc_client::Client::open_local(dir.path()).expect("client"));
        client.create_guild("terminal").expect("guild");
        std::fs::write(dir.path().join(crate::profile::FILE_NAME), "ada").expect("profile");
        let mut app =
            App::new(client, dir.path().to_path_buf(), AppOptions::default()).expect("app");

        app.error = Some("an impossible state".to_string());
        app.error_class = Some(ErrorClass::Bug);
        app.error_log = Some("/tmp/ovvc/daemon.log".to_string());
        let screen = render(&mut app);
        assert!(screen.contains("! bug"), "{screen:?}");
        assert!(screen.contains("/tmp/ovvc/daemon.log"), "{screen:?}");

        // An operational condition must not be painted as a bug, but it is
        // still visible as a warning.
        app.error = Some("peer 3f2a went offline".to_string());
        app.error_class = Some(ErrorClass::Operational);
        app.error_log = None;
        let screen = render(&mut app);
        assert!(screen.contains("! warn"), "{screen:?}");
        assert!(screen.contains("peer 3f2a went offline"), "{screen:?}");
        assert!(!screen.contains("! bug"), "{screen:?}");
    }

    #[test]
    fn renders_the_onboarding_prompt() {
        let dir = tempfile::tempdir().expect("temp dir");
        let client = Arc::new(ovvc_client::Client::open_local(dir.path()).expect("client"));
        let mut app =
            App::new(client, dir.path().to_path_buf(), AppOptions::default()).expect("app");

        let screen = render(&mut app);
        assert!(screen.contains("ovvc"), "title missing from {screen:?}");
        assert!(
            screen.contains("a terminal for peers"),
            "subtitle missing from {screen:?}"
        );
        assert!(
            screen.contains("What should people call you?"),
            "prompt missing from {screen:?}"
        );
    }

    #[test]
    fn renders_the_privacy_step() {
        let dir = tempfile::tempdir().expect("temp dir");
        let client = Arc::new(ovvc_client::Client::open_local(dir.path()).expect("client"));
        let mut app =
            App::new(client, dir.path().to_path_buf(), AppOptions::default()).expect("app");
        app.onboarding.step = crate::app::Step::Privacy;
        let screen = render(&mut app);
        assert!(screen.contains("Privacy and security"), "{screen:?}");
        assert!(screen.contains("ovvc collects none"), "{screen:?}");
        assert!(
            !screen.contains("hide my IP"),
            "the privacy step must not advertise a control with no backing: {screen:?}"
        );
        assert!(
            !screen.contains("relay only"),
            "the privacy step must not advertise relay-only mode: {screen:?}"
        );
    }

    #[test]
    fn renders_categories_kinds_and_mutes_in_the_sidebar() {
        let dir = tempfile::tempdir().expect("temp dir");
        let client = Arc::new(ovvc_client::Client::open_local(dir.path()).expect("client"));
        let guild = client.create_guild("terminal").expect("guild");
        let category = client
            .create_category(guild.id, "projects")
            .expect("category");
        let channel = client
            .create_channel_in(
                guild.id,
                "standup",
                ovvc_client::ChannelKind::Voice,
                Some(category.id),
            )
            .expect("channel");
        std::fs::write(dir.path().join(crate::profile::FILE_NAME), "ada").expect("profile");
        let mut app =
            App::new(client, dir.path().to_path_buf(), AppOptions::default()).expect("app");
        app.reload_channels();
        app.notifications.muted_channels.push(channel.id);
        let screen = render(&mut app);
        assert!(screen.contains("-- projects --"), "{screen:?}");
        assert!(screen.contains("~ standup"), "{screen:?}");
        assert!(screen.contains("~ standup [m]"), "{screen:?}");
    }

    #[test]
    fn renders_the_chat_layout() {
        let dir = tempfile::tempdir().expect("temp dir");
        let client = Arc::new(ovvc_client::Client::open_local(dir.path()).expect("client"));
        client.create_guild("Lan Party").expect("guild");
        std::fs::write(dir.path().join(crate::profile::FILE_NAME), "ada").expect("profile");

        let mut app =
            App::new(client, dir.path().to_path_buf(), AppOptions::default()).expect("app");
        let screen = render(&mut app);

        assert!(
            screen.contains("Lan Party"),
            "guild missing from {screen:?}"
        );
        assert!(
            screen.contains("# general"),
            "channel missing from {screen:?}"
        );
        assert!(
            screen.contains("members 1"),
            "members panel missing from {screen:?}"
        );
        assert!(
            !screen.contains("daemon:online"),
            "a healthy daemon should not draw an online badge: {screen:?}"
        );
    }

    #[test]
    #[ignore = "prints a rendered frame for manual inspection"]
    fn print_frames() {
        let dir = tempfile::tempdir().expect("temp dir");
        let client = Arc::new(ovvc_client::Client::open_local(dir.path()).expect("client"));
        let mut app = App::new(
            client.clone(),
            dir.path().to_path_buf(),
            AppOptions::default(),
        )
        .expect("app");
        println!("--- onboarding ---\n{}", render(&mut app));

        let guild = client.create_guild("Lan Party").expect("guild");
        let channel = client.default_channel(guild.id).expect("channel");
        let question = client
            .post_message(guild.id, channel.id, "is the daemon up?")
            .expect("message");
        let me = client.author_id();
        client
            .post_message_full(
                guild.id,
                channel.id,
                "it is, **all green** from here",
                Some(question.id().unwrap()),
                &[me],
            )
            .expect("reply");
        std::fs::write(dir.path().join(crate::profile::FILE_NAME), "ada").expect("profile");
        let mut app =
            App::new(client, dir.path().to_path_buf(), AppOptions::default()).expect("app");
        println!("--- chat ---\n{}", render(&mut app));

        app.screen = crate::app::Screen::Onboarding;
        app.onboarding.name = crate::input::Input::default();
        app.profile = "ada".to_string();
        app.onboarding.step = crate::app::Step::Choice;
        println!("--- choice step ---\n{}", render(&mut app));
    }

    #[test]
    fn existing_profile_without_guilds_opens_the_choice_screen() {
        let dir = tempfile::tempdir().expect("temp dir");
        let client = Arc::new(ovvc_client::Client::open_local(dir.path()).expect("client"));
        std::fs::write(dir.path().join(crate::profile::FILE_NAME), "ada").expect("profile");

        let mut app =
            App::new(client, dir.path().to_path_buf(), AppOptions::default()).expect("app");
        assert_eq!(app.screen, crate::app::Screen::Onboarding);
        assert_eq!(app.onboarding.step, crate::app::Step::Choice);

        let screen = render(&mut app);
        assert!(
            screen.contains("Hi ada"),
            "greeting missing from {screen:?}"
        );
    }

    #[test]
    fn renders_reply_context_and_marks_mentions() {
        let dir = tempfile::tempdir().expect("temp dir");
        let client = Arc::new(ovvc_client::Client::open_local(dir.path()).expect("client"));
        let guild = client.create_guild("threads").expect("guild");
        let channel = client.default_channel(guild.id).expect("channel");
        let original = client
            .post_message(guild.id, channel.id, "the question")
            .expect("message");
        let me = client.author_id();
        let reply = client
            .post_message_full(
                guild.id,
                channel.id,
                "the answer",
                Some(original.id().unwrap()),
                &[me],
            )
            .expect("reply");
        std::fs::write(dir.path().join(crate::profile::FILE_NAME), "ada").expect("profile");

        let mut app =
            App::new(client, dir.path().to_path_buf(), AppOptions::default()).expect("app");
        let screen = render(&mut app);
        assert!(
            screen.contains("| re <"),
            "reply context missing from {screen:?}"
        );
        assert!(
            screen.contains("the question"),
            "reply target missing from {screen:?}"
        );
        assert!(
            screen.contains("[mention]"),
            "mention marker missing from {screen:?}"
        );

        // The jump anchor points at the reply-context line, so a jump shows
        // what is being answered, not just the answer.
        let rendered = message_lines(&app, 80);
        let (_, anchor) = rendered
            .anchors
            .iter()
            .find(|(id, _)| *id == reply.id().unwrap())
            .expect("the reply has an anchor");
        let anchor_text: String = rendered.lines[*anchor]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(
            anchor_text.contains("the question"),
            "anchor text was {anchor_text:?}"
        );
    }

    #[test]
    fn renders_the_unread_divider_load_older_and_guild_badge() {
        let dir = tempfile::tempdir().expect("temp dir");
        let shared =
            ovvc_client::shared_storage(ovvc_client::Store::open(dir.path()).expect("store"));
        let client = Arc::new(
            ovvc_client::Client::with_storage(ovvc_client::Identity::generate(), shared.clone())
                .expect("reader"),
        );
        let other = ovvc_client::Client::with_storage(ovvc_client::Identity::generate(), shared)
            .expect("author");

        let guild = client.create_guild("Lan Party").expect("guild");
        let channel = client.default_channel(guild.id).expect("channel");
        let first = other
            .post_message(guild.id, channel.id, "first")
            .expect("message");
        client
            .mark_read(guild.id, channel.id, first.id().unwrap())
            .expect("read marker");
        other
            .post_message(guild.id, channel.id, "unread one")
            .expect("message");
        std::fs::write(dir.path().join(crate::profile::FILE_NAME), "ada").expect("profile");

        let mut app = App::new(
            client.clone(),
            dir.path().to_path_buf(),
            AppOptions::default(),
        )
        .expect("app");

        let screen = render(&mut app);
        assert!(
            screen.contains("new messages"),
            "divider missing from {screen:?}"
        );

        app.history_more = true;
        let screen = render(&mut app);
        assert!(
            screen.contains("[ load older ]"),
            "load-older line missing from {screen:?}"
        );

        app.unread.insert(channel.id, 2);
        let screen = render(&mut app);
        assert!(
            screen.contains("[2]"),
            "unread badge missing from {screen:?}"
        );
    }

    #[test]
    fn renders_day_rules_file_chips_and_markdown() {
        let dir = tempfile::tempdir().expect("temp dir");
        let client = Arc::new(ovvc_client::Client::open_local(dir.path()).expect("client"));
        let guild = client.create_guild("files").expect("guild");
        let channel = client.default_channel(guild.id).expect("channel");
        let message = client
            .post_message(guild.id, channel.id, "**bold** and `code`")
            .expect("message");
        std::fs::write(dir.path().join(crate::profile::FILE_NAME), "ada").expect("profile");

        let mut app =
            App::new(client, dir.path().to_path_buf(), AppOptions::default()).expect("app");
        let screen = render(&mut app);
        assert!(
            screen.contains("bold and code"),
            "markdown was not flattened: {screen:?}"
        );
        let rendered = message_lines(&app, 80);
        let text: String = rendered
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        let day = format_date(message.body.timestamp);
        assert!(text.contains(&day), "day rule {day} missing: {text:?}");
    }

    #[test]
    fn wraps_words_and_splits_long_tokens() {
        assert_eq!(wrap("hello world", 20), vec!["hello world"]);
        assert_eq!(wrap("hello world", 5), vec!["hello", "world"]);
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(wrap("", 10), vec![""]);
    }

    #[test]
    fn centres_within_bounds() {
        let area = Rect::new(0, 0, 80, 24);
        let rect = centered(area, 20, 10);
        assert_eq!(rect.x, 30);
        assert_eq!(rect.y, 7);
        let clamped = centered(Rect::new(0, 0, 10, 5), 20, 10);
        assert_eq!(clamped.width, 10);
        assert_eq!(clamped.height, 5);
    }

    fn chat_app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().expect("temp dir");
        let client = Arc::new(ovvc_client::Client::open_local(dir.path()).expect("client"));
        client.create_guild("Lan Party").expect("guild");
        std::fs::write(dir.path().join(crate::profile::FILE_NAME), "ada").expect("profile");
        let app = App::new(client, dir.path().to_path_buf(), AppOptions::default()).expect("app");
        (dir, app)
    }

    #[test]
    fn the_status_spinner_advances_only_while_connecting() {
        let (_dir, mut app) = chat_app();
        app.connecting = true;
        let first = render_at(&mut app, 80, 24);
        assert!(first.contains("[⠋] connecting"), "{first}");
        app.spinner_tick();
        let second = render_at(&mut app, 80, 24);
        assert!(second.contains("[⠙] connecting"), "{second}");
        assert!(!second.contains("[⠋] connecting"), "{second}");

        // An idle terminal neither shows nor advances the spinner.
        app.connecting = false;
        app.spinner_tick();
        let idle = render_at(&mut app, 80, 24);
        assert!(!idle.contains(" connecting "), "{idle}");
        assert!(!idle.contains("[⠋]"), "{idle}");
    }

    #[test]
    fn the_onboarding_join_step_paints_the_spinner() {
        let dir = tempfile::tempdir().expect("temp dir");
        let client = Arc::new(ovvc_client::Client::open_local(dir.path()).expect("client"));
        let mut app =
            App::new(client, dir.path().to_path_buf(), AppOptions::default()).expect("app");
        app.onboarding.step = crate::app::Step::Joining;

        let first = render_at(&mut app, 80, 24);
        assert!(
            first.contains("[⠋] connecting to the commune host and syncing history."),
            "{first}"
        );
        app.spinner_tick();
        let second = render_at(&mut app, 80, 24);
        assert!(
            second.contains("[⠙] connecting to the commune host"),
            "{second}"
        );
    }

    #[test]
    fn a_history_load_paints_the_spinner_in_the_transcript() {
        let (_dir, mut app) = chat_app();
        app.history_loading = true;
        let screen = render_at(&mut app, 80, 24);
        assert!(
            screen.contains("[⠋] loading older messages"),
            "the transcript spinner is missing: {screen}"
        );
        assert!(
            screen.contains("loading history"),
            "the status spinner is missing: {screen}"
        );
    }
}
