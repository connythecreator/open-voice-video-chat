use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use ovvc_core::error::CoreError;
use ovvc_core::model::{SignedEvent, supported_event_version};

use crate::error::Result;
use crate::fsutil::create_dir_private;

const SEGMENT_MAGIC: &[u8; 8] = b"OVVCSG01";
const SEGMENT_HEADER_LEN: u64 = 8;
const MAX_SEGMENT_SIZE: u64 = 64 * 1024 * 1024;
/// Largest record the scanner will allocate for. A `u32` length prefix on a
/// sparse file could otherwise claim up to 4 GiB and turn recovery into an
/// out-of-memory abort. Real events are bounded by the core message limit.
const MAX_RECORD_BYTES: u64 = 1024 * 1024;

/// Record headers are a little-endian `u32` payload length. The top bit marks
/// a record written with a trailing CRC32C checksum; the low 31 bits are the
/// payload length. Payloads are bounded well below 2 GiB, so the bit never
/// collides with a legacy length.
///
/// Legacy (`OVVCSG01`) segments contain unflagged records only. A segment file
/// mixes both spellings: records written before the checksum change keep their
/// original framing, and records appended after an upgrade carry the flag. The
/// segment magic is therefore unchanged and existing stores open as before.
const RECORD_CHECKSUM_FLAG: u32 = 0x8000_0000;
const RECORD_LENGTH_MASK: u32 = !RECORD_CHECKSUM_FLAG;
const RECORD_CRC_LEN: u64 = 4;

/// Reflected CRC32C (Castagnoli) polynomial. Implemented locally so the store
/// does not grow a dependency just for four bytes per record.
const CRC32C_POLYNOMIAL: u32 = 0x82f6_3b78;

const fn crc32c_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ CRC32C_POLYNOMIAL
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}

static CRC32C_TABLE: [u32; 256] = crc32c_table();

fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        let slot = ((crc ^ u32::from(byte)) & 0xff) as usize;
        crc = CRC32C_TABLE[slot] ^ (crc >> 8);
    }
    !crc
}

/// A decoded record prefix.
#[derive(Debug, Clone, Copy)]
struct RecordHeader {
    len: u32,
    checksummed: bool,
}

impl RecordHeader {
    fn decode(raw: u32) -> Self {
        Self {
            len: raw & RECORD_LENGTH_MASK,
            checksummed: raw & RECORD_CHECKSUM_FLAG != 0,
        }
    }

    fn encode(payload_len: u32) -> [u8; 4] {
        (payload_len | RECORD_CHECKSUM_FLAG).to_le_bytes()
    }

    /// Total bytes the record occupies, header included.
    fn record_len(self) -> u64 {
        4 + u64::from(self.len) + if self.checksummed { RECORD_CRC_LEN } else { 0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventLocation {
    pub segment: u32,
    pub offset: u64,
    pub len: u32,
}

impl EventLocation {
    pub fn to_bytes(self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[..4].copy_from_slice(&self.segment.to_le_bytes());
        out[4..12].copy_from_slice(&self.offset.to_le_bytes());
        out[12..].copy_from_slice(&self.len.to_le_bytes());
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 16 {
            return Err(CoreError::InvalidInput(format!(
                "event location must be 16 bytes, got {}",
                bytes.len()
            ))
            .into());
        }
        let segment = u32::from_le_bytes(bytes[..4].try_into().expect("length checked"));
        let offset = u64::from_le_bytes(bytes[4..12].try_into().expect("length checked"));
        let len = u32::from_le_bytes(bytes[12..].try_into().expect("length checked"));
        Ok(Self {
            segment,
            offset,
            len,
        })
    }
}

pub struct AppendLog {
    events_dir: PathBuf,
    file: File,
    segment: u32,
    offset: u64,
}

impl AppendLog {
    pub fn open(guild_dir: &Path) -> Result<Self> {
        let events_dir = guild_dir.join("events");
        create_dir_private(&events_dir)?;

        let segments = list_segments(&events_dir)?;
        let segment = segments.last().map(|(id, _)| *id).unwrap_or(1);

        let mut file = open_segment(&events_dir, segment)?;
        let offset = file.metadata()?.len();
        file.seek(SeekFrom::End(0))?;

        Ok(Self {
            events_dir,
            file,
            segment,
            offset,
        })
    }

    pub fn append(&mut self, payload: &[u8]) -> Result<EventLocation> {
        let payload_len =
            u32::try_from(payload.len()).map_err(|_| CoreError::EventTooLarge(payload.len()))?;
        // The scanner refuses to allocate past this bound; never write a record
        // it could not read back.
        if u64::from(payload_len) > MAX_RECORD_BYTES {
            return Err(CoreError::EventTooLarge(payload.len()).into());
        }
        let record_len = 4 + u64::from(payload_len) + RECORD_CRC_LEN;

        if self.offset > SEGMENT_HEADER_LEN
            && self.offset.saturating_add(record_len) > MAX_SEGMENT_SIZE
        {
            self.rotate()?;
        }

        let offset = self.offset;
        let payload_offset = offset + 4;
        let checksum = crc32c(payload);
        let write = (|| -> Result<()> {
            self.file.seek(SeekFrom::Start(offset))?;
            self.file.write_all(&RecordHeader::encode(payload_len))?;
            self.file.write_all(payload)?;
            self.file.write_all(&checksum.to_le_bytes())?;
            self.file.sync_data()?;
            Ok(())
        })();

        if let Err(error) = write {
            // Do not leave a partial record for a later append to sit behind.
            let _ = self.file.set_len(offset);
            let _ = self.file.seek(SeekFrom::Start(offset));
            self.offset = offset;
            return Err(error);
        }

        self.offset = offset + record_len;

        Ok(EventLocation {
            segment: self.segment,
            offset: payload_offset,
            len: payload_len,
        })
    }

    /// Move to the next segment.
    ///
    /// State is only updated once the new segment exists and is open, so a
    /// failure here leaves the log exactly as it was instead of pointing at a
    /// segment that does not exist. A rotation that died after creating the
    /// file (for example on an fd limit) leaves a header-only segment; the next
    /// attempt reuses and repairs it rather than failing with `AlreadyExists`.
    fn rotate(&mut self) -> Result<()> {
        let segment = self
            .segment
            .checked_add(1)
            .ok_or_else(|| CoreError::CorruptLog {
                path: self.events_dir.clone(),
                reason: "segment id exhausted".to_string(),
            })?;
        let mut file = open_segment(&self.events_dir, segment)?;
        let offset = file.metadata()?.len();
        file.seek(SeekFrom::End(0))?;
        self.segment = segment;
        self.file = file;
        self.offset = offset;
        Ok(())
    }
}

pub fn read_event(guild_dir: &Path, location: EventLocation) -> Result<SignedEvent> {
    let path = segment_path(&guild_dir.join("events"), location.segment);
    if location.offset < 4 || u64::from(location.len) > MAX_RECORD_BYTES {
        return Err(CoreError::CorruptLog {
            path,
            reason: format!(
                "indexed record at offset {} has invalid length {}",
                location.offset, location.len
            ),
        }
        .into());
    }
    let mut file = File::open(&path)?;
    file.seek(SeekFrom::Start(location.offset.saturating_sub(4)))?;
    let mut len_bytes = [0u8; 4];
    file.read_exact(&mut len_bytes)?;
    let header = RecordHeader::decode(u32::from_le_bytes(len_bytes));
    if header.len != location.len {
        return Err(CoreError::CorruptLog {
            path,
            reason: format!(
                "record length mismatch: index says {}, file says {}",
                location.len, header.len
            ),
        }
        .into());
    }
    let mut payload = vec![0u8; location.len as usize];
    file.read_exact(&mut payload)?;
    if header.checksummed {
        let mut crc_bytes = [0u8; 4];
        file.read_exact(&mut crc_bytes)?;
        if crc32c(&payload) != u32::from_le_bytes(crc_bytes) {
            return Err(CoreError::CorruptLog {
                path,
                reason: format!(
                    "checksum mismatch for indexed record at offset {}",
                    location.offset
                ),
            }
            .into());
        }
    }
    SignedEvent::from_bytes(&payload).map_err(Into::into)
}

/// Stream every record in a guild's segment logs to `on_event`.
///
/// Each record is decoded, handed to the callback, and dropped before the next
/// one is read, so recovery memory does not grow with a guild's history. The
/// callback may abort recovery by returning an error.
///
/// `verify` controls whether signatures are checked while scanning. Recovery
/// uses `false`: the log is structural data, and every event is verified when
/// it is written to the log (local append or verified import) and again when it
/// is read back. Re-verifying a long history on every open costs seconds.
pub fn scan_guild_streaming<F>(guild_dir: &Path, verify: bool, mut on_event: F) -> Result<()>
where
    F: FnMut(EventLocation, SignedEvent) -> Result<()>,
{
    let events_dir = guild_dir.join("events");
    if !events_dir.exists() {
        return Ok(());
    }

    let segments = list_segments(&events_dir)?;
    for (index, (segment, path)) in segments.iter().enumerate() {
        let scan = scan_segment(path, *segment, verify, &mut on_event)?;
        if scan.torn && index + 1 != segments.len() {
            // Only the segment being appended to can have a torn tail.
            return Err(CoreError::CorruptLog {
                path: path.clone(),
                reason: "torn record in a sealed segment".to_string(),
            }
            .into());
        }
    }
    Ok(())
}

struct SegmentScan {
    /// Byte offset of the end of the last fully valid record.
    valid_end: u64,
    /// A record at the end of the file could not be decoded or verified.
    torn: bool,
}

fn scan_segment<F>(path: &Path, segment: u32, verify: bool, on_event: &mut F) -> Result<SegmentScan>
where
    F: FnMut(EventLocation, SignedEvent) -> Result<()>,
{
    let file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut reader = BufReader::new(file);

    if file_len < SEGMENT_HEADER_LEN {
        // A crash before the header was fully written (for example during
        // rotation). The repair pass recreates the segment.
        return Ok(SegmentScan {
            valid_end: 0,
            torn: true,
        });
    }

    let mut header = [0u8; 8];
    reader
        .read_exact(&mut header)
        .map_err(|_| CoreError::CorruptLog {
            path: path.to_path_buf(),
            reason: "missing segment header".to_string(),
        })?;
    if &header != SEGMENT_MAGIC {
        return Err(CoreError::CorruptLog {
            path: path.to_path_buf(),
            reason: "invalid segment magic".to_string(),
        }
        .into());
    }

    let mut pos = SEGMENT_HEADER_LEN;
    let mut valid_end = SEGMENT_HEADER_LEN;
    let mut torn = false;

    while pos < file_len {
        if pos + 4 > file_len {
            torn = true;
            break;
        }

        let mut len_bytes = [0u8; 4];
        reader.read_exact(&mut len_bytes)?;
        let record = RecordHeader::decode(u32::from_le_bytes(len_bytes));
        let payload_offset = pos + 4;
        let record_end = pos.saturating_add(record.record_len());

        if record_end > file_len {
            torn = true;
            break;
        }

        // A declared length is attacker-controlled disk data. Check it before
        // allocating; a sparse file can make the prefix look valid for a
        // multi-gigabyte record while consuming almost no disk.
        if u64::from(record.len) > MAX_RECORD_BYTES {
            if record_end < file_len {
                return Err(CoreError::CorruptLog {
                    path: path.to_path_buf(),
                    reason: format!(
                        "record length {} at offset {pos} exceeds {MAX_RECORD_BYTES}",
                        record.len
                    ),
                }
                .into());
            }
            torn = true;
            break;
        }

        let mut payload = vec![0u8; record.len as usize];
        reader.read_exact(&mut payload)?;

        // A checksum failure at the tail is exactly what a power loss during
        // the final write looks like: the full-sized record reached the file
        // but not all of its bytes are the ones that were meant to be there.
        // Repair may truncate it. Anywhere earlier it is real corruption: error
        // out without touching the good records that follow it.
        if record.checksummed {
            let mut crc_bytes = [0u8; 4];
            reader.read_exact(&mut crc_bytes)?;
            if crc32c(&payload) != u32::from_le_bytes(crc_bytes) {
                if record_end < file_len {
                    return Err(CoreError::CorruptLog {
                        path: path.to_path_buf(),
                        reason: format!("checksum mismatch at offset {pos}"),
                    }
                    .into());
                }
                torn = true;
                break;
            }
        }

        // A record that cannot be decoded (or, when asked, does not verify) is
        // treated exactly like a torn one: valid data ends before it. A bad
        // record in the middle of a segment is real corruption and must not be
        // silently dropped.
        let decoded = SignedEvent::from_bytes(&payload).and_then(|event| {
            // Cheap structural checks always run. A crash can leave a
            // zero-filled payload that still decodes into an event-shaped
            // struct; it must not be treated as durable data.
            if !supported_event_version(event.body.version) || event.signature.len() != 64 {
                return Err(CoreError::InvalidSignature);
            }
            if verify {
                event.verify()?;
            }
            Ok(event)
        });
        match decoded {
            Ok(event) => {
                pos = record_end;
                valid_end = record_end;
                on_event(
                    EventLocation {
                        segment,
                        offset: payload_offset,
                        len: record.len,
                    },
                    event,
                )?;
            }
            Err(_) if record_end < file_len => {
                return Err(CoreError::CorruptLog {
                    path: path.to_path_buf(),
                    reason: format!("invalid record at offset {pos}"),
                }
                .into());
            }
            Err(_) => {
                torn = true;
                break;
            }
        }
    }

    Ok(SegmentScan { valid_end, torn })
}

fn repair_last_segment(path: &Path, segment: u32) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(CoreError::CorruptLog {
            path: path.to_path_buf(),
            reason: "segment must be a regular file".to_string(),
        }
        .into());
    }
    let len = metadata.len();
    if len < SEGMENT_HEADER_LEN {
        let mut file = OpenOptions::new().write(true).truncate(true).open(path)?;
        file.write_all(SEGMENT_MAGIC)?;
        file.sync_all()?;
        return Ok(());
    }

    // Keep only the last decoded event; the scan itself is streaming.
    let mut last: Option<(EventLocation, SignedEvent)> = None;
    let mut scan = scan_segment(path, segment, false, &mut |location, event| {
        last = Some((location, event));
        Ok(())
    })?;
    // Structural scanning skips signatures for speed, but a tail record whose
    // signature does not verify (bit rot, or a payload that was persisted in
    // full yet corrupted) must still be cut: recovery verifies events it is
    // about to index, and would otherwise fail to open the store at all. A
    // checksummed record is already covered by the scan above; this catches
    // legacy checksum-less records.
    if !scan.torn
        && let Some((location, event)) = &last
        && event.verify().is_err()
    {
        scan.valid_end = location.offset.saturating_sub(4);
        scan.torn = true;
    }
    if scan.torn && scan.valid_end < len {
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(scan.valid_end)?;
        file.sync_all()?;
    }
    Ok(())
}

/// Open the canonical file for `segment`, creating it or repairing a
/// crash-torn tail first. Only regular files are accepted: following a symlink
/// here would let a modified data directory truncate or overwrite a file
/// outside the store.
fn open_segment(events_dir: &Path, segment: u32) -> Result<File> {
    let path = segment_path(events_dir, segment);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(CoreError::CorruptLog {
                    path,
                    reason: "segment must be a regular file".to_string(),
                }
                .into());
            }
            // A crash during append can leave a partial record at the end of
            // the last segment. Left alone it becomes a permanent hole: every
            // open stops scanning there, and the next append writes *after*
            // the garbage. Truncate back to the last fully valid record.
            repair_last_segment(&path, segment)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => create_segment(&path)?,
        Err(error) => return Err(error.into()),
    }
    let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
    // New segments are created with the process umask, which is usually 0644:
    // the signed events are plaintext, so make the file owner-only.
    crate::fsutil::restrict(&path, 0o600)?;
    validate_header(&mut file, &path)?;
    Ok(file)
}

fn list_segments(events_dir: &Path) -> Result<Vec<(u32, PathBuf)>> {
    let mut segments = Vec::new();
    if !events_dir.exists() {
        return Ok(segments);
    }
    for entry in fs::read_dir(events_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("seg") {
            continue;
        }
        let Some(stem) = path
            .file_stem()
            .and_then(|value| value.to_str())
            .map(str::to_owned)
        else {
            return Err(CoreError::CorruptLog {
                path,
                reason: "segment filename is not valid UTF-8".to_string(),
            }
            .into());
        };
        let Ok(id) = stem.parse::<u32>() else {
            return Err(CoreError::CorruptLog {
                path,
                reason: format!("invalid segment filename {stem:?}"),
            }
            .into());
        };
        // Only the exact `%08d.seg` spelling that `segment_path` produces is
        // addressable by appends and reads. Accepting `1.seg` or `01.seg`
        // would scan a different file than the one written next, silently
        // forking the log.
        if id == 0 || format!("{id:08}") != stem {
            return Err(CoreError::CorruptLog {
                path,
                reason: format!("non-canonical segment filename {stem:?}"),
            }
            .into());
        }
        if !entry.file_type()?.is_file() {
            return Err(CoreError::CorruptLog {
                path,
                reason: "segment must be a regular file".to_string(),
            }
            .into());
        }
        segments.push((id, path));
    }
    segments.sort_by_key(|(id, _)| *id);
    Ok(segments)
}

fn segment_path(events_dir: &Path, segment: u32) -> PathBuf {
    events_dir.join(format!("{segment:08}.seg"))
}

fn create_segment(path: &Path) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(SEGMENT_MAGIC)?;
    file.sync_all()?;
    Ok(())
}

fn validate_header(file: &mut File, path: &Path) -> Result<()> {
    let len = file.metadata()?.len();
    if len < SEGMENT_HEADER_LEN {
        return Err(CoreError::CorruptLog {
            path: path.to_path_buf(),
            reason: "segment smaller than header".to_string(),
        }
        .into());
    }
    file.seek(SeekFrom::Start(0))?;
    let mut header = [0u8; 8];
    file.read_exact(&mut header)?;
    if &header != SEGMENT_MAGIC {
        return Err(CoreError::CorruptLog {
            path: path.to_path_buf(),
            reason: "invalid segment magic".to_string(),
        }
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::Write;

    use ovvc_core::error::CoreError;
    use ovvc_core::model::{ChannelId, EVENT_VERSION, EventBody, EventKind, GuildId, SignedEvent};
    use ovvc_core::{Identity, SyncCursor};

    use super::*;
    use crate::StoreError;

    /// The standard CRC32C check value; catches a wrong polynomial or reflection.
    #[test]
    fn crc32c_matches_the_standard_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
        assert_eq!(crc32c(b""), 0);
    }

    fn signed(body: &str) -> SignedEvent {
        let identity = Identity::from_seed([7u8; 32]);
        identity
            .sign_body(EventBody {
                version: EVENT_VERSION,
                guild_id: GuildId([1u8; 32]),
                channel_id: Some(ChannelId([2u8; 16])),
                author: identity.author_id(),
                seq: 1,
                lamport: 1,
                timestamp: 0,
                prev: None,
                kind: EventKind::Message {
                    body: body.to_string(),
                },
            })
            .unwrap()
    }

    fn guild_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn record_headers_distinguish_legacy_and_checksummed_records() {
        let legacy = RecordHeader::decode(9);
        assert_eq!(legacy.len, 9);
        assert!(!legacy.checksummed);
        assert_eq!(legacy.record_len(), 4 + 9);

        let raw = u32::from_le_bytes(RecordHeader::encode(9));
        let flagged = RecordHeader::decode(raw);
        assert_eq!(flagged.len, 9);
        assert!(flagged.checksummed);
        assert_eq!(flagged.record_len(), 4 + 9 + RECORD_CRC_LEN);
    }

    #[test]
    fn event_locations_roundtrip_through_eight_bytes() {
        let location = EventLocation {
            segment: 3,
            offset: 4,
            len: 17,
        };
        let bytes = location.to_bytes();
        assert_eq!(EventLocation::from_bytes(&bytes).unwrap(), location);
        let error = EventLocation::from_bytes(&bytes[..15]).unwrap_err();
        assert!(matches!(error, StoreError::Core(CoreError::InvalidInput(ref m)) if m.contains("16 bytes")));
    }

    #[test]
    fn append_and_read_roundtrip_and_reopen_continues_the_segment() {
        let dir = guild_dir();
        let event = signed("hello");
        let payload = event.to_bytes().unwrap();

        let mut log = AppendLog::open(dir.path()).unwrap();
        let first = log.append(&payload).unwrap();
        assert_eq!(first.segment, 1);
        assert_eq!(first.offset, SEGMENT_HEADER_LEN + 4);
        assert_eq!(first.len, payload.len() as u32);
        assert_eq!(read_event(dir.path(), first).unwrap().body, event.body);
        drop(log);

        let mut reopened = AppendLog::open(dir.path()).unwrap();
        let second = reopened.append(&payload).unwrap();
        assert!(second.offset > first.offset);
        assert_eq!(read_event(dir.path(), second).unwrap().body, event.body);
    }

    #[test]
    fn append_rejects_an_oversized_record() {
        let dir = guild_dir();
        let mut log = AppendLog::open(dir.path()).unwrap();
        let bytes = vec![0u8; MAX_RECORD_BYTES as usize + 1];
        let error = log.append(&bytes).unwrap_err();
        assert!(matches!(error, StoreError::Core(CoreError::EventTooLarge(_))));
    }

    #[test]
    fn read_event_rejects_impossible_indexes() {
        let dir = guild_dir();
        let event = signed("hello");
        let mut log = AppendLog::open(dir.path()).unwrap();
        let location = log.append(&event.to_bytes().unwrap()).unwrap();

        let missing = read_event(dir.path(), EventLocation { offset: 2, ..location }).unwrap_err();
        assert!(matches!(missing, StoreError::Core(CoreError::CorruptLog { .. })));

        let oversized = read_event(
            dir.path(),
            EventLocation {
                len: MAX_RECORD_BYTES as u32 + 1,
                ..location
            },
        )
        .unwrap_err();
        assert!(matches!(oversized, StoreError::Core(CoreError::CorruptLog { .. })));

        let mismatch = read_event(
            dir.path(),
            EventLocation {
                len: location.len + 1,
                ..location
            },
        )
        .unwrap_err();
        assert!(matches!(mismatch, StoreError::Core(CoreError::CorruptLog { .. })));
    }

    #[test]
    fn read_event_detects_a_corrupt_checksum() {
        let dir = guild_dir();
        let event = signed("hello");
        let mut log = AppendLog::open(dir.path()).unwrap();
        let location = log.append(&event.to_bytes().unwrap()).unwrap();
        drop(log);

        let path = segment_path(&dir.path().join("events"), location.segment);
        let mut file = OpenOptions::new().read(true).write(true).open(path).unwrap();
        use std::io::{Read, Seek, SeekFrom};
        file.seek(SeekFrom::Start(location.offset + 1)).unwrap();
        let mut byte = [0u8; 1];
        file.read_exact(&mut byte).unwrap();
        file.seek(SeekFrom::Start(location.offset + 1)).unwrap();
        file.write_all(&[byte[0] ^ 0xFF]).unwrap();
        file.sync_all().unwrap();

        let error = read_event(dir.path(), location).unwrap_err();
        assert!(matches!(error, StoreError::Core(CoreError::CorruptLog { .. })));
    }

    #[test]
    fn scanning_reports_events_and_tolerates_a_torn_tail() {
        let dir = guild_dir();
        let event = signed("hello");
        let mut log = AppendLog::open(dir.path()).unwrap();
        log.append(&event.to_bytes().unwrap()).unwrap();
        drop(log);

        let path = segment_path(&dir.path().join("events"), 1);
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(&[0xAA, 0xBB, 0xCC]).unwrap();

        let mut seen = Vec::new();
        scan_guild_streaming(dir.path(), true, |location, event| {
            seen.push((location, event));
            Ok(())
        })
        .unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].1.body, event.body);

        // Reopening repairs the torn tail, and the next append starts cleanly.
        let mut repaired = AppendLog::open(dir.path()).unwrap();
        let second = repaired.append(&event.to_bytes().unwrap()).unwrap();
        assert_eq!(read_event(dir.path(), second).unwrap().body, event.body);
    }

    #[test]
    fn a_missing_events_directory_scans_as_empty() {
        let dir = guild_dir();
        let mut calls = 0;
        scan_guild_streaming(dir.path(), false, |_, _| {
            calls += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, 0);
    }

    #[test]
    fn a_bad_segment_magic_is_corruption() {
        let dir = guild_dir();
        let events = dir.path().join("events");
        fs::create_dir_all(&events).unwrap();
        fs::write(segment_path(&events, 1), b"NOTOVVC!").unwrap();
        let error = scan_guild_streaming(dir.path(), false, |_, _| Ok(())).unwrap_err();
        assert!(matches!(error, StoreError::Core(CoreError::CorruptLog { .. })));
    }

    #[test]
    fn a_header_only_last_segment_is_repaired() {
        let dir = guild_dir();
        let events = dir.path().join("events");
        fs::create_dir_all(&events).unwrap();
        fs::write(segment_path(&events, 1), b"OVV").unwrap();
        let mut log = AppendLog::open(dir.path()).unwrap();
        let event = signed("hello");
        let location = log.append(&event.to_bytes().unwrap()).unwrap();
        assert_eq!(read_event(dir.path(), location).unwrap().body, event.body);
    }

    #[test]
    fn list_segments_rejects_non_canonical_names_and_directories() {
        let dir = guild_dir();
        let events = dir.path().join("events");
        fs::create_dir_all(&events).unwrap();

        fs::write(events.join("1.seg"), SEGMENT_MAGIC).unwrap();
        assert!(list_segments(&events).is_err());
        fs::remove_file(events.join("1.seg")).unwrap();

        fs::write(events.join("00000000.seg"), SEGMENT_MAGIC).unwrap();
        assert!(list_segments(&events).is_err());
        fs::remove_file(events.join("00000000.seg")).unwrap();

        fs::create_dir(events.join("00000001.seg")).unwrap();
        assert!(list_segments(&events).is_err());
        fs::remove_dir(events.join("00000001.seg")).unwrap();

        fs::write(events.join("notes.txt"), b"ignored").unwrap();
        assert!(list_segments(&events).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_segment_is_refused() {
        let dir = guild_dir();
        let events = dir.path().join("events");
        fs::create_dir_all(&events).unwrap();
        let real = dir.path().join("outside");
        fs::write(&real, SEGMENT_MAGIC).unwrap();
        std::os::unix::fs::symlink(&real, segment_path(&events, 1)).unwrap();
        assert!(AppendLog::open(dir.path()).is_err());
    }

    #[test]
    fn sync_cursor_roundtrip_from_a_stored_event() {
        let event = signed("hello");
        let cursor = SyncCursor::after(&event).unwrap();
        assert_eq!(cursor.lamport, 1);
        assert_eq!(cursor.event_id, Some(event.id().unwrap()));
    }
}
