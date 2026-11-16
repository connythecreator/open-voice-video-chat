# ovvc-client

The frontend-facing facade for ovvc. Build a GUI, TUI, web shell, service, or
bot on top of this crate; you should not need to touch `ovvc-core`,
`ovvc-net`, or `ovvc-store` directly.

## What it gives you

* **Local, signed history** — every guild, channel, member, and message is an
  ed25519-signed event in an append-only log. Anything you read back has been
  verified.
* **Optional peer-to-peer networking, per guild** — Kademlia discovery, mDNS,
  direct sync, relay/DCUtR hole punching, UPnP, and NAT-PMP. A client with no
  connected guild is fully usable offline.
* **One event stream** — `Client::subscribe()` returns a `broadcast` receiver of
  `ClientEvent`s: listening addresses, peers connecting, sync completion,
  incoming messages, DHT lookups, errors.

## Crate layering

```
ovvc-core   domain: Identity, EventBody, SignedEvent, ids, Invite, Storage trait
    |
ovvc-store  default Storage backend (redb index + segmented event log)
    |
ovvc-net    libp2p transport: sync protocol, Kademlia, mDNS, relay
    |
ovvc-client this crate: composes storage + networking behind one API
    |
 your frontend
```

Only `ovvc-core` matters to a frontend author, and mostly just its types.
`ovvc-client` re-exports them, so a frontend's `Cargo.toml` needs one
dependency:

```toml
[dependencies]
ovvc-client = { path = "../ovvc-client" }
```

## Typical lifecycle

```rust
use ovvc_client::{Client, ClientEvent, NetworkOptions};

let client = Client::open_local("~/.local/share/ovvc")?;   // identity + store
let guild = client.create_guild("my guild")?;
let channel = client.default_channel(guild.id)?;

let mut events = client.subscribe();
client.connect(guild.id, NetworkOptions::default()).await?;

client.post_message(guild.id, channel.id, "hello")?;        // stored + broadcast

while let Ok(event) = events.recv().await {
    match event {
        ClientEvent::Event { event, .. } => { /* render */ }
        ClientEvent::PeerConnected { peer, .. } => { /* update roster */ }
        _ => {}
    }
}
```

See `examples/offline_bot.rs` for a complete headless frontend.

## Plugging in your own storage

`Storage` is a trait. Implement it (for example over SQLite, IndexedDB, or
memory) and pass the backend in:

```rust
use ovvc_client::{Client, ClientConfig, Identity, StorageConfig, shared_storage};

let client = Client::open(ClientConfig {
    identity: Identity::load_or_create("identity.key".as_ref())?.0,
    storage: StorageConfig::Custom(shared_storage(my_backend)),
})?;
```

The transport only ever touches the trait, so a custom backend still gets
sync, invites, and DHT discovery.

To leave the bundled redb backend out of the dependency graph entirely, disable
the default feature:

```toml
ovvc-client = { path = "../ovvc-client", default-features = false }
```

`Client::open_local` and `StorageConfig::Directory` then disappear; construct
the client with `Client::with_storage` or `StorageConfig::Custom` instead.

## Invites

An invite is a signed statement by a member: "this guild id and name are real,
dial these peers". `Client::create_invite` produces one; `Client::join_guild`
verifies the signature, dials, and waits for the first history sync.

```rust
let invite = client.create_invite(guild.id, vec![dial_addr], Some(3600))?;
println!("{}", invite.encode()?);

client.join_guild(&Invite::decode(encoded)?, NetworkOptions::default()).await?;
```

## What is intentionally *not* here

* No UI, no terminal assumptions, no logging framework, no async runtime choice
  beyond "Tokio is present while networking".
* No server requirement. Networking is a peer graph, not a backend.
* No global mutable state: create as many `Client`s as you have identities.

## Known limits

History is fully replicated among connected peers today. Erasure-coded cold
storage and sharding (the project's longer-term direction) sit behind the same
`Storage` trait, so frontends should not assume history is always local.

A client runs at most one guild-facing transport node. Every node shares the
client identity, so running several at once makes their peer announcements
collide. For a multi-guild frontend, create one client per guild and share the
same storage:

```rust
let other = Client::open(ClientConfig {
    identity: client.identity().clone(),
    storage: StorageConfig::Custom(client.storage()),
})?;
```
