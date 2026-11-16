#![no_main]

use libfuzzer_sys::fuzz_target;
use ovvc_core::SignedEvent;

fuzz_target!(|data: &[u8]| {
    // The wire/database decoders are the trust boundary. Anything that
    // decodes must be safe to verify, hash and re-encode.
    if data.len() > 64 * 1024 {
        return;
    }

    if let Ok(event) = SignedEvent::from_bytes(data) {
        let _ = event.verify();
        let _ = event.id();
        let _ = event.body.canonical_bytes();
        let _ = event.body.signing_bytes();
        if let Ok(encoded) = event.to_bytes() {
            let _ = SignedEvent::from_bytes(&encoded);
        }
    }
});
