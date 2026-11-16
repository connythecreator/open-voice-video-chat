#![no_main]

use libfuzzer_sys::fuzz_target;
use ovvc_core::Invite;

fuzz_target!(|data: &[u8]| {
    if data.len() > 16 * 1024 {
        return;
    }

    if let Ok(text) = std::str::from_utf8(data)
        && let Ok(invite) = Invite::decode(text)
    {
        let _ = invite.verify();
        let _ = invite.encode();
        let _ = invite.body.addresses.len();
    }
});
