use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use directories::ProjectDirs;
use futures::StreamExt;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{Multiaddr, PeerId, SwarmBuilder, identify, noise, ping, relay, tcp, yamux};
use ovvc_core::Identity;

#[derive(Debug, Parser)]
#[command(
    name = "ovvc-server",
    version,
    about = "Optional relay server for ovvc"
)]
struct Cli {
    /// Multiaddr to listen on. Repeatable.
    #[arg(long, default_value = "/ip4/0.0.0.0/tcp/7000")]
    listen: Vec<String>,

    /// Path to the server identity seed.
    #[arg(long)]
    identity: Option<PathBuf>,
}

#[derive(NetworkBehaviour)]
struct ServerBehaviour {
    relay: relay::Behaviour,
    identify: identify::Behaviour,
    ping: ping::Behaviour,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let identity_path = match cli.identity {
        Some(path) => path,
        None => default_identity_path()?,
    };
    let (identity, created) = Identity::load_or_create(&identity_path)?;
    let mut seed = identity.seed();
    let keypair = libp2p::identity::Keypair::ed25519_from_bytes(&mut seed)
        .context("creating libp2p identity")?;
    let peer_id = keypair.public().to_peer_id();

    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(|key| {
            Ok(ServerBehaviour {
                relay: relay::Behaviour::new(key.public().to_peer_id(), relay::Config::default()),
                identify: identify::Behaviour::new(identify::Config::new(
                    "/ovvc/identify/1".to_string(),
                    key.public(),
                )),
                ping: ping::Behaviour::new(ping::Config::new()),
            })
        })?
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(300)))
        .build();

    for address in &cli.listen {
        let address: Multiaddr = address
            .parse()
            .with_context(|| format!("invalid listen multiaddr: {address}"))?;
        swarm
            .listen_on(address.clone())
            .with_context(|| format!("listening on {address}"))?;
    }

    if created {
        println!("created server identity: {}", identity_path.display());
    }
    println!("relay peer id: {peer_id}");
    println!("share this with clients: <listen-address>/p2p/{peer_id}");

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            event = swarm.select_next_some() => {
                handle_event(event, peer_id);
            }
        }
    }
    println!("shutting down");
    Ok(())
}

fn handle_event(event: SwarmEvent<ServerBehaviourEvent>, peer_id: PeerId) {
    match event {
        SwarmEvent::NewListenAddr { address, .. } => {
            println!("listening: {address}/p2p/{peer_id}");
        }
        SwarmEvent::ConnectionEstablished { peer_id: peer, .. } => {
            println!("connected: {peer}");
        }
        SwarmEvent::ConnectionClosed { peer_id: peer, .. } => {
            println!("disconnected: {peer}");
        }
        SwarmEvent::Behaviour(ServerBehaviourEvent::Relay(event)) => match event {
            relay::Event::ReservationReqAccepted {
                src_peer_id,
                renewed,
            } => {
                let action = if renewed { "renewed" } else { "accepted" };
                println!("reservation {action}: {src_peer_id}");
            }
            relay::Event::ReservationClosed { src_peer_id } => {
                println!("reservation closed: {src_peer_id}");
            }
            relay::Event::CircuitReqAccepted {
                src_peer_id,
                dst_peer_id,
            } => {
                println!("circuit: {src_peer_id} -> {dst_peer_id}");
            }
            _ => {}
        },
        SwarmEvent::OutgoingConnectionError { error, .. } => {
            eprintln!("outgoing connection error: {error:?}");
        }
        SwarmEvent::IncomingConnectionError { error, .. } => {
            eprintln!("incoming connection error: {error:?}");
        }
        _ => {}
    }
}

fn default_identity_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("org", "ovvc", "ovvc")
        .context("could not determine a data directory; pass --identity")?;
    std::fs::create_dir_all(dirs.data_dir())?;
    Ok(dirs.data_dir().join("server-identity.key"))
}
