//! A minimal headless frontend built on `ovvc-client`.
//!
//! Run with `cargo run -p ovvc-client --example offline_bot`. It shows the
//! whole lifecycle a GUI or TUI would use: open local state, create a guild,
//! post, read history, and render events.
//!
//! The same code works with networking enabled; replace `Client::open_local`
//! with a `ClientConfig` and call `client.connect(guild.id, ..)` once the user
//! asks to go online.

use ovvc_client::{Client, EventKind};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = tempfile::tempdir()?;
    let client = Client::open_local(data_dir.path())?;

    println!("identity: {}", client.author_id().short());

    let guild = client.create_guild("example guild")?;
    let channel = client.default_channel(guild.id)?;
    println!("created {} / #{}", guild.name, channel.name);

    client.post_message(guild.id, channel.id, "hello from a custom frontend")?;
    client.post_message(guild.id, channel.id, "no terminal required")?;

    for event in client.history(guild.id, channel.id, 10)? {
        let EventKind::Message { body } = &event.body.kind else {
            continue;
        };
        println!("{}: {body}", event.body.author.short());
    }

    Ok(())
}
