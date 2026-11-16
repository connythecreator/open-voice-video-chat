use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use directories::ProjectDirs;
use ovvc_client::{
    Attachment, AttachmentId, AuthorId, ChannelId, ChannelKind, ChannelMeta, ClientError,
    ClientEvent, Diagnostic, DirectConversation, ErrorClass, EventKind, GuildId, GuildMeta, Invite,
    MAX_ATTACHMENT_BYTES, MAX_PROFILE_ABOUT_BYTES, MAX_PROFILE_LINK_BYTES, MAX_PROFILE_LINKS,
    MAX_PROFILE_PRONOUNS_BYTES, MemberProfile, Multiaddr, NetworkOptions, SignedEvent,
    TimelineEntry, direct_guild_id, hash_bytes, human_size, parse_mentions, peer_id_for_author,
    timeline,
};
use ovvc_config::{
    Config, ConfigLayer, ConfigPaths, LoadedConfig, Overrides, init as config_init,
    load as config_load,
};
use ovvc_control::{CallTopology, PresenceStatus, VoiceMember, VoiceMemberState, VoiceSnapshot};
use ovvc_ipc::{
    DaemonCapabilities, IPC_PROTOCOL, IpcError, PolicyView, VoiceAudioStats, daemon_log_path,
    default_socket_path, read_daemon_info,
};
use ovvc_remote::{ConnectOptions, RemoteClient};
use ovvc_theme::{Preset, Theme};
use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast;

/// Output that tolerates a closed pipe. `println!` panics on EPIPE, so
/// `ovvc ... | head` would abort with a backtrace; these helpers ignore the
/// write error instead, like every other Unix filter.
macro_rules! outln {
    ($($arg:tt)*) => {{
        let _ = writeln!(io::stdout(), $($arg)*);
    }};
}

macro_rules! errln {
    ($($arg:tt)*) => {{
        let _ = writeln!(io::stderr(), $($arg)*);
    }};
}

macro_rules! outp {
    ($($arg:tt)*) => {{
        let mut stdout = io::stdout();
        let _ = write!(stdout, $($arg)*);
        let _ = stdout.flush();
    }};
}

/// Render untrusted text so it cannot drive the terminal or forge a log line.
///
/// Guild, channel and member names and message bodies come from peers; a raw
/// escape sequence here would let a sender set the window title, write the
/// clipboard, clear the screen, or — with an embedded newline — impersonate
/// another user in the transcript. Stored data is validated on import, but
/// this boundary also covers older or hand-edited stores.
fn safe(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() || is_bidi_control(c) {
            out.extend(c.escape_debug());
        } else {
            out.push(c);
        }
    }
    out
}

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

/// Print a classified daemon diagnostic on stderr.
///
/// A bug names the daemon log because that is the actionable part of an
/// internal failure. An operational condition is a warning, not an error, so a
/// peer going offline does not read as an application failure.
fn print_diagnostic(diagnostic: &Diagnostic) {
    match diagnostic.class {
        ErrorClass::Bug => {
            let log = diagnostic
                .log_path
                .as_ref()
                .map(|path| format!(" (log: {})", safe(path)))
                .unwrap_or_default();
            errln!("bug: {}{log}", safe(&diagnostic.message));
        }
        ErrorClass::Operational => errln!("warning: {}", safe(&diagnostic.message)),
        ErrorClass::Security => errln!("security: {}", safe(&diagnostic.message)),
        ErrorClass::User => errln!("error: {}", safe(&diagnostic.message)),
        ErrorClass::Policy => errln!("policy: {}", safe(&diagnostic.message)),
    }
}

/// The taxonomy class for a one-shot command failure.
///
/// A typed client, core or IPC error knows its own class. Anything left is a
/// command-line, file or configuration failure the caller can correct, so it
/// defaults to `User` rather than claiming a bug.
fn error_class(error: &anyhow::Error) -> ErrorClass {
    for cause in error.chain() {
        if let Some(error) = cause.downcast_ref::<ClientError>() {
            return error.class();
        }
        if let Some(error) = cause.downcast_ref::<ovvc_core::CoreError>() {
            return ErrorClass::from_core_error(error);
        }
        if let Some(error) = cause.downcast_ref::<IpcError>() {
            return error.class();
        }
    }
    ErrorClass::User
}

#[derive(Debug, Parser)]
#[command(
    name = "ovvc",
    version,
    about = "A serverless, signed-event chat client"
)]
struct Cli {
    #[arg(long, global = true, env = "OVVC_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Daemon control socket. Defaults to the configuration's `socket`, then
    /// the daemon's discovery record or the per-user runtime path.
    #[arg(long, global = true, env = "OVVC_SOCKET", value_name = "FILE")]
    socket: Option<PathBuf>,

    /// Read configuration from this file instead of the user config file.
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Apply this named profile from <config-dir>/profiles.
    #[arg(long, global = true, env = "OVVC_PROFILE", value_name = "NAME")]
    profile: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create the local identity and data directory.
    Init,
    /// Show, validate or create the layered configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Run local health checks.
    Doctor {
        /// Print a stable JSON object instead of the check lines.
        #[arg(long)]
        json: bool,
    },
    /// Print the local identity.
    Id,
    /// Manage communes.
    #[command(visible_alias = "commune")]
    Guild {
        #[command(subcommand)]
        command: GuildCommand,
    },
    /// Manage channels.
    Channel {
        #[command(subcommand)]
        command: ChannelCommand,
    },
    /// Send a message to a channel.
    Send {
        guild: String,
        channel: String,
        #[arg(required = true, num_args = 1..)]
        message: Vec<String>,
    },
    /// Reply to a message (event id or unique prefix). Mention members with
    /// @name or @id-prefix; they are stored as ids, not text.
    Reply {
        guild: String,
        channel: String,
        target: String,
        #[arg(required = true, num_args = 1..)]
        message: Vec<String>,
    },
    /// Edit one of your own messages (event id or unique prefix).
    Edit {
        guild: String,
        channel: String,
        target: String,
        #[arg(required = true, num_args = 1..)]
        message: Vec<String>,
    },
    /// Delete one of your own messages (event id or unique prefix).
    Delete {
        guild: String,
        channel: String,
        target: String,
    },
    /// Show one member's profile, counts and activity.
    Member {
        guild: String,
        /// Display name, full author id, or a unique prefix.
        member: String,
    },
    /// Set the display name other members see for you in a commune, and
    /// optionally the rest of your profile.
    Profile {
        guild: String,
        name: String,
        /// About text, one paragraph; an empty value clears it.
        #[arg(long, value_name = "TEXT")]
        about: Option<String>,
        /// Pronouns; an empty value clears them.
        #[arg(long, value_name = "TEXT")]
        pronouns: Option<String>,
        /// An http(s) link. Repeatable, up to five; omit to keep the current
        /// links.
        #[arg(long = "link", value_name = "URL")]
        link: Vec<String>,
    },
    /// Open or reuse a direct conversation with a commune member.
    Dm { guild: String, member: String },
    /// List direct conversations.
    Dms,
    /// Leave a commune's roster; local history stays on disk.
    Leave { guild: String },
    /// Show recent messages in a channel.
    History {
        guild: String,
        channel: String,
        #[arg(short, long, default_value_t = 50)]
        limit: usize,
    },
    /// Search a commune's message history.
    Search {
        guild: String,
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },
    /// Set your presence in a commune. Presence expires after 60 seconds unless
    /// something keeps refreshing it, so --hold stays online for a while.
    Presence {
        guild: String,
        /// online, away, dnd or offline.
        status: String,
        /// Keep refreshing for this many seconds, up to an hour. 0 sets it once.
        #[arg(long, default_value_t = 0)]
        hold: u64,
    },
    /// Join a voice call, change your call flags, or read the call state.
    ///
    /// Voice runs through the daemon's control session, so the commune must
    /// have a live control session and the selected channel must be a voice
    /// channel. The daemon owns the audio device; `voice status` also prints
    /// the local audio counters when one is running.
    Voice {
        guild: String,
        #[command(subcommand)]
        command: VoiceCommand,
    },
    /// Show the policy a commune applies to this client.
    ///
    /// Prints the guild policy, the local client floor, the effective policy
    /// for each axis and which axes the floor raised. This is a read model;
    /// none of it is enforced yet.
    Policy { guild: String },
    /// Show or choose the theme preset written to theme.toml.
    Theme {
        /// phosphor, amber, ice or paper. Omit to print the current setting.
        preset: Option<String>,
        #[arg(long)]
        list: bool,
    },
    /// Send a file as an attachment.
    Attach {
        guild: String,
        channel: String,
        path: PathBuf,
        #[arg(num_args = 0..)]
        caption: Vec<String>,
    },
    /// Fetch an attachment into the local filesystem.
    Download {
        guild: String,
        /// Attachment id hex, an id prefix, or the file name shown in history.
        attachment: String,
        /// Channel to resolve a prefix or name in. Defaults to every channel.
        #[arg(long)]
        channel: Option<String>,
        /// Where to write the file. Defaults to its announced name in the
        /// current directory.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },
    /// Open a chat session. Add --listen/--dial for peer-to-peer mode.
    Chat {
        guild: String,
        channel: Option<String>,
        /// Listen for peers on a multiaddr. Repeatable.
        #[arg(long = "listen", value_name = "MULTIADDR")]
        listen: Vec<String>,
        /// Dial a peer multiaddr. Repeatable. Also seeds the DHT.
        #[arg(long = "dial", value_name = "MULTIADDR")]
        dial: Vec<String>,
        /// Look up an author id (hex) in the DHT and dial it. Repeatable.
        #[arg(long = "find", value_name = "AUTHOR_ID")]
        find: Vec<String>,
        /// Try to map a public port with UPnP IGD.
        #[arg(long)]
        upnp: bool,
        /// Try to map a public port with NAT-PMP.
        #[arg(long)]
        natpmp: bool,
        /// Relay multiaddr for NAT traversal and hole punching.
        #[arg(long, value_name = "MULTIADDR")]
        relay: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Print the system, user and profiles paths and whether they exist.
    Path,
    /// Load the layered configuration and print it with its provenance.
    Show {
        /// Print the effective configuration as JSON.
        #[arg(long)]
        json: bool,
        /// Print secrets such as `media.turn_credential` instead of masking
        /// them. The default masks them so they cannot leak into scrollback or
        /// a pasted bug report.
        #[arg(long)]
        reveal_secrets: bool,
    },
    /// Load and validate every layer, naming each source.
    Validate,
    /// Write a default configuration file.
    Init {
        /// Replace an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Print only values that differ from the built-in defaults.
    Diff {
        /// Print a secret value such as `media.turn_credential` instead of
        /// masking it.
        #[arg(long)]
        reveal_secrets: bool,
    },
}

#[derive(Debug, Subcommand)]
enum VoiceCommand {
    /// Join the call on a voice channel.
    Join {
        /// Voice channel name, full channel id, or a unique prefix.
        channel: String,
    },
    /// Leave the call on a voice channel.
    Leave {
        /// Voice channel name, full channel id, or a unique prefix.
        channel: String,
    },
    /// Mute or unmute yourself in a call.
    Mute {
        /// Voice channel name, full channel id, or a unique prefix.
        channel: String,
        /// on/true mutes, off/false unmutes.
        #[arg(value_name = "on|off", action = clap::ArgAction::Set, value_parser = clap::builder::BoolishValueParser::new())]
        on: bool,
    },
    /// Deafen or undeafen yourself in a call.
    Deafen {
        /// Voice channel name, full channel id, or a unique prefix.
        channel: String,
        /// on/true deafens, off/false undeafens.
        #[arg(value_name = "on|off", action = clap::ArgAction::Set, value_parser = clap::builder::BoolishValueParser::new())]
        on: bool,
    },
    /// Turn push-to-talk on or off in a call.
    Ptt {
        /// Voice channel name, full channel id, or a unique prefix.
        channel: String,
        /// on/true enables push-to-talk, off/false disables it.
        #[arg(value_name = "on|off", action = clap::ArgAction::Set, value_parser = clap::builder::BoolishValueParser::new())]
        on: bool,
    },
    /// Show members, topology, coordinators and audio counters.
    Status {
        /// Voice channel name, full channel id, or a unique prefix.
        channel: String,
    },
}

#[derive(Debug, Subcommand)]
enum GuildCommand {
    /// Create a commune with a default #general channel.
    Create { name: String },
    /// List local communes.
    List,
    /// Show commune details.
    Show { guild: String },
    /// Create a signed invite for a commune.
    Invite {
        guild: String,
        /// Peer multiaddr to include in the invite. Repeatable.
        #[arg(long = "peer", value_name = "MULTIADDR", required = true)]
        peer: Vec<String>,
        /// Optional expiry in seconds from now.
        #[arg(long)]
        expires_in: Option<u64>,
    },
    /// Join an existing commune using a signed invite.
    Join { invite: String },
}

#[derive(Debug, Subcommand)]
enum ChannelCommand {
    /// Create a channel in a commune.
    Create { guild: String, name: String },
    /// List channels in a commune.
    List { guild: String },
}

fn main() {
    if let Err(error) = run() {
        errln!("{}: {error:#}", error_class(&error));
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let globals = Globals {
        data_dir: cli.data_dir,
        socket: cli.socket,
        config: cli.config,
        profile: cli.profile,
    };

    match cli.command {
        Command::Config { command } => config_command(&globals, command),
        Command::Doctor { json } => doctor_command(&globals, json),
        command => {
            // Load once, then reuse the configuration for the whole command,
            // so every subcommand sees the same data directory and socket.
            let loaded = load_effective_config(&globals)?;
            let data_dir = resolve_data_dir(&globals, &loaded.config)?;
            let session = Session::new(
                data_dir,
                &loaded.config,
                globals.config.clone(),
                globals.profile.clone(),
            );
            run_command(&session, command)
        }
    }
}

fn run_command(session: &Session, command: Command) -> Result<()> {
    match command {
        Command::Config { .. } | Command::Doctor { .. } => {
            bail!("configuration commands are handled before the configuration is loaded")
        }
        Command::Init => init(session),
        Command::Id => {
            let client = session.open()?;
            outln!("{}", client.public_key_hex());
            outln!("short: {}", client.author_id().short());
            if let Some(peer) = peer_id_for_author(client.author_id()) {
                outln!("peer: {peer}");
            }
            Ok(())
        }
        Command::Guild { command } => guild_command(session, command),
        Command::Channel { command } => channel_command(session, command),
        Command::Send {
            guild,
            channel,
            message,
        } => {
            let client = session.open()?;
            let guild = match resolve_guild_target(&client, &guild)? {
                GuildTarget::Ready(guild) => guild,
                GuildTarget::DirectSyncing(conversation) => {
                    bail!("{}", direct_syncing_message(&conversation))
                }
            };
            let channel = client.find_channel(guild.id, &channel)?;
            let body = message.join(" ");
            let mentions = mentions_for(&client, guild.id, &body)?;
            let event = client.post_message_full(guild.id, channel.id, &body, None, &mentions)?;
            outln!("sent {}", event.id()?);
            Ok(())
        }
        Command::Reply {
            guild,
            channel,
            target,
            message,
        } => {
            let client = session.open()?;
            let guild = client.find_guild(&guild)?;
            let channel = client.find_channel(guild.id, &channel)?;
            let target = resolve_message(&client, guild.id, channel.id, &target)?;
            let body = message.join(" ");
            let mentions = mentions_for(&client, guild.id, &body)?;
            let event = client.post_message_full(
                guild.id,
                channel.id,
                &body,
                Some(target.id()?),
                &mentions,
            )?;
            outln!("replied {}", event.id()?);
            Ok(())
        }
        Command::Edit {
            guild,
            channel,
            target,
            message,
        } => {
            let client = session.open()?;
            let guild = client.find_guild(&guild)?;
            let channel = client.find_channel(guild.id, &channel)?;
            let target = resolve_message(&client, guild.id, channel.id, &target)?;
            let body = message.join(" ");
            let event = client.edit_message(guild.id, target.id()?, &body)?;
            outln!("edited {}", event.id()?);
            Ok(())
        }
        Command::Delete {
            guild,
            channel,
            target,
        } => {
            let client = session.open()?;
            let guild = client.find_guild(&guild)?;
            let channel = client.find_channel(guild.id, &channel)?;
            let target = resolve_message(&client, guild.id, channel.id, &target)?;
            client.delete_message(guild.id, target.id()?)?;
            outln!("deleted {}", target.id()?);
            Ok(())
        }
        Command::Member { guild, member } => {
            let client = session.open()?;
            let guild = client.find_guild(&guild)?;
            let author = resolve_member(&client, guild.id, &member)?;
            let profile = client.member_profile(guild.id, author)?;
            for line in profile_lines(&profile, client.author_id()) {
                outln!("{line}");
            }
            Ok(())
        }
        Command::Profile {
            guild,
            name,
            about,
            pronouns,
            link,
        } => {
            let client = session.open()?;
            let guild = client.find_guild(&guild)?;
            let touched = about.is_some() || pronouns.is_some() || !link.is_empty();
            // A member that has just joined may not be on the local roster
            // yet. Naming them is the event that adds the row, so the existing
            // positional-only form must not require a profile first. With
            // flags, missing fields inherit from the current profile when one
            // exists and stay empty otherwise.
            let current = if touched {
                client.member_profile(guild.id, client.author_id()).ok()
            } else {
                None
            };
            let about = about.unwrap_or_else(|| {
                current
                    .as_ref()
                    .map(|profile| profile.about.clone())
                    .unwrap_or_default()
            });
            let pronouns = pronouns.unwrap_or_else(|| {
                current
                    .as_ref()
                    .map(|profile| profile.pronouns.clone())
                    .unwrap_or_default()
            });
            let links = if link.is_empty() {
                current.map(|profile| profile.links).unwrap_or_default()
            } else {
                link
            };
            // Validate the complete profile before the first write, so a bad
            // flag cannot leave the display name changed and the profile not.
            if touched {
                validate_profile_fields(&about, &pronouns, &links)?;
            }
            client.set_display_name(guild.id, &name)?;
            outln!("display name set to {}", safe(&name));
            if touched {
                client.set_profile(guild.id, &about, &pronouns, &links)?;
                outln!(
                    "profile updated: about {} byte(s), pronouns {}, {} link(s)",
                    about.len(),
                    if pronouns.is_empty() {
                        "none".to_string()
                    } else {
                        safe(&pronouns)
                    },
                    links.len()
                );
            }
            Ok(())
        }
        Command::Dm { guild, member } => {
            let client = session.open()?;
            let guild = client.find_guild(&guild)?;
            let peer = resolve_member(&client, guild.id, &member)?;
            if peer == client.author_id() {
                bail!("cannot open a direct message with yourself");
            }
            let peer_name = member_name(&client, guild.id, peer)?;
            let dm_id = direct_guild_id(&client.author_id(), &peer);
            // A known conversation (a local guild, or just the announcement
            // the recipient imported) is reused rather than opened a second
            // time. `open_direct_message` is still called: it is idempotent,
            // never founds a second log for a DM the peer opened, and it is
            // what starts or reconnects the DM node. Without the call, a
            // daemon restart left an announced conversation listed but its log
            // permanently unsynced.
            let known = client.get_guild(dm_id)?.is_some()
                || client
                    .list_direct_messages()?
                    .into_iter()
                    .any(|conversation| conversation.guild.id == dm_id);
            let dm = client.open_direct_message(guild.id, peer)?;
            if known {
                outln!("direct message with {} already exists", safe(&peer_name));
            } else {
                outln!("opened direct message with {}", safe(&peer_name));
            }
            outln!("id: {}", dm.id);
            outln!("name: {}", safe(&dm.name));
            Ok(())
        }
        Command::Dms => {
            let client = session.open()?;
            let conversations = client.list_direct_messages()?;
            if conversations.is_empty() {
                outln!("no direct messages");
                return Ok(());
            }
            for conversation in conversations {
                outln!(
                    "{}  dm:{}  <{}>",
                    conversation.guild.id.short(),
                    safe(&conversation.peer.display_name),
                    conversation.peer.author.short()
                );
            }
            Ok(())
        }
        Command::Leave { guild } => {
            let client = session.open()?;
            let guild = client.find_guild(&guild)?;
            client.leave_guild(guild.id)?;
            outln!("left commune {}", guild.id.short());
            Ok(())
        }
        Command::History {
            guild,
            channel,
            limit,
        } => {
            let client = session.open()?;
            let guild = match resolve_guild_target(&client, &guild)? {
                GuildTarget::Ready(guild) => guild,
                GuildTarget::DirectSyncing(conversation) => {
                    bail!("{}", direct_syncing_message(&conversation))
                }
            };
            let channel = client.find_channel(guild.id, &channel)?;
            print_history(&client, guild.id, channel.id, limit)
        }
        Command::Search {
            guild,
            query,
            limit,
        } => {
            let client = session.open()?;
            let guild = client.find_guild(&guild)?;
            let query = query.join(" ");
            let hits = client.search(guild.id, &query, limit)?;
            if hits.is_empty() {
                outln!("no matches");
                return Ok(());
            }
            let channels = client.list_channel_views(guild.id)?;
            for hit in &hits {
                let channel = channels
                    .iter()
                    .find(|channel| channel.id == hit.channel_id)
                    .map(|channel| format!("{}{}", channel.kind.glyph(), safe(&channel.name)))
                    .unwrap_or_else(|| hit.channel_id.short());
                outln!(
                    "{}  {}  <{}>  {}",
                    message_id(&hit.event),
                    channel,
                    hit.event.body.author.short(),
                    safe(&preview(&hit.body, 120))
                );
            }
            Ok(())
        }
        Command::Presence {
            guild,
            status,
            hold,
        } => {
            let client = session.open()?;
            let guild = client.find_guild(&guild)?;
            let Some(status) = presence_from_name(&status) else {
                bail!("unknown presence {status:?}; try online, away, dnd or offline");
            };
            client.set_presence(guild.id, status)?;
            if hold == 0 || status == PresenceStatus::Offline {
                outln!(
                    "presence {} (visible for up to 60 seconds unless refreshed)",
                    presence_name(status)
                );
                return Ok(());
            }
            outln!(
                "presence {} for {}s; ctrl-c stops refreshing",
                presence_name(status),
                hold.min(MAX_PRESENCE_HOLD_SECS)
            );
            if hold > MAX_PRESENCE_HOLD_SECS {
                outln!(
                    "  holding for at most {}s, the longest refresh this command supports",
                    MAX_PRESENCE_HOLD_SECS
                );
            }
            let deadline = std::time::Instant::now()
                + std::time::Duration::from_secs(hold.min(MAX_PRESENCE_HOLD_SECS));
            let mut refresh_error = None;
            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                std::thread::sleep(remaining.min(std::time::Duration::from_secs(20)));
                if std::time::Instant::now() >= deadline {
                    break;
                }
                if let Err(error) = client.set_presence(guild.id, status) {
                    refresh_error = Some(error);
                    break;
                }
            }
            // The frame expires on its own after a minute, but clearing it now
            // means a peer does not show this author online after the hold ran
            // out. That matters most after a failed refresh, when the frame is
            // the only thing still saying otherwise.
            let cleared = client.set_presence(guild.id, PresenceStatus::Offline);
            if let Some(error) = refresh_error {
                return Err(anyhow::Error::new(error).context("refreshing presence"));
            }
            cleared.context("clearing presence")?;
            outln!("presence offline");
            Ok(())
        }
        Command::Voice { guild, command } => voice_command(session, &guild, command),
        Command::Policy { guild } => policy_command(session, &guild),
        Command::Theme { preset, list } => theme_command(preset, list),
        Command::Attach {
            guild,
            channel,
            path,
            caption,
        } => {
            let client = session.open()?;
            let guild = client.find_guild(&guild)?;
            let channel = client.find_channel(guild.id, &channel)?;
            let metadata =
                std::fs::metadata(&path).with_context(|| format!("reading {}", path.display()))?;
            if !metadata.is_file() {
                bail!("{} is not a regular file", path.display());
            }
            if metadata.len() > MAX_ATTACHMENT_BYTES {
                bail!(
                    "{} is {}, over the {} limit",
                    path.display(),
                    human_size(metadata.len()),
                    human_size(MAX_ATTACHMENT_BYTES)
                );
            }
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
                bail!(
                    "{} grew past the {} limit while it was being read",
                    path.display(),
                    human_size(MAX_ATTACHMENT_BYTES)
                );
            }
            let attachment = ovvc_core::attachment_for_file(&path, &bytes)?;
            let stored = client.put_blob(&bytes)?;
            if stored != attachment.hash {
                bail!(
                    "the daemon stored the file as {stored}, expected {}",
                    attachment.hash
                );
            }
            let body = caption.join(" ");
            let mentions = mentions_for(&client, guild.id, &body)?;
            let event = client.post_message_rich(
                guild.id,
                channel.id,
                &body,
                None,
                &mentions,
                std::slice::from_ref(&attachment),
            )?;
            outln!("sent {} with {}", event.id()?, safe(&attachment.name));
            outln!(
                "  {}  {}  {}",
                human_size(attachment.size),
                attachment.mime,
                attachment.hash
            );
            Ok(())
        }
        Command::Download {
            guild,
            attachment,
            channel,
            out,
        } => {
            let client = session.open()?;
            let guild = client.find_guild(&guild)?;
            let attachment =
                resolve_attachment(&client, guild.id, &attachment, channel.as_deref())?;
            let bytes = client.fetch_blob(guild.id, attachment.hash)?;
            if hash_bytes(&bytes) != attachment.hash {
                bail!(
                    "the bytes the daemon returned do not match {}",
                    attachment.hash
                );
            }
            let target = out.unwrap_or_else(|| PathBuf::from(safe_download_name(&attachment.name)));
            if let Some(parent) = target.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            // `create_new` refuses an existing file and creates the new one in
            // the same step, so two downloads racing for one name cannot both
            // win, and a partial write is removed instead of looking complete.
            let mut file = match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    bail!(
                        "{} already exists; pass --out to write somewhere else",
                        target.display()
                    );
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("creating {}", target.display()));
                }
            };
            if let Err(error) = file.write_all(&bytes) {
                drop(file);
                let _ = std::fs::remove_file(&target);
                return Err(error).with_context(|| format!("writing {}", target.display()));
            }
            outln!(
                "saved {} ({}) to {}",
                safe(&attachment.name),
                human_size(bytes.len() as u64),
                target.display()
            );
            Ok(())
        }
        Command::Chat {
            guild,
            channel,
            listen,
            dial,
            find,
            upnp,
            natpmp,
            relay,
        } => {
            let client = session.open()?;
            let guild = client.find_guild(&guild)?;
            let channel = match channel {
                Some(channel) => client.find_channel(guild.id, &channel)?,
                None => client.default_channel(guild.id)?,
            };

            if listen.is_empty() && dial.is_empty() && find.is_empty() && relay.is_none() {
                local_chat(&client, guild, channel)
            } else {
                let options = NetworkOptions {
                    listen: parse_addrs(&listen)?,
                    dial: parse_addrs(&dial)?,
                    relay: relay
                        .map(|value| {
                            value
                                .parse::<Multiaddr>()
                                .with_context(|| format!("invalid relay multiaddr: {value}"))
                        })
                        .transpose()?,
                    upnp,
                    natpmp,
                };
                let find = find
                    .iter()
                    .map(|value| {
                        value
                            .parse::<AuthorId>()
                            .with_context(|| format!("invalid author id: {value}"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                tokio_runtime()?.block_on(networked_chat(
                    Arc::new(client),
                    guild,
                    channel,
                    options,
                    find,
                ))
            }
        }
    }
}

fn guild_command(session: &Session, command: GuildCommand) -> Result<()> {
    let client = session.open()?;
    match command {
        GuildCommand::Create { name } => {
            let guild = client.create_guild(&name)?;
            outln!("created commune {}", guild.id);
            outln!("name: {}", safe(&guild.name));
            for channel in client.list_channels(guild.id)? {
                outln!("default channel: #{} ({})", safe(&channel.name), channel.id);
            }
            Ok(())
        }
        GuildCommand::List => {
            let guilds = client.list_guilds()?;
            if guilds.is_empty() {
                outln!("no communes");
                return Ok(());
            }
            for guild in guilds {
                outln!("{}  {}", guild.id.short(), safe(&guild.name));
            }
            Ok(())
        }
        GuildCommand::Show { guild } => {
            let guild = client.find_guild(&guild)?;
            print_guild(&client, &guild)
        }
        GuildCommand::Invite {
            guild,
            peer,
            expires_in,
        } => {
            let guild = client.find_guild(&guild)?;
            for address in &peer {
                let address = address
                    .parse::<Multiaddr>()
                    .with_context(|| format!("invalid peer multiaddr: {address}"))?;
                if !address.to_string().contains("/p2p/") {
                    bail!("peer multiaddr must include /p2p/<peer-id>: {address}");
                }
            }
            let invite = client.create_invite(guild.id, peer, expires_in)?;
            outln!("{}", invite.encode()?);
            Ok(())
        }
        GuildCommand::Join { invite } => {
            let invite = Invite::decode(&invite)?;
            let guild = client.join_guild(&invite, NetworkOptions::default())?;
            outln!("joined commune {} ({})", safe(&guild.name), guild.id);
            for channel in client.list_channels(guild.id)? {
                outln!("#{}  {}", safe(&channel.name), channel.id);
            }
            Ok(())
        }
    }
}

fn channel_command(session: &Session, command: ChannelCommand) -> Result<()> {
    let client = session.open()?;
    match command {
        ChannelCommand::Create { guild, name } => {
            let guild = client.find_guild(&guild)?;
            let channel = client.create_channel(guild.id, &name)?;
            outln!("created #{} ({})", safe(&channel.name), channel.id);
            Ok(())
        }
        ChannelCommand::List { guild } => {
            let guild = client.find_guild(&guild)?;
            print_channels(&client, guild.id)
        }
    }
}

/// The global arguments shared by every command.
struct Globals {
    data_dir: Option<PathBuf>,
    socket: Option<PathBuf>,
    config: Option<PathBuf>,
    profile: Option<String>,
}

impl Globals {
    fn paths(&self) -> Result<ConfigPaths> {
        ConfigPaths::discover().context("locating configuration")
    }

    fn overrides(&self) -> Overrides {
        Overrides {
            config: self.config.clone(),
            profile: self.profile.clone(),
            data_dir: self.data_dir.clone(),
            socket: self.socket.clone(),
            theme: None,
        }
    }
}

/// Everything a command needs to reach the daemon.
struct Session {
    data_dir: PathBuf,
    connect: ConnectOptions,
}

impl Session {
    fn new(
        data_dir: PathBuf,
        config: &Config,
        config_path: Option<PathBuf>,
        profile: Option<String>,
    ) -> Self {
        let connect = ConnectOptions {
            socket: config.socket.clone(),
            // A daemon this command starts must read the same layered
            // configuration and profile the command itself did, otherwise
            // `--config` would silently stop applying once the daemon is
            // running.
            config: config_path,
            profile,
            ..ConnectOptions::default()
        };
        Self { data_dir, connect }
    }

    fn open(&self) -> Result<RemoteClient> {
        let path = self.data_dir.join("identity.key");
        if !path.exists() {
            bail!(
                "identity not found; run `ovvc --data-dir {} init`",
                self.data_dir.display()
            );
        }
        RemoteClient::attach(&self.data_dir, self.connect.clone())
            .context("attaching to the local daemon")
    }
}

fn load_effective_config(globals: &Globals) -> Result<LoadedConfig> {
    let paths = globals.paths()?;
    config_load(&paths, globals.overrides()).context("loading configuration")
}

/// CLI --data-dir, then the configuration, then the per-user default.
fn resolve_data_dir(globals: &Globals, config: &Config) -> Result<PathBuf> {
    if let Some(path) = &globals.data_dir {
        return Ok(path.clone());
    }
    if let Some(path) = &config.data_dir {
        return Ok(path.clone());
    }
    ProjectDirs::from("org", "ovvc", "ovvc")
        .map(|dirs| dirs.data_dir().to_path_buf())
        .context("could not determine a data directory; pass --data-dir")
}

fn init(session: &Session) -> Result<()> {
    let existed = session.data_dir.join("identity.key").exists();
    // The daemon creates the identity when it opens a fresh data directory.
    // The CLI never opens the store itself, so `init` starts (or joins) the
    // daemon and reads the identity back over the socket.
    let client = RemoteClient::attach(&session.data_dir, session.connect.clone())
        .with_context(|| format!("initializing data directory {}", session.data_dir.display()))?;

    if existed {
        outln!("already initialized");
    } else {
        outln!("initialized ovvc");
    }
    outln!("data: {}", session.data_dir.display());
    outln!("identity: {}", client.public_key_hex());
    Ok(())
}

fn presence(exists: bool) -> &'static str {
    if exists { "present" } else { "missing" }
}

fn layer_name(layer: ConfigLayer) -> &'static str {
    match layer {
        ConfigLayer::System => "system",
        ConfigLayer::SystemDropin => "dropin",
        ConfigLayer::User => "user",
        ConfigLayer::Profile => "profile",
        ConfigLayer::Explicit => "explicit",
    }
}

fn profile_chain(chain: &[String]) -> String {
    if chain.is_empty() {
        "default".to_string()
    } else {
        chain.join(" -> ")
    }
}

fn print_sources(sources: &[ovvc_config::ConfigSource]) {
    outln!("sources:");
    for source in sources {
        outln!(
            "  {:<8} {} ({})",
            layer_name(source.layer),
            source.path.display(),
            presence(source.present)
        );
    }
}

fn config_command(globals: &Globals, command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Path => config_path(globals),
        ConfigCommand::Show {
            json,
            reveal_secrets,
        } => config_show(globals, json, reveal_secrets),
        ConfigCommand::Validate => config_validate(globals),
        ConfigCommand::Init { force } => config_write(globals, force),
        ConfigCommand::Diff { reveal_secrets } => config_diff(globals, reveal_secrets),
    }
}

fn config_path(globals: &Globals) -> Result<()> {
    let paths = globals.paths()?;
    outln!(
        "system:   {} ({})",
        paths.system.display(),
        presence(paths.system.is_file())
    );
    outln!(
        "drop-ins: {} ({})",
        paths.system_dropins.display(),
        presence(paths.system_dropins.is_dir())
    );
    if let Some(explicit) = &globals.config {
        outln!(
            "explicit: {} ({})",
            explicit.display(),
            presence(explicit.is_file())
        );
    }
    outln!(
        "user:     {} ({})",
        paths.user.display(),
        presence(paths.user.is_file())
    );
    outln!(
        "profiles: {} ({})",
        paths.profiles.display(),
        presence(paths.profiles.is_dir())
    );
    Ok(())
}

/// What a masked secret is replaced with in `config show` and `config diff`.
const MASKED_SECRET: &str = "***";

/// The only secret in the configuration today. It is a bearer credential for
/// the configured TURN server.
const TURN_CREDENTIAL_KEY: &str = "media.turn_credential";

/// Mask `media.turn_credential` before a configuration is printed.
///
/// `config show` is routinely pasted into a bug report or left in terminal
/// scrollback, and the TURN credential is a bearer secret, so it is redacted
/// unless the caller asks for it with `--reveal-secrets`.
fn mask_secrets(config: &mut Config) {
    if config.media.turn_credential.is_some() {
        config.media.turn_credential = Some(MASKED_SECRET.to_string());
    }
}

fn config_show(globals: &Globals, json: bool, reveal_secrets: bool) -> Result<()> {
    let loaded = load_effective_config(globals)?;
    let mut config = loaded.config.clone();
    if !reveal_secrets {
        mask_secrets(&mut config);
    }
    if json {
        outln!(
            "{}",
            serde_json::to_string_pretty(&config).context("rendering the configuration as JSON")?
        );
        return Ok(());
    }
    outp!("{}", config.to_toml()?);
    outln!();
    print_sources(&loaded.sources);
    outln!("profile chain: {}", profile_chain(&loaded.profile_chain));
    Ok(())
}

fn config_validate(globals: &Globals) -> Result<()> {
    let paths = globals.paths()?;
    // Record the files considered before loading, so a parse or validation
    // error still says where ovvc looked.
    let primary = match &globals.config {
        Some(path) => ovvc_config::ConfigSource {
            path: path.clone(),
            layer: ConfigLayer::Explicit,
            present: path.is_file(),
        },
        None => ovvc_config::ConfigSource {
            path: paths.user.clone(),
            layer: ConfigLayer::User,
            present: paths.user.is_file(),
        },
    };
    let mut sources = vec![ovvc_config::ConfigSource {
        path: paths.system.clone(),
        layer: ConfigLayer::System,
        present: paths.system.is_file(),
    }];
    // Drop-ins sit between the system file and the user file, so name them here
    // too; otherwise a parse error introduced by one points at the user file
    // and the drop-in that caused it never appears.
    match ovvc_config::system_dropin_files(&paths.system_dropins) {
        Ok(dropins) => {
            sources.extend(dropins.into_iter().map(|path| ovvc_config::ConfigSource {
                path,
                layer: ConfigLayer::SystemDropin,
                present: true,
            }));
        }
        Err(error) => outln!("  drop-ins: {error}"),
    }
    sources.push(primary);
    print_sources(&sources);
    let loaded = config_load(&paths, globals.overrides()).context("validating configuration")?;
    for source in loaded
        .sources
        .iter()
        .filter(|source| source.layer == ConfigLayer::Profile)
    {
        outln!(
            "  {:<8} {} ({})",
            layer_name(source.layer),
            source.path.display(),
            presence(source.present)
        );
    }
    outln!("profile chain: {}", profile_chain(&loaded.profile_chain));
    outln!("configuration ok");
    Ok(())
}

fn config_write(globals: &Globals, force: bool) -> Result<()> {
    let target = match &globals.config {
        Some(path) => path.clone(),
        None => globals.paths()?.user,
    };
    let written = config_init(&target, force).map_err(anyhow::Error::new)?;
    outln!("wrote {}", written.display());
    Ok(())
}

fn config_diff(globals: &Globals, reveal_secrets: bool) -> Result<()> {
    let loaded = load_effective_config(globals)?;
    let mut diff = loaded.diff();
    if !reveal_secrets && diff.contains_key(TURN_CREDENTIAL_KEY) {
        diff.insert(TURN_CREDENTIAL_KEY.to_string(), MASKED_SECRET.to_string());
    }
    if diff.is_empty() {
        outln!("no differences from the built-in defaults");
        return Ok(());
    }
    for (key, value) in diff {
        outln!("{key} = {value}");
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct DoctorCheck {
    name: String,
    level: &'static str,
    detail: String,
}

#[derive(Debug, Serialize)]
struct DoctorSource {
    path: String,
    layer: &'static str,
    present: bool,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    ok: bool,
    checks: Vec<DoctorCheck>,
    config_sources: Vec<DoctorSource>,
    /// The daemon's runtime report, absent when no daemon was reachable or it
    /// predates the capability op.
    capabilities: Option<DaemonCapabilities>,
    /// Where the daemon writes its diagnostic log, so a bug report can name
    /// the file even when the daemon was not reachable.
    log_path: Option<String>,
}

fn check(name: &str, level: &'static str, detail: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        name: name.to_string(),
        level,
        detail: detail.into(),
    }
}

fn doctor_sources(sources: &[ovvc_config::ConfigSource]) -> Vec<DoctorSource> {
    sources
        .iter()
        .map(|source| DoctorSource {
            path: source.path.display().to_string(),
            layer: layer_name(source.layer),
            present: source.present,
        })
        .collect()
}

fn doctor_command(globals: &Globals, json: bool) -> Result<()> {
    let mut checks = Vec::new();
    let mut sources = Vec::new();
    let loaded = match globals.paths() {
        Ok(paths) => match config_load(&paths, globals.overrides()) {
            Ok(loaded) => {
                checks.push(check(
                    "configuration",
                    "ok",
                    format!("loaded and validated profile {:?}", loaded.config.profile),
                ));
                sources = doctor_sources(&loaded.sources);
                Some(loaded)
            }
            Err(error) => {
                checks.push(check("configuration", "fail", format!("{error}")));
                let primary = match &globals.config {
                    Some(path) => ovvc_config::ConfigSource {
                        path: path.clone(),
                        layer: ConfigLayer::Explicit,
                        present: path.is_file(),
                    },
                    None => ovvc_config::ConfigSource {
                        path: paths.user.clone(),
                        layer: ConfigLayer::User,
                        present: paths.user.is_file(),
                    },
                };
                sources = doctor_sources(&[
                    ovvc_config::ConfigSource {
                        path: paths.system.clone(),
                        layer: ConfigLayer::System,
                        present: paths.system.is_file(),
                    },
                    primary,
                ]);
                None
            }
        },
        Err(error) => {
            checks.push(check("configuration", "fail", format!("{error:#}")));
            None
        }
    };
    let config = loaded.as_ref().map(|loaded| &loaded.config);

    let data_dir = globals
        .data_dir
        .clone()
        .or_else(|| config.and_then(|config| config.data_dir.clone()))
        .or_else(|| {
            ProjectDirs::from("org", "ovvc", "ovvc").map(|dirs| dirs.data_dir().to_path_buf())
        });
    let data_dir = match data_dir {
        Some(dir) => {
            let existed = dir.exists();
            match check_data_dir(&dir) {
                Ok(()) if existed => checks.push(check(
                    "data-dir",
                    "ok",
                    format!("{} exists and is writable", dir.display()),
                )),
                Ok(()) => checks.push(check(
                    "data-dir",
                    "ok",
                    format!("{} can be created", dir.display()),
                )),
                Err(detail) => checks.push(check("data-dir", "fail", detail)),
            }
            Some(dir)
        }
        None => {
            checks.push(check(
                "data-dir",
                "fail",
                "could not determine a data directory; pass --data-dir",
            ));
            None
        }
    };

    // Prefer the socket the running daemon recorded in daemon.json. A daemon
    // started with an explicit `--socket` that the config does not name is
    // still discoverable through that record, and the CLI, TUI and GUI all
    // attach through it. Fall back to the config and then the derived default
    // only when no record exists.
    let recorded_socket = data_dir
        .as_ref()
        .and_then(|dir| read_daemon_info(dir).ok().flatten())
        .map(|info| PathBuf::from(info.socket));
    let socket = recorded_socket
        .or_else(|| config.and_then(|config| config.socket.clone()))
        .or_else(|| data_dir.as_ref().map(|dir| default_socket_path(dir)));
    match &socket {
        Some(socket) => match socket.parent() {
            Some(parent) => match check_data_dir(parent) {
                Ok(()) if parent.exists() => checks.push(check(
                    "socket",
                    "ok",
                    format!("{} is writable", parent.display()),
                )),
                Ok(()) => checks.push(check(
                    "socket",
                    "ok",
                    format!("{} can be created", parent.display()),
                )),
                Err(detail) => checks.push(check("socket", "fail", detail)),
            },
            None => checks.push(check(
                "socket",
                "warn",
                "socket path has no parent directory",
            )),
        },
        None => checks.push(check(
            "socket",
            "warn",
            "no socket path could be determined",
        )),
    }

    // The runtime report is only available from a reachable daemon. It is
    // filled in below and used instead of probing PATH or /dev/snd.
    let mut daemon_capabilities: Option<DaemonCapabilities> = None;
    // Fall back to the path the daemon would use, so `doctor` can name the log
    // even before the daemon has written one.
    let mut daemon_log = data_dir.as_ref().map(|dir| daemon_log_path(dir));

    // A daemon check never fails: a CLI-only install has no daemon, and a
    // daemon that is merely stopped is a warning rather than a broken setup.
    if let Some(dir) = &data_dir {
        let identity = dir.join("identity.key").exists();
        let info = read_daemon_info(dir);
        if !identity {
            checks.push(check("daemon", "warn", "no identity yet; run `ovvc init`"));
        } else {
            match &socket {
                Some(socket) => {
                    let options = ConnectOptions {
                        spawn_daemon: false,
                        online: false,
                        socket: Some(socket.clone()),
                        config: globals.config.clone(),
                        profile: globals.profile.clone(),
                        timeout: std::time::Duration::from_secs(2),
                    };
                    let probe = RemoteClient::attach(dir, options)
                        .map_err(|error| error.to_string())
                        .and_then(|client| {
                            let status =
                                client.daemon_status().map_err(|error| error.to_string())?;
                            let capabilities = client
                                .daemon_capabilities()
                                .map_err(|error| error.to_string())?;
                            Ok((status, capabilities))
                        });
                    match probe {
                        Ok((status, capabilities)) => {
                            daemon_capabilities = capabilities;
                            if !status.log_path.is_empty() {
                                daemon_log = Some(PathBuf::from(&status.log_path));
                            }
                            checks.push(check(
                                "daemon",
                                "ok",
                                format!(
                                    "reachable: version {} on {} (protocol {IPC_PROTOCOL}, log: {})",
                                    status.version,
                                    status.socket,
                                    status.log_path
                                ),
                            ));
                        }
                        Err(error) => {
                            let state = match &info {
                                Ok(Some(info)) => format!("stale record from pid {}", info.pid),
                                Ok(None) => "no discovery record".to_string(),
                                Err(error) => format!("unreadable discovery record: {error}"),
                            };
                            checks.push(check(
                                "daemon",
                                "warn",
                                format!("not reachable at {} ({state}): {error}", socket.display()),
                            ));
                        }
                    }
                }
                None => checks.push(check("daemon", "warn", "no socket path to probe")),
            }
        }
    } else {
        checks.push(check("daemon", "warn", "no data directory to inspect"));
    }

    if let Some(path) = &daemon_log {
        match std::fs::metadata(path) {
            Ok(metadata) => checks.push(check(
                "log",
                "ok",
                format!("{} ({} bytes)", path.display(), metadata.len()),
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => checks.push(check(
                "log",
                "ok",
                format!(
                    "{} (created when the daemon next reports a condition)",
                    path.display()
                ),
            )),
            Err(error) => checks.push(check(
                "log",
                "warn",
                format!("{} is unreadable: {error}", path.display()),
            )),
        }
    }

    match ovvc_theme::theme_path() {
        None => checks.push(check(
            "theme",
            "warn",
            "no theme file path; the built-in phosphor palette is used",
        )),
        Some(path) => match ovvc_theme::load_from(&path) {
            Ok(Some(_)) => checks.push(check("theme", "ok", format!("loaded {}", path.display()))),
            Ok(None) => checks.push(check(
                "theme",
                "ok",
                format!(
                    "{} is absent; the built-in phosphor palette is used",
                    path.display()
                ),
            )),
            Err(error) => checks.push(check("theme", "warn", format!("{error:#}"))),
        },
    }

    let audio_engine = config
        .map(|config| config.audio.engine.as_str())
        .unwrap_or("unknown");
    let audio_backend = config
        .map(|config| config.audio.backend.as_str())
        .unwrap_or("unknown");
    let media_transport = config
        .map(|config| config.media.transport.as_str())
        .unwrap_or("unknown");
    capability_checks(
        &mut checks,
        daemon_capabilities.as_ref(),
        audio_engine,
        audio_backend,
        media_transport,
    );

    let failed = checks.iter().filter(|check| check.level == "fail").count();
    let warned = checks.iter().filter(|check| check.level == "warn").count();

    if json {
        let report = DoctorReport {
            ok: failed == 0,
            checks,
            config_sources: sources,
            capabilities: daemon_capabilities,
            log_path: daemon_log.map(|path| path.display().to_string()),
        };
        outln!(
            "{}",
            serde_json::to_string_pretty(&report).context("rendering doctor output as JSON")?
        );
    } else {
        let profile = config
            .map(|config| config.profile.as_str())
            .unwrap_or("<unavailable>");
        outln!("profile: {profile}");
        outln!("ovvc-config version: {}", env!("CARGO_PKG_VERSION"));
        for check in &checks {
            outln!("{}: {}: {}", check.level, check.name, check.detail);
        }
        if failed == 0 {
            outln!("summary: ok ({warned} warning(s))");
        } else {
            outln!("summary: {failed} failed, {warned} warning(s)");
        }
    }
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Report the daemon's compiled and selected capabilities.
///
/// Audio is never inferred from PATH or `/dev/snd`: a daemon that can answer
/// reports what it compiled, and one that cannot is named as unreported. Video
/// is always described as a descriptor only, because this tree has no live
/// capture, track or screen-share path.
fn capability_checks(
    checks: &mut Vec<DoctorCheck>,
    capabilities: Option<&DaemonCapabilities>,
    engine: &str,
    configured_backend: &str,
    configured_transport: &str,
) {
    match capabilities {
        Some(caps) => {
            // A resolved null backend is the honest answer on a default build,
            // but it also means a call has no microphone or speaker, so it
            // must not read as a healthy audio setup. An `auto` report means
            // the daemon will try PipeWire and silently fall back to null when
            // the server or device cannot open, so it is a warning with the
            // fallback named rather than an unconditional ok.
            let (level, note) = match caps.audio_backend_selected.as_str() {
                "null" => ("warn", "; no live device, calls are silent"),
                "auto" => (
                    "warn",
                    "; auto opens pipewire when the device is reachable and silently falls \
                     back to null otherwise",
                ),
                _ => ("ok", ""),
            };
            checks.push(check(
                "audio",
                level,
                format!(
                    "compiled backends {}; selected {} (configured {}); engine {}; codecs {}{}",
                    join_list(&caps.audio_backends_compiled),
                    caps.audio_backend_selected,
                    configured_backend,
                    engine,
                    join_list(&caps.audio_codecs),
                    note,
                ),
            ));
            checks.push(check(
                "media",
                "ok",
                format!(
                    "compiled transports {}; selected {} (configured {})",
                    join_list(&caps.media_transports_compiled),
                    caps.media_transport_selected,
                    configured_transport,
                ),
            ));
            checks.push(check(
                "features",
                "ok",
                if caps.features.is_empty() {
                    "none".to_string()
                } else {
                    join_list(&caps.features)
                },
            ));
            if caps.video_descriptor_only {
                checks.push(check(
                    "video",
                    "warn",
                    "descriptor only: no live capture, track or screen share in this build",
                ));
            } else {
                checks.push(check("video", "ok", "live video reported by the daemon"));
            }
        }
        None => {
            checks.push(check(
                "audio",
                "warn",
                format!(
                    "daemon did not report compiled backends; configured engine {engine}, backend {configured_backend}"
                ),
            ));
            checks.push(check(
                "media",
                "warn",
                format!(
                    "daemon did not report compiled transports; configured transport {configured_transport}"
                ),
            ));
            checks.push(check(
                "video",
                "warn",
                "descriptor only: no live capture, track or screen share in this build",
            ));
        }
    }
}

fn join_list(values: &[String]) -> String {
    if values.is_empty() {
        "none".to_string()
    } else {
        values.join(", ")
    }
}

fn check_data_dir(dir: &Path) -> std::result::Result<(), String> {
    // Resolve a relative path against this process's working directory once,
    // so the ancestor walk below sees the same path `create_dir_all` would use.
    let absolute = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(dir))
            .unwrap_or_else(|_| dir.to_path_buf())
    };
    if absolute.exists() {
        if !absolute.is_dir() {
            return Err(format!("{} exists but is not a directory", dir.display()));
        }
        return probe_writable(&absolute)
            .map_err(|error| format!("{} is not writable: {error}", dir.display()));
    }
    // The daemon creates the whole chain with `create_dir_all`, so a nested
    // path is fine as long as its nearest existing ancestor is writable.
    let ancestor = absolute
        .ancestors()
        .skip(1)
        .find(|candidate| candidate.exists())
        .ok_or_else(|| format!("{} has no existing ancestor", dir.display()))?;
    if !ancestor.is_dir() {
        return Err(format!(
            "{} is not a directory, so {} cannot be created",
            ancestor.display(),
            dir.display()
        ));
    }
    probe_writable(ancestor).map_err(|error| {
        format!(
            "cannot create {}: {} is not writable: {error}",
            dir.display(),
            ancestor.display()
        )
    })
}

/// Write and remove a uniquely named probe file, which is the only portable
/// way to answer "can this process create a file here?" without trusting
/// permission bits alone.
fn probe_writable(dir: &Path) -> std::io::Result<()> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    let probe = dir.join(format!(".ovvc-doctor-{}-{nonce}", std::process::id()));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

fn tokio_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("creating async runtime")
}

fn print_guild(client: &RemoteClient, guild: &GuildMeta) -> Result<()> {
    outln!("{}", safe(&guild.name));
    outln!("id: {}", guild.id);
    outln!("founder: {}", guild.founder.short());
    outln!("created: {}", format_time(guild.created_at));
    outln!();
    print_channels(client, guild.id)?;
    outln!();
    let members = client.list_members(guild.id)?;
    if members.is_empty() {
        outln!("members: none");
    } else {
        outln!("members:");
        for member in members {
            outln!(
                "  {}  {}",
                member.author.short(),
                safe(&member.display_name)
            );
        }
    }
    Ok(())
}

/// Resolve a member by display name, full author id or unique id prefix.
///
/// A name match wins over an id prefix, and ambiguity is an error rather than
/// a guess, because naming the wrong person changes whose profile is shown or
/// which conversation is opened.
fn resolve_member(client: &RemoteClient, guild_id: GuildId, needle: &str) -> Result<AuthorId> {
    let needle = needle.trim();
    if needle.is_empty() {
        bail!("no member given");
    }
    let members = client.list_members(guild_id)?;
    let by_name: Vec<_> = members
        .iter()
        .filter(|member| member.display_name.eq_ignore_ascii_case(needle))
        .collect();
    match by_name.len() {
        1 => return Ok(by_name[0].author),
        0 => {}
        count => bail!("{needle} matches {count} members by name; give an author id"),
    }
    let prefix = needle.to_ascii_lowercase();
    let by_id: Vec<_> = members
        .iter()
        .filter(|member| {
            member
                .author
                .to_string()
                .to_ascii_lowercase()
                .starts_with(&prefix)
        })
        .collect();
    match by_id.len() {
        1 => Ok(by_id[0].author),
        0 => bail!("no member matches {needle}"),
        count => bail!("{needle} matches {count} members; give more of the author id"),
    }
}

fn member_name(client: &RemoteClient, guild_id: GuildId, author: AuthorId) -> Result<String> {
    Ok(client
        .list_members(guild_id)?
        .into_iter()
        .find(|member| member.author == author)
        .map(|member| member.display_name)
        .unwrap_or_else(|| author.short()))
}

/// What a guild reference resolved to.
enum GuildTarget {
    /// A guild whose log is in the local store.
    Ready(GuildMeta),
    /// A direct conversation named by an announcement but not yet synced.
    DirectSyncing(DirectConversation),
}

/// Resolve a guild name, hex id or prefix, including DM ids that are only
/// known from a `DirectMessageOpened` announcement.
///
/// `find_guild` only sees guilds whose log exists locally. The announcement
/// that opens a DM arrives before the opener's log does, so the recipient can
/// list the conversation while `history` and `send` still cannot name it.
/// Reporting that as "not found" tells the user the conversation does not
/// exist, when in fact it is on its way.
fn resolve_guild_target(client: &RemoteClient, needle: &str) -> Result<GuildTarget> {
    match client.find_guild(needle) {
        Ok(guild) => Ok(GuildTarget::Ready(guild)),
        Err(error) => {
            let needle = needle.trim();
            let conversations = client.list_direct_messages()?;
            let exact = conversations
                .iter()
                .find(|conversation| conversation.guild.id.to_hex() == needle);
            let by_prefix: Vec<_> = conversations
                .iter()
                .filter(|conversation| conversation.guild.id.to_hex().starts_with(needle))
                .collect();
            match (exact, by_prefix.len()) {
                (Some(conversation), _) => Ok(GuildTarget::DirectSyncing(conversation.clone())),
                (None, 1) => Ok(GuildTarget::DirectSyncing(by_prefix[0].clone())),
                _ => Err(error.into()),
            }
        }
    }
}

fn direct_syncing_message(conversation: &DirectConversation) -> String {
    format!(
        "direct message with {} is still syncing; try again in a moment",
        safe(&conversation.peer.display_name)
    )
}

/// Validate the same profile limits the store enforces, before writing.
fn validate_profile_fields(about: &str, pronouns: &str, links: &[String]) -> Result<()> {
    if about.len() > MAX_PROFILE_ABOUT_BYTES {
        bail!(
            "about is {} bytes; the limit is {MAX_PROFILE_ABOUT_BYTES}",
            about.len()
        );
    }
    if pronouns.len() > MAX_PROFILE_PRONOUNS_BYTES {
        bail!(
            "pronouns are {} bytes; the limit is {MAX_PROFILE_PRONOUNS_BYTES}",
            pronouns.len()
        );
    }
    if links.len() > MAX_PROFILE_LINKS {
        bail!(
            "{} links given; a profile carries at most {MAX_PROFILE_LINKS}",
            links.len()
        );
    }
    for link in links {
        if link.len() > MAX_PROFILE_LINK_BYTES {
            bail!(
                "a profile link is {} bytes; the limit is {MAX_PROFILE_LINK_BYTES}",
                link.len()
            );
        }
        if !(link.starts_with("http://") || link.starts_with("https://")) {
            bail!("profile link {} is not an http(s) URL", safe(link));
        }
    }
    Ok(())
}

/// The lines `ovvc member` prints. Kept separate so the formatting is tested
/// without a daemon.
fn profile_lines(profile: &MemberProfile, me: AuthorId) -> Vec<String> {
    let mut lines = vec![
        format!("name:       {}", safe(&profile.display_name)),
        format!("author:     {}", profile.author.short()),
        format!("commune:    {}", profile.guild_id.short()),
        format!("joined:     {}", format_time(profile.joined_at)),
        format!("messages:   {}", profile.message_count),
        format!("activity:   {}", format_time(profile.last_activity)),
        format!(
            "you:        {}",
            if profile.author == me { "yes" } else { "no" }
        ),
        format!(
            "pronouns:   {}",
            if profile.pronouns.is_empty() {
                "(none)".to_string()
            } else {
                safe(&profile.pronouns)
            }
        ),
        "about:".to_string(),
    ];
    if profile.about.trim().is_empty() {
        lines.push("  (not set)".to_string());
    } else {
        lines.extend(
            profile
                .about
                .lines()
                .map(|line| format!("  {}", safe(line))),
        );
    }
    lines.push("links:".to_string());
    if profile.links.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        lines.extend(profile.links.iter().map(|link| format!("  {}", safe(link))));
    }
    lines
}

fn print_channels(client: &RemoteClient, guild_id: GuildId) -> Result<()> {
    let channels = client.list_channels(guild_id)?;
    if channels.is_empty() {
        outln!("no channels");
        return Ok(());
    }
    for channel in channels {
        outln!("  #{}  {}", safe(&channel.name), channel.id);
    }
    Ok(())
}

fn print_history(
    client: &RemoteClient,
    guild_id: GuildId,
    channel_id: ChannelId,
    limit: usize,
) -> Result<()> {
    let events = client.history(guild_id, channel_id, limit)?;
    let entries = timeline(&events)?;
    let mut printed = 0usize;
    for entry in &entries {
        match entry {
            TimelineEntry::Message { .. } | TimelineEntry::Deleted { .. } => {
                print_timeline_entry(entry);
                printed += 1;
            }
            TimelineEntry::Other(_) => {}
        }
    }
    if printed == 0 {
        outln!("no messages");
    }
    Ok(())
}

fn print_timeline_entry(entry: &TimelineEntry<'_>) {
    match entry {
        TimelineEntry::Message {
            event,
            body,
            edited,
            reply_to,
            mentions,
            reactions,
        } => {
            if let Some(reply) = reply_to {
                match &reply.body {
                    Some(target_body) => outln!(
                        "    replying to {}: {}",
                        reply.event.body.author.short(),
                        safe(&preview(target_body, 60))
                    ),
                    None => outln!(
                        "    replying to {}: <message deleted>",
                        reply.event.body.author.short()
                    ),
                }
            }
            let edited = if *edited { " (edited)" } else { "" };
            let mentioned = if !mentions.is_empty() {
                format!(" (+{} mentioned)", mentions.len())
            } else {
                String::new()
            };
            let reacted = if reactions.is_empty() {
                String::new()
            } else {
                let summary: Vec<String> = reactions
                    .iter()
                    .map(|reaction| format!("{} x{}", reaction.emoji, reaction.authors.len()))
                    .collect();
                format!(" [{}]", summary.join(" "))
            };
            outln!(
                "[{}] {} {}: {}{edited}{mentioned}{reacted}",
                format_time(event.body.timestamp),
                message_id(event),
                event.body.author.short(),
                safe(body)
            );
            for attachment in event.body.kind.attachments() {
                outln!(
                    "    file: {} ({}, {}, {})",
                    safe(&attachment.name),
                    attachment.mime,
                    readable_size(attachment.size),
                    attachment.hash.short()
                );
            }
        }
        TimelineEntry::Deleted { event } => {
            outln!(
                "[{}] {} {}: <message deleted>",
                format_time(event.body.timestamp),
                message_id(event),
                event.body.author.short()
            );
        }
        TimelineEntry::Other(_) => {}
    }
}

fn print_message(event: &SignedEvent) {
    let Some(body) = event.body.kind.message_body() else {
        return;
    };
    let mentioned = if event.body.kind.mentions().is_empty() {
        String::new()
    } else {
        format!(" (+{} mentioned)", event.body.kind.mentions().len())
    };
    outln!(
        "[{}] {} {}: {}{mentioned}",
        format_time(event.body.timestamp),
        message_id(event),
        event.body.author.short(),
        safe(body)
    );
    for attachment in event.body.kind.attachments() {
        outln!(
            "    file: {} ({}, {})",
            safe(&attachment.name),
            attachment.mime,
            readable_size(attachment.size)
        );
    }
}

/// Human readable file size for a CLI attachment line, shared with the other
/// frontends.
fn readable_size(bytes: u64) -> String {
    human_size(bytes)
}

/// Resolve `@name` / `@id-prefix` tokens in `text` against the guild roster.
fn mentions_for(client: &RemoteClient, guild_id: GuildId, text: &str) -> Result<Vec<AuthorId>> {
    if !text.contains('@') {
        return Ok(Vec::new());
    }
    Ok(parse_mentions(text, &client.list_members(guild_id)?))
}

/// The longest `presence --hold` this command accepts, so a typo cannot pin a
/// presence for days.
const MAX_PRESENCE_HOLD_SECS: u64 = 3600;

fn presence_from_name(name: &str) -> Option<PresenceStatus> {
    match name.trim().to_lowercase().as_str() {
        "online" => Some(PresenceStatus::Online),
        "away" => Some(PresenceStatus::Away),
        "dnd" | "busy" | "do-not-disturb" => Some(PresenceStatus::DoNotDisturb),
        "offline" | "invisible" => Some(PresenceStatus::Offline),
        _ => None,
    }
}

fn presence_name(status: PresenceStatus) -> &'static str {
    match status {
        PresenceStatus::Online => "online",
        PresenceStatus::Away => "away",
        PresenceStatus::DoNotDisturb => "dnd",
        PresenceStatus::Offline => "offline",
    }
}

/// Run one `ovvc voice` subcommand against the daemon.
fn voice_command(session: &Session, guild: &str, command: VoiceCommand) -> Result<()> {
    let client = session.open()?;
    let guild = client.find_guild(guild)?;
    match command {
        VoiceCommand::Join { channel } => {
            let channel = voice_channel(&client, guild.id, &channel)?;
            // Each channel's call has its own audio service, so a stale call
            // left by another frontend would keep a second device open.
            leave_other_voice_calls(&client, guild.id, channel.id)?;
            let snapshot = client.voice_join(guild.id, channel.id, false, false, false)?;
            outln!("joined ~ #{}", safe(&channel.name));
            print_voice_snapshot(&client, guild.id, &snapshot)?;
        }
        VoiceCommand::Leave { channel } => {
            let channel = voice_channel(&client, guild.id, &channel)?;
            match client.voice_leave(guild.id, channel.id)? {
                Some(snapshot) => {
                    outln!("left ~ #{}", safe(&channel.name));
                    print_voice_snapshot(&client, guild.id, &snapshot)?;
                }
                None => outln!("left ~ #{} (the call ended)", safe(&channel.name)),
            }
        }
        VoiceCommand::Mute { channel, on } => {
            let channel = voice_channel(&client, guild.id, &channel)?;
            let snapshot = voice_set_flag(&client, guild.id, channel.id, |state| {
                state.muted = on;
            })?;
            outln!("mute {} in ~ #{}", on_word(on), safe(&channel.name));
            print_voice_snapshot(&client, guild.id, &snapshot)?;
        }
        VoiceCommand::Deafen { channel, on } => {
            let channel = voice_channel(&client, guild.id, &channel)?;
            let snapshot = voice_set_flag(&client, guild.id, channel.id, |state| {
                state.deafened = on;
            })?;
            outln!("deafen {} in ~ #{}", on_word(on), safe(&channel.name));
            print_voice_snapshot(&client, guild.id, &snapshot)?;
        }
        VoiceCommand::Ptt { channel, on } => {
            let channel = voice_channel(&client, guild.id, &channel)?;
            let snapshot = voice_set_flag(&client, guild.id, channel.id, |state| {
                state.push_to_talk = on;
            })?;
            outln!("push-to-talk {} in ~ #{}", on_word(on), safe(&channel.name));
            print_voice_snapshot(&client, guild.id, &snapshot)?;
        }
        VoiceCommand::Status { channel } => {
            let channel = voice_channel(&client, guild.id, &channel)?;
            match client.voice_snapshot(guild.id, channel.id)? {
                Some(snapshot) => print_voice_snapshot(&client, guild.id, &snapshot)?,
                None => outln!("no call in ~ #{}", safe(&channel.name)),
            }
            match client.voice_audio(guild.id, channel.id)? {
                Some(stats) => outln!("{}", voice_audio_line(&stats)),
                None => {
                    // Name the backend this daemon would select, so an `auto`
                    // fallback to the silent null backend is visible even when
                    // no audio service is running.
                    let selected = client
                        .daemon_capabilities()
                        .ok()
                        .flatten()
                        .map(|caps| caps.audio_backend_selected)
                        .unwrap_or_else(|| "unknown".to_string());
                    outln!(
                        "audio: no media for this call (selected backend {selected}, no audio service running)"
                    );
                }
            }
        }
    }
    Ok(())
}

fn on_word(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// Leave any call in this guild that still contains the local member, apart
/// from `keep`.
fn leave_other_voice_calls(
    client: &RemoteClient,
    guild_id: GuildId,
    keep: ChannelId,
) -> Result<()> {
    let local = client.author_id();
    for view in client.list_channel_views(guild_id)? {
        if view.kind != ChannelKind::Voice || view.id == keep {
            continue;
        }
        if let Some(snapshot) = client.voice_snapshot(guild_id, view.id)?
            && snapshot.members.iter().any(|member| member.author == local)
        {
            client.voice_leave(guild_id, view.id)?;
        }
    }
    Ok(())
}

/// Resolve a channel and refuse one that cannot carry a call.
fn voice_channel(client: &RemoteClient, guild_id: GuildId, needle: &str) -> Result<ChannelMeta> {
    let channel = client.find_channel(guild_id, needle)?;
    // `find_channel` returns the stored metadata, which does not carry the
    // channel kind. The channel tree does, so resolve the same id there.
    let kind = client
        .list_channel_views(guild_id)?
        .into_iter()
        .find(|view| view.id == channel.id)
        .map(|view| view.kind)
        .unwrap_or(ChannelKind::Text);
    if kind != ChannelKind::Voice {
        bail!(
            "#{} is a {} channel, not a voice channel",
            safe(&channel.name),
            kind.label()
        );
    }
    Ok(channel)
}

/// Read the local member's published state, apply `change`, then publish it.
///
/// The previous state is copied from the live snapshot so speaking, video and
/// screen share are not cleared by a flag this command does not own.
fn voice_set_flag(
    client: &RemoteClient,
    guild_id: GuildId,
    channel_id: ChannelId,
    change: impl FnOnce(&mut VoiceMemberState),
) -> Result<VoiceSnapshot> {
    let previous = client
        .voice_snapshot(guild_id, channel_id)?
        .and_then(|snapshot| {
            snapshot
                .members
                .iter()
                .find(|member| member.author == client.author_id())
                .map(|member| member.state)
        })
        .unwrap_or_default();
    let mut state = previous;
    change(&mut state);
    Ok(client.set_voice_state(guild_id, channel_id, state)?)
}

/// The signalled call topology.
///
/// The control plane elects a hub and reports a flat star above two members.
/// The snapshot also carries `hub_policy` and `key_epoch`, so the label can
/// name the key ring that the daemon actually installed.
fn voice_topology_label(topology: &CallTopology) -> &'static str {
    match topology {
        CallTopology::Mesh => "mesh",
        CallTopology::Star { .. } => "star",
    }
}

/// The media path that is actually running, with the keyring truth.
fn voice_media_label(topology: &CallTopology) -> &'static str {
    match topology {
        CallTopology::Mesh => "media path: mesh  keys all-pairs",
        CallTopology::Star { .. } => {
            "media path: mesh (hub forwarding pending)  keys all-pairs (leaf->hub pending)"
        }
    }
}

/// The one coordinator marker for a member, if any.
fn voice_role_marker(
    hub: Option<AuthorId>,
    master: Option<AuthorId>,
    backup: Option<AuthorId>,
    author: AuthorId,
) -> Option<&'static str> {
    if hub == Some(author) {
        Some("[hub]")
    } else if master == Some(author) {
        Some("[master]")
    } else if backup == Some(author) {
        Some("[backup]")
    } else {
        None
    }
}

/// Print one call: header, coordinators, then every member with markers.
fn print_voice_snapshot(
    client: &RemoteClient,
    guild_id: GuildId,
    snapshot: &VoiceSnapshot,
) -> Result<()> {
    let names: HashMap<AuthorId, String> = client
        .list_members(guild_id)
        .unwrap_or_default()
        .into_iter()
        .map(|member| (member.author, member.display_name))
        .collect();
    let topology = voice_topology_label(&snapshot.topology);
    outln!(
        "call #{}  {} member(s)  topology {}",
        channels_name(client, guild_id, snapshot.channel_id),
        snapshot.members.len(),
        topology
    );
    outln!(
        "hub {}  master {}  backup {}  trust {}  keys e{}",
        voice_author_name(&names, snapshot.hub),
        voice_author_name(&names, snapshot.master),
        voice_author_name(&names, snapshot.backup),
        snapshot.hub_policy.trust.name(),
        snapshot.key_epoch
    );
    outln!("{}", voice_media_label(&snapshot.topology));
    for member in &snapshot.members {
        let mut markers = voice_member_markers(member);
        if let Some(marker) = voice_role_marker(
            snapshot.hub,
            snapshot.master,
            snapshot.backup,
            member.author,
        ) {
            push_marker(&mut markers, marker);
        }
        let name = names
            .get(&member.author)
            .cloned()
            .unwrap_or_else(|| member.author.short());
        outln!("  {:<28} {}", markers, safe(&name));
    }
    Ok(())
}

fn channels_name(client: &RemoteClient, guild_id: GuildId, channel_id: ChannelId) -> String {
    client
        .get_channel(guild_id, channel_id)
        .ok()
        .flatten()
        .map(|channel| safe(&channel.name))
        .unwrap_or_else(|| channel_id.short())
}

fn voice_author_name(names: &HashMap<AuthorId, String>, author: Option<AuthorId>) -> String {
    match author {
        Some(author) => names
            .get(&author)
            .cloned()
            .map(|name| safe(&name))
            .unwrap_or_else(|| author.short()),
        None => "none".to_string(),
    }
}

/// The bracketed state markers for one member, in a stable ASCII order.
fn voice_member_markers(member: &VoiceMember) -> String {
    let mut markers = String::new();
    if member.state.speaking {
        markers.push_str("[spk]");
    }
    if member.state.muted {
        push_marker(&mut markers, "[mute]");
    }
    if member.state.deafened {
        push_marker(&mut markers, "[deaf]");
    }
    if member.state.push_to_talk {
        push_marker(&mut markers, "[ptt]");
    }
    markers
}

fn push_marker(markers: &mut String, marker: &str) {
    if !markers.is_empty() {
        markers.push(' ');
    }
    markers.push_str(marker);
}

/// One line with every counter the daemon reports for the local audio engine,
/// including which media transport carried the call and, for WebRTC, its
/// connection quality.
fn voice_audio_line(stats: &VoiceAudioStats) -> String {
    // RTT, loss and relay state are WebRTC-only counters. Printing zeroes for
    // the libp2p fallback would present values that were never measured.
    let quality = if stats.transport == "webrtc" {
        format!(
            "transport={} rtt={}ms packets_lost={} relayed={}",
            stats.transport, stats.rtt_ms, stats.packets_lost, stats.relayed
        )
    } else {
        format!(
            "transport={} rtt=- packets_lost=- relayed=-",
            stats.transport
        )
    };
    format!(
        "audio: {quality} e2ee={} epoch={} e2ee_errors={} backend={} peers={} latency={}ms xruns={} captured={} packets={} played={} concealed={} lost={} decode_errors={} peak={} rms={:.3}",
        stats.e2ee,
        stats.epoch,
        stats.e2ee_errors,
        stats.backend,
        stats.peers,
        stats.latency_ms,
        stats.xruns,
        stats.captured,
        stats.packets,
        stats.played,
        stats.concealed,
        stats.lost,
        stats.decode_errors,
        stats.peak,
        stats.rms
    )
}

fn policy_command(session: &Session, guild: &str) -> Result<()> {
    let client = session.open()?;
    let guild = client.find_guild(guild)?;
    let view = client.policy(guild.id)?;
    outln!("commune: {} ({})", safe(&guild.name), guild.id.short());
    outln!(
        "guild:   {} / {}",
        view.guild.profile.name(),
        view.guild.topology.name()
    );
    outln!();
    outln!(
        "{:<16} {:<21} {:<21} {:<21} {}",
        "axis",
        "guild",
        "client floor",
        "effective",
        "raised"
    );
    for (axis, published, floor, effective) in policy_axis_rows(&view) {
        let raised = if view.raised_by_client.iter().any(|name| name == axis) {
            "*"
        } else {
            ""
        };
        outln!(
            "{:<16} {:<21} {:<21} {:<21} {}",
            axis,
            published,
            floor,
            effective,
            raised
        );
    }
    outln!();
    let raised = if view.raised_by_client.is_empty() {
        "none".to_string()
    } else {
        view.raised_by_client.join(", ")
    };
    outln!("raised by client: {raised}");
    outln!("read model only; policy enforcement is not implemented");
    Ok(())
}

/// One row per axis: guild value, client floor, effective value.
///
/// `None` in the floor means the member has no opinion on that axis. The
/// debug spelling is the enum variant name, which is compact enough for the
/// terminal and stable across builds.
fn policy_axis_rows(view: &PolicyView) -> Vec<(&'static str, String, String, String)> {
    let guild = &view.guild;
    let floor = &view.client_floor;
    let effective = &view.effective;
    vec![
        (
            "discovery",
            format!("{:?}", guild.discovery),
            floor
                .discovery
                .map(|value| format!("{value:?}"))
                .unwrap_or_else(|| "-".to_string()),
            format!("{:?}", effective.discovery),
        ),
        (
            "transport",
            format!("{:?}", guild.transport),
            floor
                .transport
                .map(|value| format!("{value:?}"))
                .unwrap_or_else(|| "-".to_string()),
            format!("{:?}", effective.transport),
        ),
        (
            "padding",
            format!("{:?}", guild.padding),
            floor
                .padding
                .map(|value| format!("{value:?}"))
                .unwrap_or_else(|| "-".to_string()),
            format!("{:?}", effective.padding),
        ),
        (
            "author_identity",
            format!("{:?}", guild.author_identity),
            floor
                .author_identity
                .map(|value| format!("{value:?}"))
                .unwrap_or_else(|| "-".to_string()),
            format!("{:?}", effective.author_identity),
        ),
        (
            "timestamps",
            format!("{:?}", guild.timestamps),
            floor
                .timestamps
                .map(|value| format!("{value:?}"))
                .unwrap_or_else(|| "-".to_string()),
            format!("{:?}", effective.timestamps),
        ),
        (
            "hardware_keys",
            format!("{:?}", guild.hardware_keys),
            floor
                .hardware_keys
                .map(|value| format!("{value:?}"))
                .unwrap_or_else(|| "-".to_string()),
            format!("{:?}", effective.hardware_keys),
        ),
        (
            "plugins",
            format!("{:?}", guild.plugins),
            floor
                .plugins
                .map(|value| format!("{value:?}"))
                .unwrap_or_else(|| "-".to_string()),
            format!("{:?}", effective.plugins),
        ),
    ]
}

fn theme_command(preset: Option<String>, list: bool) -> Result<()> {
    let active = match active_theme_preset() {
        Ok(active) => active,
        Err(error) => {
            errln!("warning: {error}");
            None
        }
    };
    if list {
        for candidate in Preset::all().iter().copied() {
            let marker = if Some(candidate) == active { "*" } else { " " };
            outln!(
                "{marker} {:<9} {:<9} {}",
                candidate.name(),
                candidate.label(),
                candidate.description()
            );
        }
        match ovvc_theme::theme_path() {
            Some(path) => outln!("file: {}", path.display()),
            None => outln!("no config directory; set OVVC_THEME to choose a file"),
        }
        return Ok(());
    }
    match preset {
        Some(name) => {
            let Some(preset) = Preset::from_name(&name) else {
                bail!("unknown theme preset {name:?}; try phosphor, amber, ice or paper");
            };
            let path = ovvc_theme::save_preset(preset)?;
            outln!("theme set to {} ({})", preset.label(), path.display());
            outln!("running frontends keep their current palette until restarted");
        }
        None => match ovvc_theme::theme_path() {
            Some(path) => {
                outln!("file: {}", path.display());
                match active {
                    Some(preset) => outln!("preset: {} ({})", preset.name(), preset.label()),
                    None if path.exists() => outln!("preset: custom"),
                    None => outln!("preset: phosphor (built in; no file yet)"),
                }
            }
            None => outln!("no config directory; set OVVC_THEME to choose a file"),
        },
    }
    Ok(())
}

/// Which preset the theme file selects, if it matches one exactly.
///
/// A hand-edited file with any changed colour reports no preset, because the
/// list should not claim a palette it is not showing.
fn active_theme_preset() -> Result<Option<Preset>> {
    let Some(path) = ovvc_theme::theme_path() else {
        return Ok(None);
    };
    let Some(theme) = ovvc_theme::load_from(&path)? else {
        return Ok(None);
    };
    Ok(Preset::all()
        .iter()
        .copied()
        .find(|preset| Theme::preset(*preset) == theme))
}

/// Find the attachment a download command names, by full id, id prefix or the
/// file name shown in history.
fn resolve_attachment(
    client: &RemoteClient,
    guild_id: GuildId,
    needle: &str,
    channel: Option<&str>,
) -> Result<Attachment> {
    let needle = needle.trim();
    if needle.is_empty() {
        bail!("no attachment given");
    }
    if let Ok(id) = needle.parse::<AttachmentId>() {
        if let Some(named) = collect_attachments(client, guild_id, channel)?
            .into_iter()
            .find(|attachment| attachment.hash == id)
        {
            return Ok(named);
        }
        return Ok(Attachment {
            hash: id,
            size: 0,
            mime: "application/octet-stream".to_string(),
            name: format!("attachment-{}", id.short()),
        });
    }
    let prefix = needle.to_ascii_lowercase();
    let matches: Vec<Attachment> = collect_attachments(client, guild_id, channel)?
        .into_iter()
        .filter(|attachment| {
            attachment.hash.to_hex().starts_with(&prefix) || attachment.name == needle
        })
        .collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().expect("exactly one match")),
        0 => bail!("no attachment matches {needle}; history shows each file's id"),
        count => bail!("{needle} matches {count} attachments; give more of the id"),
    }
}

/// Every attachment announced in the chosen channel, or in every channel when
/// none is named, newest message first.
fn collect_attachments(
    client: &RemoteClient,
    guild_id: GuildId,
    channel: Option<&str>,
) -> Result<Vec<Attachment>> {
    let channels = match channel {
        Some(name) => vec![client.find_channel(guild_id, name)?],
        None => client.list_channels(guild_id)?,
    };
    let mut seen = std::collections::HashSet::new();
    let mut found = Vec::new();
    for channel in channels {
        for event in client.history(guild_id, channel.id, 500)? {
            for attachment in event.body.kind.attachments() {
                if seen.insert(attachment.hash) {
                    found.push(attachment.clone());
                }
            }
        }
    }
    Ok(found)
}

/// A file name a download can use without escaping the current directory.
///
/// The announced name comes from a peer, so path separators and leading dots
/// are replaced rather than trusted.
fn safe_download_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .take(80)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | ' ') {
                character
            } else {
                '_'
            }
        })
        .collect();
    while out.starts_with('.') {
        out.remove(0);
    }
    if out.trim().is_empty() {
        out = "attachment".to_string();
    }
    out
}

/// The short event id shown in history, so `/reply`, `/edit` and `/delete`
/// can address a message without a separate lookup command.
fn message_id(event: &SignedEvent) -> String {
    event
        .id()
        .map(|id| id.short())
        .unwrap_or_else(|_| "????????".to_string())
}

fn preview(text: &str, width: usize) -> String {
    let flattened: String = text
        .chars()
        .map(|c| if c == '\n' { ' ' } else { c })
        .collect();
    if flattened.chars().count() <= width {
        return flattened;
    }
    let mut out: String = flattened.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Resolve a message by full event id or unique hex prefix within a channel's
/// recent history.
fn resolve_message(
    client: &RemoteClient,
    guild_id: GuildId,
    channel_id: ChannelId,
    needle: &str,
) -> Result<SignedEvent> {
    let needle = needle.trim();
    // The default page is small; edits and deletes are typically aimed at
    // something recent, so look back further than the history view shows.
    let events = client.history(guild_id, channel_id, 1000)?;
    let matches: Vec<_> = events
        .into_iter()
        .filter(|event| event.body.kind.is_message())
        .filter(|event| {
            event
                .id()
                .map(|id| id.to_hex().starts_with(needle))
                .unwrap_or(false)
        })
        .collect();
    match matches.len() {
        0 => bail!("no message with id '{needle}' in this channel"),
        1 => Ok(matches.into_iter().next().expect("one match")),
        _ => bail!("message id '{needle}' is ambiguous"),
    }
}

fn chat_edit(
    client: &RemoteClient,
    guild_id: GuildId,
    channel_id: ChannelId,
    args: &str,
) -> Result<SignedEvent> {
    let Some((target, body)) = args.split_once(' ') else {
        bail!("usage: /edit <message-id> <new text>");
    };
    let target = resolve_message(client, guild_id, channel_id, target)?;
    Ok(client.edit_message(guild_id, target.id()?, body.trim())?)
}

fn chat_reply(
    client: &RemoteClient,
    guild_id: GuildId,
    channel_id: ChannelId,
    args: &str,
) -> Result<SignedEvent> {
    let Some((target, body)) = args.split_once(' ') else {
        bail!("usage: /reply <message-id> <text>");
    };
    let body = body.trim();
    if body.is_empty() {
        bail!("usage: /reply <message-id> <text>");
    }
    let target = resolve_message(client, guild_id, channel_id, target)?;
    let mentions = mentions_for(client, guild_id, body)?;
    Ok(client.post_message_full(guild_id, channel_id, body, Some(target.id()?), &mentions)?)
}

fn chat_delete(
    client: &RemoteClient,
    guild_id: GuildId,
    channel_id: ChannelId,
    args: &str,
) -> Result<()> {
    let target = args.trim();
    if target.is_empty() {
        bail!("usage: /delete <message-id>");
    }
    let target = resolve_message(client, guild_id, channel_id, target)?;
    client.delete_message(guild_id, target.id()?)?;
    Ok(())
}

fn local_chat(client: &RemoteClient, guild: GuildMeta, mut channel: ChannelMeta) -> Result<()> {
    outln!("{} / #{}", safe(&guild.name), safe(&channel.name));
    outln!("local mode; type /help for commands, /quit to exit");
    print_history(client, guild.id, channel.id, 50)?;

    let stdin = io::stdin();
    let mut line = String::new();
    loop {
        outp!("> ");
        io::stdout().flush()?;
        line.clear();
        if stdin.read_line(&mut line)? == 0 {
            outln!();
            break;
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }

        match input {
            "/quit" | "/exit" => break,
            "/help" => print_help(),
            "/channels" => print_channel_list(client, guild.id, &channel)?,
            "/members" => print_members(client, guild.id)?,
            _ if input.starts_with("/name ") => {
                let name = input["/name ".len()..].trim();
                match client.set_display_name(guild.id, name) {
                    Ok(()) => outln!("display name set to {}", safe(name)),
                    Err(error) => errln!("error: {error:#}"),
                }
            }
            "/history" => print_history(client, guild.id, channel.id, 50)?,
            _ if input.starts_with("/channel ") => {
                let name = input["/channel ".len()..].trim();
                match client.find_channel(guild.id, name) {
                    Ok(next) => {
                        channel = next;
                        outln!("switched to #{}", safe(&channel.name));
                    }
                    Err(error) => errln!("error: {error}"),
                }
            }
            _ if input.starts_with("/reply ") => {
                match chat_reply(client, guild.id, channel.id, &input["/reply ".len()..]) {
                    Ok(event) => print_message(&event),
                    Err(error) => errln!("error: {error:#}"),
                }
            }
            _ if input.starts_with("/edit ") => {
                match chat_edit(client, guild.id, channel.id, &input["/edit ".len()..]) {
                    Ok(event) => outln!("edited {}", event.id()?),
                    Err(error) => errln!("error: {error:#}"),
                }
            }
            _ if input.starts_with("/delete ") => {
                match chat_delete(client, guild.id, channel.id, &input["/delete ".len()..]) {
                    Ok(()) => outln!("message deleted"),
                    Err(error) => errln!("error: {error:#}"),
                }
            }
            _ if input.starts_with('/') => {
                errln!("unknown command; type /help");
            }
            _ => {
                let mentions = mentions_for(client, guild.id, input)?;
                let event =
                    client.post_message_full(guild.id, channel.id, input, None, &mentions)?;
                print_message(&event);
            }
        }
    }

    Ok(())
}

enum ChatAction {
    Continue,
    Quit,
    Switch(ChannelMeta),
    Send(SignedEvent),
}

fn handle_chat_input(
    client: &RemoteClient,
    guild_id: GuildId,
    channel: &ChannelMeta,
    input: &str,
) -> Result<ChatAction> {
    match input {
        "/quit" | "/exit" => Ok(ChatAction::Quit),
        "/help" => {
            print_help();
            outln!("/find <author-id>  look up a peer in the DHT and dial it");
            Ok(ChatAction::Continue)
        }
        "/channels" => {
            print_channel_list(client, guild_id, channel)?;
            Ok(ChatAction::Continue)
        }
        "/members" => {
            print_members(client, guild_id)?;
            Ok(ChatAction::Continue)
        }
        _ if input.starts_with("/name ") => {
            let name = input["/name ".len()..].trim();
            match client.set_display_name(guild_id, name) {
                Ok(()) => outln!("display name set to {}", safe(name)),
                Err(error) => errln!("error: {error:#}"),
            }
            Ok(ChatAction::Continue)
        }
        "/history" => {
            print_history(client, guild_id, channel.id, 50)?;
            Ok(ChatAction::Continue)
        }
        _ if input.starts_with("/channel ") => {
            let name = input["/channel ".len()..].trim();
            let next = client.find_channel(guild_id, name)?;
            outln!("switched to #{}", safe(&next.name));
            Ok(ChatAction::Switch(next))
        }
        _ if input.starts_with("/reply ") => {
            match chat_reply(client, guild_id, channel.id, &input["/reply ".len()..]) {
                Ok(event) => outln!("replied {}", event.id()?),
                Err(error) => errln!("error: {error:#}"),
            }
            Ok(ChatAction::Continue)
        }
        _ if input.starts_with("/edit ") => {
            match chat_edit(client, guild_id, channel.id, &input["/edit ".len()..]) {
                Ok(event) => outln!("edited {}", event.id()?),
                Err(error) => errln!("error: {error:#}"),
            }
            Ok(ChatAction::Continue)
        }
        _ if input.starts_with("/delete ") => {
            match chat_delete(client, guild_id, channel.id, &input["/delete ".len()..]) {
                Ok(()) => outln!("message deleted"),
                Err(error) => errln!("error: {error:#}"),
            }
            Ok(ChatAction::Continue)
        }
        _ if input.starts_with('/') => {
            errln!("unknown command; type /help");
            Ok(ChatAction::Continue)
        }
        _ => {
            let mentions = mentions_for(client, guild_id, input)?;
            let event = client.post_message_full(guild_id, channel.id, input, None, &mentions)?;
            Ok(ChatAction::Send(event))
        }
    }
}

fn print_members(client: &RemoteClient, guild_id: GuildId) -> Result<()> {
    let members = client.list_members(guild_id)?;
    if members.is_empty() {
        outln!("no members");
        return Ok(());
    }
    for member in members {
        outln!(
            "  {}  {}",
            safe(&member.display_name),
            member.author.short()
        );
    }
    Ok(())
}

fn print_help() {
    outln!("/channels          list channels");
    outln!("/channel <name>    switch channel");
    outln!("/members           list commune members");
    outln!("/name <name>       set your display name (for @mentions)");
    outln!("/history           show recent messages");
    outln!("/reply <id> <text> reply to a message");
    outln!("/edit <id> <text>  edit one of your messages");
    outln!("/delete <id>       delete one of your messages");
    outln!("/quit              exit");
}

fn print_channel_list(
    client: &RemoteClient,
    guild_id: GuildId,
    current: &ChannelMeta,
) -> Result<()> {
    for candidate in client.list_channels(guild_id)? {
        let marker = if candidate.id == current.id { "*" } else { " " };
        outln!(
            "{marker} #{}  {}",
            safe(&candidate.name),
            candidate.id.short()
        );
    }
    Ok(())
}

async fn networked_chat(
    client: Arc<RemoteClient>,
    guild: GuildMeta,
    mut channel: ChannelMeta,
    options: NetworkOptions,
    find: Vec<AuthorId>,
) -> Result<()> {
    let mut events = client.subscribe();
    // A previous frontend may have been killed before it could disconnect, so
    // start from a clean network state and honour this session's listen
    // addresses rather than reusing a stale node.
    client.disconnect(guild.id);
    client.connect_async(guild.id, options).await?;

    outln!("{} / #{}", safe(&guild.name), safe(&channel.name));
    if let Some(peer_id) = client.peer_id(guild.id) {
        outln!("peer id: {peer_id}");
    }
    outln!("p2p mode; mDNS + Kademlia discovery active, type /help for commands");
    for author in find {
        client.lookup(guild.id, author)?;
    }
    print_history(&client, guild.id, channel.id, 50)?;

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let line = match line {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(error) => return Err(error.into()),
                };
                let input = line.trim();
                if input.is_empty() {
                    continue;
                }
                if let Some(value) = input.strip_prefix("/find ") {
                    match value.trim().parse::<AuthorId>() {
                        Ok(author) => {
                            if let Err(error) = client.lookup(guild.id, author) {
                                errln!("error: {error}");
                            }
                        }
                        Err(error) => errln!("error: {error}"),
                    }
                    continue;
                }
                match handle_chat_input(&client, guild.id, &channel, input) {
                    Ok(ChatAction::Continue) => {}
                    Ok(ChatAction::Quit) => break,
                    Ok(ChatAction::Switch(next)) => {
                        channel = next;
                    }
                    Ok(ChatAction::Send(event)) => {
                        print_message(&event);
                    }
                    Err(error) => errln!("error: {error:#}"),
                }
            }
            event = events.recv() => {
                match event {
                    Ok(ClientEvent::Listening { address, .. }) => {
                        if let Some(peer_id) = client.peer_id(guild.id) {
                            print_listen(&address, peer_id);
                        }
                    }
                    Ok(ClientEvent::ExternalAddr { address, .. }) => {
                        outln!("external address: {}", safe(&address.to_string()));
                    }
                    Ok(ClientEvent::ExternalAddrRemoved { address, .. }) => {
                        outln!("external address expired: {}", safe(&address.to_string()));
                    }
                    Ok(ClientEvent::PeerConnected { peer, .. }) => outln!("peer connected: {peer}"),
                    Ok(ClientEvent::PeerDisconnected { peer, .. }) => {
                        outln!("peer disconnected: {peer}");
                    }
                    Ok(ClientEvent::HolePunch { peer, success, .. }) => {
                        if success {
                            outln!("direct connection upgraded with {peer}");
                        } else {
                            outln!("direct connection attempt with {peer} failed; staying on relay");
                        }
                    }
                    Ok(ClientEvent::Synced { peer, .. }) => outln!("synced with {peer}"),
                    Ok(ClientEvent::LookingUp { author, .. }) => {
                        outln!("looking up {} in the DHT", author.short());
                    }
                    Ok(ClientEvent::PeerFound { author, addresses, .. }) => {
                        outln!(
                            "found {} ({} announced address(es)); dialing",
                            author.short(),
                            addresses.len()
                        );
                    }
                    Ok(ClientEvent::LookupFailed { author, reason, .. }) => {
                        errln!("lookup for {} failed: {}", author.short(), safe(&reason));
                    }
                    Ok(ClientEvent::Event { event, .. }) => {
                        if event.body.channel_id == Some(channel.id)
                            && event.body.author != client.author_id()
                        {
                            match &event.body.kind {
                                EventKind::Message { .. }
                                | EventKind::MessageV2 { .. }
                                | EventKind::MessageV3 { .. } => {
                                    print_message(&event);
                                    if event.body.kind.mentions_author(client.author_id()) {
                                        // Terminal bell plus a line that cannot
                                        // be mistaken for the message itself.
                                        outp!("\x07");
                                        outln!(
                                            "*** {} mentioned you ***",
                                            event.body.author.short()
                                        );
                                    }
                                }
                                EventKind::MessageEdited { .. } => outln!(
                                    "{} edited a message; /history to refresh",
                                    event.body.author.short()
                                ),
                                EventKind::MessageDeleted { .. } => outln!(
                                    "{} deleted a message; /history to refresh",
                                    event.body.author.short()
                                ),
                                _ => {}
                            }
                        }
                    }
                    Ok(ClientEvent::Diagnostic(diagnostic)) => print_diagnostic(&diagnostic),
                    Ok(ClientEvent::Error { message, .. }) => errln!("warning: {}", safe(&message)),
                    Ok(ClientEvent::Control { .. }) => {}
                    // `ClientEvent` is non-exhaustive, so a variant added by a
                    // newer build is ignored here rather than failing to
                    // compile. Nothing in this loop needs to hear every event.
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        errln!("network events skipped: {skipped}");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    client.disconnect(guild.id);
    Ok(())
}

fn print_listen(address: &Multiaddr, peer_id: ovvc_client::PeerId) {
    let text = address.to_string();
    outln!("listening: {text}");
    let dialable = if let Some(rest) = text.strip_prefix("/ip4/0.0.0.0/") {
        format!("/ip4/127.0.0.1/{rest}")
    } else if let Some(rest) = text.strip_prefix("/ip6/::/") {
        format!("/ip6/::1/{rest}")
    } else {
        text
    };
    outln!("dial: {dialable}/p2p/{peer_id}");
}

fn parse_addrs(values: &[String]) -> Result<Vec<Multiaddr>> {
    values
        .iter()
        .map(|value| {
            value
                .parse::<Multiaddr>()
                .with_context(|| format!("invalid multiaddr: {value}"))
        })
        .collect()
}

fn format_time(timestamp: u64) -> String {
    OffsetDateTime::from_unix_timestamp(timestamp as i64)
        .ok()
        .and_then(|time| time.format(&Rfc3339).ok())
        .unwrap_or_else(|| timestamp.to_string())
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use ovvc_core::Identity;

    use super::*;

    #[test]
    fn safe_escapes_terminal_commands_and_line_breaks() {
        assert_eq!(
            safe("hi\x1b]0;title\x07\nthere"),
            "hi\\u{1b}]0;title\\u{7}\\nthere"
        );
        assert_eq!(safe("a\u{202e}b"), "a\\u{202e}b");
        assert_eq!(safe("plain text"), "plain text");
    }

    #[test]
    fn a_forwarding_topology_is_labelled_star_and_a_mesh_stays_mesh() {
        // Seven members: the elected hub plus six leaves, which is the shape
        // the control plane now derives above MESH_LIMIT.
        let root = Identity::from_seed([11u8; 32]).author_id();
        let leaves = (2u8..=7)
            .map(|seed| Identity::from_seed([seed; 32]).author_id())
            .collect();
        let forwarding = CallTopology::Star { hub: root, leaves };
        assert_eq!(voice_topology_label(&forwarding), "star");
        assert_eq!(voice_topology_label(&CallTopology::Mesh), "mesh");
        assert!(
            voice_media_label(&forwarding).contains("forwarding pending"),
            "the CLI must keep the forwarding caveat"
        );
        assert!(
            voice_media_label(&forwarding).contains("keys all-pairs"),
            "the CLI must state the actual keyring"
        );
        assert_eq!(
            voice_media_label(&CallTopology::Mesh),
            "media path: mesh  keys all-pairs"
        );
    }

    #[test]
    fn voice_roles_mark_hub_master_and_backup() {
        let hub = Identity::from_seed([1u8; 32]).author_id();
        let backup = Identity::from_seed([2u8; 32]).author_id();
        let member = Identity::from_seed([3u8; 32]).author_id();

        assert_eq!(
            voice_role_marker(Some(hub), Some(hub), Some(backup), hub),
            Some("[hub]")
        );
        assert_eq!(
            voice_role_marker(None, Some(member), Some(backup), member),
            Some("[master]"),
            "a mesh or direct call still marks its coordinator"
        );
        assert_eq!(
            voice_role_marker(Some(hub), Some(hub), Some(backup), backup),
            Some("[backup]")
        );
        assert_eq!(
            voice_role_marker(Some(hub), Some(hub), Some(backup), member),
            None
        );
    }

    #[test]
    fn commune_alias_and_profile_dm_commands_parse() {
        let guild = Cli::try_parse_from(["ovvc", "commune", "list"]).expect("commune alias");
        assert!(matches!(guild.command, Command::Guild { .. }));

        let profile = Cli::try_parse_from([
            "ovvc",
            "profile",
            "terminal",
            "ada",
            "--about",
            "hello",
            "--pronouns",
            "they/them",
            "--link",
            "https://a.example",
            "--link",
            "https://b.example",
        ])
        .expect("profile flags");
        match profile.command {
            Command::Profile {
                guild,
                name,
                about,
                pronouns,
                link,
            } => {
                assert_eq!(guild, "terminal");
                assert_eq!(name, "ada");
                assert_eq!(about.as_deref(), Some("hello"));
                assert_eq!(pronouns.as_deref(), Some("they/them"));
                assert_eq!(link, ["https://a.example", "https://b.example"]);
            }
            other => panic!("not a profile command: {other:?}"),
        }

        let dm = Cli::try_parse_from(["ovvc", "dm", "terminal", "bob"]).expect("dm");
        assert!(matches!(dm.command, Command::Dm { .. }));
        let dms = Cli::try_parse_from(["ovvc", "dms"]).expect("dms");
        assert!(matches!(dms.command, Command::Dms));
        let member = Cli::try_parse_from(["ovvc", "member", "terminal", "bob"]).expect("member");
        assert!(matches!(member.command, Command::Member { .. }));
    }

    #[test]
    fn profile_validation_matches_the_store_limits() {
        let about = "a".repeat(MAX_PROFILE_ABOUT_BYTES);
        assert!(validate_profile_fields(&about, "", &[]).is_ok());
        let long_about = "a".repeat(MAX_PROFILE_ABOUT_BYTES + 1);
        assert!(validate_profile_fields(&long_about, "", &[]).is_err());

        let pronouns = "p".repeat(MAX_PROFILE_PRONOUNS_BYTES);
        assert!(validate_profile_fields("", &pronouns, &[]).is_ok());
        let long_pronouns = "p".repeat(MAX_PROFILE_PRONOUNS_BYTES + 1);
        assert!(validate_profile_fields("", &long_pronouns, &[]).is_err());

        let five: Vec<String> = (0..MAX_PROFILE_LINKS)
            .map(|index| format!("https://example.com/{index}"))
            .collect();
        assert!(validate_profile_fields("", "", &five).is_ok());
        let mut six = five.clone();
        six.push("https://example.com/5".to_string());
        assert!(validate_profile_fields("", "", &six).is_err());

        let prefix = "https://example.com/";
        let link = format!(
            "{prefix}{}",
            "a".repeat(MAX_PROFILE_LINK_BYTES - prefix.len())
        );
        assert_eq!(link.len(), MAX_PROFILE_LINK_BYTES);
        assert!(validate_profile_fields("", "", &[link]).is_ok());
        let long_link = format!(
            "{prefix}{}",
            "a".repeat(MAX_PROFILE_LINK_BYTES - prefix.len() + 1)
        );
        assert!(validate_profile_fields("", "", &[long_link]).is_err());

        assert!(validate_profile_fields("", "", &["ftp://example.com".to_string()]).is_err());
        assert!(validate_profile_fields("", "", &["https://example.com".to_string()]).is_ok());
    }

    #[test]
    fn profile_lines_show_counts_activity_and_a_self_marker() {
        let me = Identity::from_seed([3u8; 32]).author_id();
        let other = Identity::from_seed([4u8; 32]).author_id();
        let profile = MemberProfile {
            guild_id: GuildId([1; 32]),
            author: me,
            display_name: "Ada".to_string(),
            about: "first\nsecond".to_string(),
            pronouns: "she/her".to_string(),
            links: vec!["https://example.com".to_string()],
            joined_at: 0,
            updated_at: 10,
            message_count: 3,
            last_activity: 0,
        };
        let lines = profile_lines(&profile, me).join("\n");
        assert!(lines.contains("name:       Ada"), "{lines}");
        assert!(
            lines.contains(&format!("author:     {}", me.short())),
            "{lines}"
        );
        assert!(
            lines.contains("joined:     1970-01-01T00:00:00Z"),
            "{lines}"
        );
        assert!(lines.contains("messages:   3"), "{lines}");
        assert!(lines.contains("you:        yes"), "{lines}");
        assert!(lines.contains("pronouns:   she/her"), "{lines}");
        assert!(lines.contains("  first\n  second"), "{lines}");
        assert!(lines.contains("  https://example.com"), "{lines}");

        let lines = profile_lines(&profile, other).join("\n");
        assert!(lines.contains("you:        no"), "{lines}");

        let empty = MemberProfile {
            about: String::new(),
            pronouns: String::new(),
            links: Vec::new(),
            ..profile
        };
        let lines = profile_lines(&empty, me).join("\n");
        assert!(lines.contains("pronouns:   (none)"), "{lines}");
        assert!(lines.contains("about:\n  (not set)"), "{lines}");
        assert!(lines.contains("links:\n  (none)"), "{lines}");
    }

    #[test]
    fn one_shot_failures_carry_a_class() {
        let not_found = anyhow::Error::new(ClientError::Core(ovvc_core::CoreError::NotFound(
            "commune".to_string(),
        )));
        assert_eq!(error_class(&not_found), ErrorClass::User);

        let offline = anyhow::Error::new(ClientError::NotConnected(GuildId([2u8; 32])));
        assert_eq!(error_class(&offline), ErrorClass::Operational);

        let internal = anyhow::Error::new(IpcError::internal("invariant"));
        assert_eq!(error_class(&internal), ErrorClass::Bug);

        // Anything left is an argument or file problem the caller can fix.
        let input = anyhow::anyhow!("unknown presence");
        assert_eq!(error_class(&input), ErrorClass::User);
    }

    #[test]
    fn voice_status_names_the_media_transport_and_connection_quality() {
        let stats = VoiceAudioStats {
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
        };
        let line = voice_audio_line(&stats);
        assert!(line.contains("transport=webrtc"), "{line}");
        assert!(line.contains("rtt=35ms"), "{line}");
        assert!(line.contains("packets_lost=12"), "{line}");
        assert!(line.contains("relayed=true"), "{line}");
        assert!(line.contains("e2ee=true"), "{line}");
        assert!(line.contains("epoch=1"), "{line}");
        assert!(line.contains("e2ee_errors=0"), "{line}");
        // The existing audio counters are still on the same line.
        assert!(line.contains("backend=pipewire"), "{line}");
        assert!(line.contains("latency=20ms"), "{line}");

        let fallback = VoiceAudioStats {
            transport: "libp2p".to_string(),
            rtt_ms: 0,
            packets_lost: 0,
            relayed: false,
            ..stats.clone()
        };
        let line = voice_audio_line(&fallback);
        assert!(line.contains("transport=libp2p"), "{line}");
        assert!(line.contains("rtt=-"), "{line}");
        assert!(line.contains("packets_lost=-"), "{line}");
        assert!(line.contains("relayed=-"), "{line}");

        // A call without a keyring reports no E2EE and epoch zero, so status
        // does not hide a call that is carrying no media.
        let no_key = VoiceAudioStats {
            e2ee: false,
            epoch: 0,
            e2ee_errors: 4,
            ..stats
        };
        let line = voice_audio_line(&no_key);
        assert!(line.contains("e2ee=false"), "{line}");
        assert!(line.contains("epoch=0"), "{line}");
        assert!(line.contains("e2ee_errors=4"), "{line}");
    }
}
