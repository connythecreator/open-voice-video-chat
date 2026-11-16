//! Content-addressed attachments.
//!
//! An attachment is announced in a message as a hash, size, media type and a
//! display name. The bytes live outside the event log, keyed by the hash, and
//! a peer fetches them from anyone that has them. The same layer is what later
//! carries guild emoji and image previews, so it is deliberately independent
//! of any one message kind.
//!
//! The hash is BLAKE3 over the exact bytes. A message never carries the bytes
//! themselves: that would make the signed log unbounded and force every peer
//! to keep a copy of every file just to verify history.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};

/// The most attachments one message may announce.
pub const MAX_ATTACHMENTS_PER_MESSAGE: usize = 10;
/// The largest single attachment this implementation stores or serves.
///
/// 25 MiB is large enough for a screenshot or a short clip and small enough
/// that a hostile peer cannot use one message to fill a disk. The limit is
/// enforced when a message is signed, when it is imported, and when a blob is
/// stored.
pub const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
/// Longest accepted display name, in bytes.
pub const MAX_ATTACHMENT_NAME_BYTES: usize = 200;
/// Longest accepted media type, in bytes.
pub const MAX_ATTACHMENT_MIME_BYTES: usize = 100;
/// Chunk size for blob transfers.
///
/// This stays well below the request-response codec's response limit, so a
/// chunk plus its envelope always fits, and it is a multiple of 4 KiB so a
/// receiver can write it without reassembly.
pub const BLOB_CHUNK_BYTES: usize = 256 * 1024;

/// The BLAKE3 hash that names one blob.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AttachmentId(pub [u8; 32]);

impl AttachmentId {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn short(&self) -> String {
        self.to_hex()[..8].to_string()
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 32 {
            return Err(CoreError::InvalidId(format!(
                "expected 32 bytes, got {}",
                bytes.len()
            )));
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(bytes);
        Ok(Self(out))
    }

    /// True for the all-zero id, which is reserved for "no blob".
    pub fn is_zero(&self) -> bool {
        self.0.iter().all(|byte| *byte == 0)
    }
}

impl fmt::Display for AttachmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for AttachmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AttachmentId").field(&self.to_hex()).finish()
    }
}

impl FromStr for AttachmentId {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        let bytes = hex::decode(s)
            .map_err(|_| CoreError::InvalidId(format!("invalid hex attachment id: {s}")))?;
        Self::from_slice(&bytes)
    }
}

/// One attachment announced by a message.
///
/// `name` is only a suggestion for the receiving user interface. It is never
/// used as a path and is never trusted for a filesystem write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub hash: AttachmentId,
    pub size: u64,
    pub mime: String,
    pub name: String,
}

impl Attachment {
    /// Check the metadata a message is about to announce.
    ///
    /// This cannot check that the bytes match `hash` or `size`; that is done
    /// when a blob is stored or fetched.
    pub fn validate(&self) -> Result<()> {
        if self.hash.is_zero() {
            return Err(CoreError::InvalidInput(
                "attachment has an empty content hash".to_string(),
            ));
        }
        if self.size > MAX_ATTACHMENT_BYTES {
            return Err(CoreError::InvalidInput(format!(
                "attachment is {} bytes, over the {MAX_ATTACHMENT_BYTES} byte limit",
                self.size
            )));
        }
        if self.name.trim().is_empty() {
            return Err(CoreError::InvalidInput(
                "attachment has no display name".to_string(),
            ));
        }
        if self.name.len() > MAX_ATTACHMENT_NAME_BYTES {
            return Err(CoreError::InvalidInput(format!(
                "attachment name is {} bytes, over the {MAX_ATTACHMENT_NAME_BYTES} byte limit",
                self.name.len()
            )));
        }
        if self.name.chars().any(char::is_control) {
            return Err(CoreError::InvalidInput(
                "attachment name contains a control character".to_string(),
            ));
        }
        if self.mime.len() > MAX_ATTACHMENT_MIME_BYTES {
            return Err(CoreError::InvalidInput(format!(
                "attachment media type is {} bytes, over the {MAX_ATTACHMENT_MIME_BYTES} byte limit",
                self.mime.len()
            )));
        }
        if !is_plausible_mime(&self.mime) {
            return Err(CoreError::InvalidInput(format!(
                "attachment media type is not a type/subtype pair: {:?}",
                self.mime
            )));
        }
        Ok(())
    }
}

/// Validate a message's attachment list.
///
/// Duplicate hashes are rejected because they would make a reader download
/// the same bytes twice and would give the same blob two names in one message.
pub fn validate_attachments(attachments: &[Attachment]) -> Result<()> {
    if attachments.len() > MAX_ATTACHMENTS_PER_MESSAGE {
        return Err(CoreError::InvalidInput(format!(
            "message has {} attachments, over the {MAX_ATTACHMENTS_PER_MESSAGE} limit",
            attachments.len()
        )));
    }
    let mut seen = std::collections::HashSet::with_capacity(attachments.len());
    for attachment in attachments {
        attachment.validate()?;
        if !seen.insert(attachment.hash) {
            return Err(CoreError::InvalidInput(format!(
                "message announces attachment {} more than once",
                attachment.hash.short()
            )));
        }
    }
    Ok(())
}

/// Hash bytes exactly as they would be stored.
pub fn hash_bytes(bytes: &[u8]) -> AttachmentId {
    AttachmentId(*blake3::hash(bytes).as_bytes())
}

/// Build the metadata a message announces for a file whose bytes are read.
///
/// The display name comes from the path and is checked here, because a name
/// that is empty, too long or full of control characters cannot be sent. The
/// MIME type is only a hint for the receiver's open action, so a conservative
/// extension table is enough and an unknown type is sent as opaque bytes.
pub fn attachment_for_file(path: &std::path::Path, bytes: &[u8]) -> Result<Attachment> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| CoreError::InvalidInput("the file name is not valid UTF-8".to_string()))?;
    if name.is_empty() {
        return Err(CoreError::InvalidInput("the file has no name".to_string()));
    }
    if name.len() > MAX_ATTACHMENT_NAME_BYTES {
        return Err(CoreError::InvalidInput(format!(
            "the file name is longer than {MAX_ATTACHMENT_NAME_BYTES} bytes"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(CoreError::InvalidInput(
            "the file name contains control characters".to_string(),
        ));
    }
    let mime = mime_for_path(path);
    Ok(Attachment {
        hash: hash_bytes(bytes),
        size: bytes.len() as u64,
        mime,
        name: name.to_string(),
    })
}

/// A conservative MIME guess from the extension, shared by the frontends.
pub fn mime_for_path(path: &std::path::Path) -> String {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mime = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "md" => "text/markdown",
        "json" => "application/json",
        "csv" => "text/csv",
        "zip" => "application/zip",
        "ogg" => "audio/ogg",
        "opus" => "audio/opus",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    };
    mime.to_string()
}

/// Hash a file without holding it in memory.
pub fn hash_reader(reader: &mut impl std::io::Read) -> Result<AttachmentId> {
    let mut hasher = blake3::Hasher::new();
    std::io::copy(reader, &mut hasher)?;
    Ok(AttachmentId(*hasher.finalize().as_bytes()))
}

/// Human readable size for a file chip, e.g. `940 B`, `12.3 KiB`, `4.1 MiB`.
///
/// Every frontend shows attachment sizes in its own style, but they all quote
/// the same number, so the formatting lives here rather than in three places
/// that can drift apart.
pub fn human_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let bytes = bytes as f64;
    if bytes < KIB {
        return format!("{} B", bytes as u64);
    }
    let kib = bytes / KIB;
    if kib < KIB {
        return format!("{kib:.1} KiB");
    }
    let mib = kib / KIB;
    if mib < KIB {
        return format!("{mib:.1} MiB");
    }
    format!("{:.1} GiB", mib / KIB)
}

/// A conservative media type check: one `/`, no whitespace or controls, and
/// non-empty type and subtype. The receiver still treats the value as a hint.
fn is_plausible_mime(mime: &str) -> bool {
    let Some((kind, subtype)) = mime.split_once('/') else {
        return false;
    };
    !kind.is_empty()
        && !subtype.is_empty()
        && !subtype.contains('/')
        && !mime
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment(name: &str) -> Attachment {
        Attachment {
            hash: hash_bytes(b"a picture"),
            size: 9,
            mime: "image/png".to_string(),
            name: name.to_string(),
        }
    }

    #[test]
    fn hash_is_blake3_of_the_exact_bytes() {
        // Known BLAKE3 vector for the empty input.
        assert_eq!(
            hash_bytes(b"").to_hex(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        let mut bytes = &b"a picture"[..];
        assert_eq!(hash_reader(&mut bytes).unwrap(), hash_bytes(b"a picture"));
    }

    #[test]
    fn sizes_read_as_units_a_person_uses() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(700 * 1024), "700.0 KiB");
        assert_eq!(human_size(25 * 1024 * 1024), "25.0 MiB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn ids_roundtrip_through_hex() {
        let id = hash_bytes(b"roundtrip");
        assert_eq!(id.to_hex().parse::<AttachmentId>().unwrap(), id);
        assert_eq!(AttachmentId::from_slice(id.as_bytes()).unwrap(), id);
        assert!(AttachmentId::from_slice(&[0u8; 31]).is_err());
        assert!("not hex".parse::<AttachmentId>().is_err());
    }

    #[test]
    fn metadata_validation_rejects_the_obvious_abuse() {
        assert!(attachment("holiday.png").validate().is_ok());
        assert!(attachment("").validate().is_err());
        assert!(attachment("   ").validate().is_err());
        assert!(attachment("bad\nname").validate().is_err());
        assert!(
            attachment(&"x".repeat(MAX_ATTACHMENT_NAME_BYTES + 1))
                .validate()
                .is_err()
        );

        let mut zero = attachment("zero");
        zero.hash = AttachmentId([0u8; 32]);
        assert!(zero.validate().is_err());

        let mut huge = attachment("huge");
        huge.size = MAX_ATTACHMENT_BYTES + 1;
        assert!(huge.validate().is_err());

        let mut no_slash = attachment("mime");
        no_slash.mime = "png".to_string();
        assert!(no_slash.validate().is_err());

        let mut spaced = attachment("mime");
        spaced.mime = "image/ png".to_string();
        assert!(spaced.validate().is_err());
    }

    #[test]
    fn duplicate_hashes_and_excess_count_are_rejected() {
        let one = attachment("one.png");
        assert!(validate_attachments(std::slice::from_ref(&one)).is_ok());
        assert!(validate_attachments(&[one.clone(), one.clone()]).is_err());

        let many: Vec<_> = (0..=MAX_ATTACHMENTS_PER_MESSAGE)
            .map(|index| Attachment {
                hash: hash_bytes(&index.to_be_bytes()),
                size: 1,
                mime: "image/png".to_string(),
                name: format!("{index}.png"),
            })
            .collect();
        assert!(validate_attachments(&many).is_err());
    }
}
