//! Generate a small valid corpus so the fuzzers start inside the decoders
//! instead of spending their budget rejecting empty input.
//!
//! Run with:
//!   cargo test --manifest-path fuzz/Cargo.toml --test seeds

use std::fs;
use std::path::{Path, PathBuf};

use ovvc_core::model::{ChannelId, EVENT_VERSION, EventBody, EventKind, GuildId, SignedEvent};
use ovvc_core::{Identity, Invite};
use ovvc_net::announce::Announcement;

fn seed_path(root: &Path, target: &str, name: &str) -> PathBuf {
    let dir = root.join("corpus").join(target);
    fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn write_seed(root: &Path, target: &str, name: &str, bytes: &[u8]) {
    fs::write(seed_path(root, target, name), bytes).unwrap();
}

#[test]
fn generate_corpus_seeds() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let identity = Identity::from_seed([7u8; 32]);
    let guild_id = GuildId([1u8; 32]);
    let channel_id = ChannelId([2u8; 16]);

    let body = EventBody {
        version: EVENT_VERSION,
        guild_id,
        channel_id: Some(channel_id),
        author: identity.author_id(),
        seq: 1,
        lamport: 1,
        timestamp: 0,
        prev: None,
        kind: EventKind::Message {
            body: "fuzz seed".to_string(),
        },
    };
    let signature = identity.sign_bytes(&body.signing_bytes().unwrap());
    let event = SignedEvent { body, signature };
    let event_bytes = event.to_bytes().unwrap();
    write_seed(root, "fuzz_event", "valid_message", &event_bytes);

    let invite = Invite::new(
        guild_id,
        "fuzz guild".to_string(),
        &identity,
        vec!["/ip4/127.0.0.1/tcp/4001".to_string()],
        None,
    )
    .unwrap();
    write_seed(
        root,
        "fuzz_invite",
        "valid_invite",
        invite.encode().unwrap().as_bytes(),
    );

    let announcement = Announcement::sign(
        &identity,
        vec!["/ip4/127.0.0.1/tcp/4001".to_string()],
        3600,
    )
    .unwrap();
    write_seed(
        root,
        "fuzz_announcement",
        "valid_announcement",
        &announcement.encode().unwrap(),
    );

    let mut segment = Vec::new();
    segment.extend_from_slice(b"OVVCSG01");
    segment.extend_from_slice(&(event_bytes.len() as u32).to_le_bytes());
    segment.extend_from_slice(&event_bytes);
    write_seed(root, "fuzz_store_open", "valid_segment", &segment);

    let mut truncated = segment.clone();
    truncated.pop();
    write_seed(root, "fuzz_store_open", "truncated_record", &truncated);

    let mut zero_tail = segment.clone();
    zero_tail.extend_from_slice(&[0u8; 16]);
    write_seed(root, "fuzz_store_open", "zero_tail", &zero_tail);
}
