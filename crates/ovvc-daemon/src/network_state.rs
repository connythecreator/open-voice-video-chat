//! The peer-network intent, persisted next to the store.
//!
//! A daemon that restarts used to come up with every guild offline, so two
//! machines that had been talking before a restart silently stopped seeing
//! each other until somebody clicked "go online" again. The daemon writes
//! what it was asked to do here and replays it on the next start.
//!
//! The file records intent, not a snapshot of the live node: `online` says the
//! guild belongs on the peer network, and the addresses are what the frontend
//! asked for. A transient [ go offline ] does not clear it, because quitting a
//! frontend must not silently strand the machine for the next start. A daemon
//! started for an interactive frontend also connects guilds that have no record
//! at all, on an ephemeral port.

use std::fs;
use std::path::{Path, PathBuf};

use ovvc_core::GuildId;
use ovvc_ipc::NetworkOptionsDto;
use serde::{Deserialize, Serialize};

pub const NETWORK_FILE: &str = "network.json";

/// One guild's remembered network settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedNetwork {
    /// Hex guild id. Kept as text so a person can read the file and a bad id
    /// cannot make the whole record unreadable.
    pub guild_id: String,
    /// Whether the person last asked for this guild to be online.
    pub online: bool,
    pub listen: Vec<String>,
    pub dial: Vec<String>,
    #[serde(default)]
    pub relay: Option<String>,
    #[serde(default)]
    pub upnp: bool,
    #[serde(default)]
    pub natpmp: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct NetworkFile {
    #[serde(default)]
    guilds: Vec<SavedNetwork>,
}

impl SavedNetwork {
    pub fn new(guild_id: GuildId, online: bool, options: &NetworkOptionsDto) -> Self {
        Self {
            guild_id: guild_id.to_hex(),
            online,
            listen: options.listen.clone(),
            dial: options.dial.clone(),
            relay: options.relay.clone(),
            upnp: options.upnp,
            natpmp: options.natpmp,
        }
    }

    pub fn guild_id(&self) -> Option<GuildId> {
        self.guild_id.parse().ok()
    }

    pub fn options(&self) -> NetworkOptionsDto {
        NetworkOptionsDto {
            listen: self.listen.clone(),
            dial: self.dial.clone(),
            relay: self.relay.clone(),
            upnp: self.upnp,
            natpmp: self.natpmp,
        }
    }
}

pub fn network_path(data_dir: &Path) -> PathBuf {
    data_dir.join(NETWORK_FILE)
}

/// Read the saved intents. A missing or corrupt file is the same as an empty
/// one: the daemon must still start, and a half-written record must not stop
/// every guild from coming online.
pub fn load(data_dir: &Path) -> Vec<SavedNetwork> {
    fs::read(network_path(data_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<NetworkFile>(&bytes).ok())
        .map(|file| file.guilds)
        .unwrap_or_default()
}

/// Write the whole list atomically, the same way the settings file is
/// written, so a crash leaves the previous answer rather than a truncated one.
pub fn save_all(data_dir: &Path, guilds: &[SavedNetwork]) -> std::io::Result<()> {
    let path = network_path(data_dir);
    let temporary = path.with_extension("json.tmp");
    let file = NetworkFile {
        guilds: guilds.to_vec(),
    };
    let bytes = serde_json::to_vec_pretty(&file).map_err(std::io::Error::other)?;
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, &path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovvc_core::GuildId;

    fn guild(seed: u8) -> GuildId {
        GuildId([seed; 32])
    }

    fn dto(listen: &[&str], dial: &[&str]) -> NetworkOptionsDto {
        NetworkOptionsDto {
            listen: listen.iter().map(|value| value.to_string()).collect(),
            dial: dial.iter().map(|value| value.to_string()).collect(),
            relay: Some("/ip4/10.0.0.1/tcp/4001".to_string()),
            upnp: true,
            natpmp: false,
        }
    }

    #[test]
    fn a_saved_intent_round_trips_through_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let entry = SavedNetwork::new(
            guild(7),
            true,
            &dto(&["/ip4/0.0.0.0/tcp/41000"], &["/ip4/192.0.2.5/tcp/41000"]),
        );
        save_all(dir.path(), std::slice::from_ref(&entry)).expect("save");
        let loaded = load(dir.path());
        assert_eq!(loaded, vec![entry]);
        assert_eq!(loaded[0].guild_id(), Some(guild(7)));
        assert_eq!(
            loaded[0].options().listen,
            vec!["/ip4/0.0.0.0/tcp/41000".to_string()]
        );
    }

    #[test]
    fn taking_a_guild_offline_is_a_record_too() {
        let dir = tempfile::tempdir().expect("temp dir");
        let entry = SavedNetwork::new(guild(3), false, &dto(&[], &[]));
        save_all(dir.path(), std::slice::from_ref(&entry)).expect("save");
        let loaded = load(dir.path());
        assert_eq!(loaded.len(), 1);
        assert!(!loaded[0].online);
    }

    #[test]
    fn a_missing_or_corrupt_file_reads_as_no_intent() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(load(dir.path()).is_empty());
        fs::write(network_path(dir.path()), b"{ not json").expect("write");
        assert!(load(dir.path()).is_empty());
        // An id that will not parse is skipped by the caller rather than
        // making the whole file useless.
        fs::write(
            network_path(dir.path()),
            br#"{"guilds":[{"guild_id":"nonsense","online":true,"listen":[],"dial":[]}]}"#,
        )
        .expect("write");
        let loaded = load(dir.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].guild_id(), None);
    }

    #[test]
    fn unknown_fields_from_a_newer_daemon_do_not_hide_the_rest() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(
            network_path(dir.path()),
            br#"{"guilds":[{"guild_id":"0707070707070707070707070707070707070707070707070707070707070707","online":true,"listen":["/ip4/0.0.0.0/tcp/41000"],"dial":[],"future":42}]}"#,
        )
        .expect("write");
        let loaded = load(dir.path());
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].online);
        assert_eq!(loaded[0].guild_id(), Some(guild(7)));
    }
}
