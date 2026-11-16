#![no_main]

use libfuzzer_sys::fuzz_target;
use ovvc_net::announce::Announcement;

fuzz_target!(|data: &[u8]| {
    if data.len() > 8 * 1024 {
        return;
    }

    if let Ok(announcement) = Announcement::decode(data) {
        let expected = announcement.body.author;
        let _ = announcement.verify(expected);
        let _ = announcement.encode();
    }
});
