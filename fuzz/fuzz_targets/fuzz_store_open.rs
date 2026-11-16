#![no_main]

use std::fs;

use libfuzzer_sys::fuzz_target;
use ovvc_store::Store;

/// `GuildId([1u8; 32]).to_hex()`. The seed corpus writes a valid event whose
/// body carries this guild id, so the matching directory is exercised; other
/// bytes must be rejected or repaired without panicking.
const GUILD_HEX: &str = "0101010101010101010101010101010101010101010101010101010101010101";

fuzz_target!(|data: &[u8]| {
    if data.len() > 128 * 1024 {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let events = dir.path().join("guilds").join(GUILD_HEX).join("events");
    fs::create_dir_all(&events).unwrap();
    fs::write(events.join("00000001.seg"), data).unwrap();

    // Open runs header validation, tail repair, structural scanning, index
    // pruning and recovery. A corrupt directory must never panic or hang.
    let _ = Store::open(dir.path());
});
