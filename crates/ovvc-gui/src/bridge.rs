//! The bridge between egui's synchronous frame loop and the async daemon
//! client. A background thread owns the tokio runtime; the UI exchanges plain
//! messages with it and never blocks on the socket.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ovvc_core::{
    Attachment, AuthorId, ChannelId, EventId, GuildId, MAX_ATTACHMENT_BYTES, attachment_for_file,
};
use ovvc_ipc::{DaemonClient, Op, Reply};

use crate::state::{Message, RequestTag, StagedAttachment};

enum Command {
    Call {
        op: Op,
        tag: RequestTag,
    },
    StageAttachment {
        path: PathBuf,
    },
    SendRich {
        send_id: u64,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: String,
        reply_to: Option<EventId>,
        mentions: Vec<AuthorId>,
        attachments: Vec<StagedAttachment>,
    },
    SaveAttachment {
        guild_id: GuildId,
        attachment: Attachment,
    },
}

pub struct Bridge {
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
    inbox: tokio::sync::mpsc::UnboundedReceiver<Message>,
}

impl Bridge {
    /// Start the daemon client on its own runtime and thread.
    ///
    /// `ctx` is used only to wake egui when a reply or event arrives.
    pub fn start(socket: PathBuf, ctx: egui::Context) -> Self {
        Self::start_with_cache(socket, ctx, attachment_cache_dir())
    }

    /// Start the bridge with an explicit download directory.
    ///
    /// Tests use this so a download lands in a temporary directory instead of
    /// the person's cache; the running application always passes the real one.
    pub fn start_with_cache(socket: PathBuf, ctx: egui::Context, cache_dir: PathBuf) -> Self {
        let (commands, command_rx) = tokio::sync::mpsc::unbounded_channel();
        let (message_tx, inbox) = tokio::sync::mpsc::unbounded_channel();
        let spawned = std::thread::Builder::new()
            .name("ovvc-gui-bridge".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = message_tx.send(Message::BridgeError(format!(
                            "could not start the daemon client runtime: {error}"
                        )));
                        ctx.request_repaint();
                        return;
                    }
                };
                runtime.block_on(supervisor(socket, cache_dir, command_rx, message_tx, ctx));
            });
        if let Err(error) = spawned {
            // The thread could not start; the UI will simply stay offline.
            eprintln!("ovvc-gui: could not start bridge thread: {error}");
        }
        Self { commands, inbox }
    }

    pub fn send(&self, op: Op, tag: RequestTag) {
        let _ = self.commands.send(Command::Call { op, tag });
    }

    /// Read a file and stage it for the next send. The read happens on a
    /// blocking worker, so a slow disk cannot stall the frame.
    pub fn stage_attachment(&self, path: PathBuf) {
        let _ = self.commands.send(Command::StageAttachment { path });
    }

    /// Upload the staged files, then post one message announcing them.
    ///
    /// The daemon must have the bytes before a message names them, otherwise a
    /// peer asks for a file that does not exist and the author cannot serve it.
    #[allow(clippy::too_many_arguments)]
    pub fn send_rich(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        body: String,
        reply_to: Option<EventId>,
        mentions: Vec<AuthorId>,
        attachments: Vec<StagedAttachment>,
        send_id: u64,
    ) {
        let _ = self.commands.send(Command::SendRich {
            send_id,
            guild_id,
            channel_id,
            body,
            reply_to,
            mentions,
            attachments,
        });
    }

    /// Fetch an attachment into the cache and report where it landed.
    pub fn save_attachment(&self, guild_id: GuildId, attachment: Attachment) {
        let _ = self.commands.send(Command::SaveAttachment {
            guild_id,
            attachment,
        });
    }

    /// Drain every message that arrived since the last frame.
    pub fn poll(&mut self) -> Vec<Message> {
        let mut messages = Vec::new();
        while let Ok(message) = self.inbox.try_recv() {
            messages.push(message);
        }
        messages
    }
}

async fn supervisor(
    socket: PathBuf,
    cache_dir: PathBuf,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<Command>,
    messages: tokio::sync::mpsc::UnboundedSender<Message>,
    ctx: egui::Context,
) {
    let client = Arc::new(DaemonClient::new(socket));
    let mut events = client.subscribe();
    let mut ticker = tokio::time::interval(Duration::from_millis(750));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else {
                    return;
                };
                let client = Arc::clone(&client);
                let messages = messages.clone();
                let ctx = ctx.clone();
                let cache_dir = cache_dir.clone();
                tokio::spawn(async move {
                    match command {
                        Command::Call { op, tag } => {
                            let result = client.call(op).await;
                            let _ = messages.send(Message::Reply { tag, result });
                        }
                        Command::StageAttachment { path } => {
                            let result = stage_attachment(path.clone()).await;
                            let _ = messages.send(Message::AttachmentStaged { path, result });
                        }
                        Command::SendRich {
                            send_id,
                            guild_id,
                            channel_id,
                            body,
                            reply_to,
                            mentions,
                            attachments,
                        } => {
                            let result = send_rich(
                                &client,
                                guild_id,
                                channel_id,
                                body,
                                reply_to,
                                mentions,
                                attachments,
                            )
                            .await;
                            let _ = messages.send(Message::Reply {
                                tag: RequestTag::SendRichMessage(send_id),
                                result,
                            });
                        }
                        Command::SaveAttachment {
                            guild_id,
                            attachment,
                        } => {
                            let name = attachment.name.clone();
                            let result =
                                save_attachment(&client, &cache_dir, guild_id, attachment).await;
                            let _ = messages.send(Message::AttachmentSaved { name, result });
                        }
                    }
                    ctx.request_repaint();
                });
            }
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        let _ = messages.send(Message::Event(event));
                        ctx.request_repaint();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
            _ = ticker.tick() => {
                let _ = messages.send(Message::DaemonState(client.state()));
                ctx.request_repaint();
            }
        }
    }
}

/// Read a file and build the metadata the message will announce.
///
/// The size is checked from the file's metadata before reading, so pointing the
/// composer at a multi-gigabyte file fails immediately instead of allocating the
/// whole thing, and it is checked again on the bytes actually read in case the
/// file grew between the two calls.
async fn stage_attachment(path: PathBuf) -> Result<StagedAttachment, String> {
    tokio::task::spawn_blocking(move || {
        let metadata = std::fs::metadata(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        if !metadata.is_file() {
            return Err(format!("{} is not a regular file", path.display()));
        }
        if metadata.len() > MAX_ATTACHMENT_BYTES {
            return Err(format!(
                "{} is {}, over the {} limit",
                path.display(),
                ovvc_core::human_size(metadata.len()),
                ovvc_core::human_size(MAX_ATTACHMENT_BYTES)
            ));
        }
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
            return Err(format!(
                "{} grew past the {} limit while it was being read",
                path.display(),
                ovvc_core::human_size(MAX_ATTACHMENT_BYTES)
            ));
        }
        let attachment = attachment_for_file(&path, &bytes).map_err(|error| error.to_string())?;
        Ok(StagedAttachment {
            path,
            attachment,
            bytes,
        })
    })
    .await
    .map_err(|error| format!("the file read task failed: {error}"))?
}

/// Upload every staged file, then post one message that names them.
///
/// The uploads happen first and in order: a message that names bytes the daemon
/// cannot serve would leave every receiver asking for a file nobody has.
async fn send_rich(
    client: &DaemonClient,
    guild_id: GuildId,
    channel_id: ChannelId,
    body: String,
    reply_to: Option<EventId>,
    mentions: Vec<AuthorId>,
    attachments: Vec<StagedAttachment>,
) -> Result<Reply, ovvc_ipc::IpcError> {
    for staged in &attachments {
        match client
            .call(Op::PutBlob {
                bytes: staged.bytes.clone(),
            })
            .await?
        {
            Reply::BlobId(stored) if stored == staged.attachment.hash => {}
            Reply::BlobId(stored) => {
                return Err(ovvc_ipc::IpcError::internal(format!(
                    "the daemon stored the file as {stored}, expected {}",
                    staged.attachment.hash
                )));
            }
            _ => {
                return Err(ovvc_ipc::IpcError::internal(
                    "the daemon sent an unexpected reply to a blob upload".to_string(),
                ));
            }
        }
    }
    let announced: Vec<Attachment> = attachments
        .into_iter()
        .map(|staged| staged.attachment)
        .collect();
    client
        .call(Op::PostMessageRich {
            guild_id,
            channel_id,
            body,
            reply_to,
            mentions,
            attachments: announced,
        })
        .await
}

/// Fetch a blob and write it into the attachment cache.
async fn save_attachment(
    client: &DaemonClient,
    cache_dir: &Path,
    guild_id: GuildId,
    attachment: Attachment,
) -> Result<PathBuf, String> {
    let bytes = match client
        .call(Op::FetchBlob {
            guild_id,
            hash: attachment.hash,
        })
        .await
        .map_err(|error| error.message)?
    {
        Reply::Blob(Some(bytes)) => bytes,
        Reply::Blob(None) => {
            return Err("the daemon does not have these bytes yet".to_string());
        }
        _ => return Err("the daemon sent an unexpected reply to a blob fetch".to_string()),
    };
    // The network layer already checks a fetched blob against its hash, but the
    // bytes cross the socket on the way here, so check once more before they
    // are written under a name that claims that hash.
    if ovvc_core::hash_bytes(&bytes) != attachment.hash {
        return Err(format!(
            "the daemon returned bytes that do not match {}",
            attachment.hash
        ));
    }
    let directory = cache_dir.to_path_buf();
    tokio::task::spawn_blocking(move || write_cached(&directory, &attachment, &bytes))
        .await
        .map_err(|error| format!("the download task failed: {error}"))?
}

/// Where downloaded attachments land.
fn attachment_cache_dir() -> PathBuf {
    directories::ProjectDirs::from("org", "ovvc", "ovvc")
        .map(|directories| directories.cache_dir().join("attachments"))
        .unwrap_or_else(|| std::env::temp_dir().join("ovvc-attachments"))
}

/// Write the bytes under a name that can never escape the cache directory.
///
/// The announced name is only a hint from a peer, so it is reduced to safe
/// characters and prefixed with the content hash, which keeps two files with the
/// same name apart and means a name like `..` cannot climb out of the directory.
fn write_cached(
    directory: &Path,
    attachment: &Attachment,
    bytes: &[u8],
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    let name = safe_file_name(&attachment.name);
    let path = directory.join(format!("{}-{name}", attachment.hash.short()));
    // Write beside the target and rename, so an interrupted download never
    // leaves a truncated file that looks complete on the next open.
    let temporary = directory.join(format!(
        ".{}-{}.{}.tmp",
        attachment.hash.short(),
        std::process::id(),
        name
    ));
    if let Err(error) = std::fs::write(&temporary, bytes) {
        // A failed write can still leave the temp file behind, and a hidden
        // `.tmp` next to a half-written download is exactly what the rename
        // dance exists to avoid.
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("could not write {}: {error}", temporary.display()));
    }
    if let Err(error) = std::fs::rename(&temporary, &path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("could not finish {}: {error}", path.display()));
    }
    Ok(path)
}

/// Keep the characters a file manager will display without escaping anything.
fn safe_file_name(name: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announced_names_cannot_escape_the_cache_directory() {
        assert_eq!(safe_file_name("holiday.png"), "holiday.png");
        assert_eq!(safe_file_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(safe_file_name(".."), "attachment");
        assert_eq!(safe_file_name(""), "attachment");
        assert_eq!(safe_file_name("a\u{1}b"), "a_b");
        assert_eq!(safe_file_name(&"x".repeat(200)).len(), 80);
        assert!(!safe_file_name(&format!("{}.txt", "..".repeat(50))).starts_with('.'));
    }
}
