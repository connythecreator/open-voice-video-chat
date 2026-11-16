//! ChaCha20-Poly1305 framing for one direction of a pairwise stream.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use zeroize::Zeroize;

use crate::error::{CryptoError, Result};
use crate::keys::PairwiseKeys;
use crate::replay::ReplayWindow;
use crate::{HEADER_BYTES, MAX_PLAINTEXT_BYTES, TAG_BYTES};

/// Encrypts and decrypts one peer's media in both directions.
///
/// Each direction has its own AEAD key and counter, so an outgoing counter
/// can never collide with an incoming one. The wire layout is the 12-byte
/// header (`epoch` u32 BE, `counter` u64 BE), the ciphertext, then the
/// 16-byte Poly1305 tag.
pub struct MediaCipher {
    send: ChaCha20Poly1305,
    recv: ChaCha20Poly1305,
    epoch: u32,
    /// Next outgoing counter. Starts at one; zero is never used.
    send_counter: AtomicU64,
    /// Counters accepted in the incoming direction.
    recv_window: Mutex<ReplayWindow>,
}

impl MediaCipher {
    /// Build both directions from a derived pair of keys at `epoch`.
    pub fn new(keys: PairwiseKeys, epoch: u32) -> Self {
        let (mut send_key, mut recv_key) = keys.into_keys();
        let mut send_key_array = Key::from(send_key);
        let mut recv_key_array = Key::from(recv_key);
        let send = ChaCha20Poly1305::new(&send_key_array);
        let recv = ChaCha20Poly1305::new(&recv_key_array);
        send_key.as_mut_slice().zeroize();
        recv_key.as_mut_slice().zeroize();
        send_key_array.as_mut_slice().zeroize();
        recv_key_array.as_mut_slice().zeroize();
        Self {
            send,
            recv,
            epoch,
            send_counter: AtomicU64::new(1),
            recv_window: Mutex::new(ReplayWindow::new()),
        }
    }

    /// Encrypt one frame and return the complete wire payload.
    ///
    /// The result is `HEADER_BYTES + plaintext.len() + TAG_BYTES` long.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        if plaintext.len() > MAX_PLAINTEXT_BYTES {
            return Err(CryptoError::PlaintextTooLarge);
        }
        let counter = self.next_counter()?;

        let mut header = [0u8; HEADER_BYTES];
        header[..4].copy_from_slice(&self.epoch.to_be_bytes());
        header[4..].copy_from_slice(&counter.to_be_bytes());
        let nonce = Nonce::from(header);

        let ciphertext = self
            .send
            .encrypt(&nonce, plaintext)
            .map_err(|_| CryptoError::Decrypt)?;

        let mut payload = Vec::with_capacity(HEADER_BYTES + plaintext.len() + TAG_BYTES);
        payload.extend_from_slice(&header);
        payload.extend_from_slice(&ciphertext);
        Ok(payload)
    }

    /// Authenticate and decrypt one wire payload.
    ///
    /// Rejects a truncated payload, the wrong epoch, counter zero, an
    /// oversized plaintext, a replay, and any AEAD failure.
    pub fn decrypt(&self, payload: &[u8]) -> Result<Vec<u8>> {
        if payload.len() < HEADER_BYTES + TAG_BYTES {
            return Err(CryptoError::Decrypt);
        }
        let header_slice = payload.get(..HEADER_BYTES).ok_or(CryptoError::Decrypt)?;
        let mut header = [0u8; HEADER_BYTES];
        header.copy_from_slice(header_slice);

        let epoch = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let counter = u64::from_be_bytes([
            header[4], header[5], header[6], header[7], header[8], header[9], header[10],
            header[11],
        ]);
        if epoch != self.epoch {
            return Err(CryptoError::WrongEpoch);
        }
        if counter == 0 {
            return Err(CryptoError::Decrypt);
        }
        let plaintext_len = payload.len() - HEADER_BYTES - TAG_BYTES;
        if plaintext_len > MAX_PLAINTEXT_BYTES {
            return Err(CryptoError::PlaintextTooLarge);
        }
        let ciphertext = payload.get(HEADER_BYTES..).ok_or(CryptoError::Decrypt)?;

        // Hold the window across authentication so two threads cannot both
        // accept the same counter. Only commit after the tag verifies, so a
        // forged frame does not burn a counter.
        let mut window = self.lock_window();
        if !window.is_acceptable(counter) {
            return Err(CryptoError::Replay);
        }
        let nonce = Nonce::from(header);
        let plaintext = self
            .recv
            .decrypt(&nonce, ciphertext)
            .map_err(|_| CryptoError::Decrypt)?;
        if !window.accept(counter) {
            return Err(CryptoError::Replay);
        }
        Ok(plaintext)
    }

    /// Claim the next outgoing counter, refusing to wrap.
    fn next_counter(&self) -> Result<u64> {
        let mut current = self.send_counter.load(Ordering::Relaxed);
        loop {
            if current == u64::MAX {
                return Err(CryptoError::CounterExhausted);
            }
            match self.send_counter.compare_exchange_weak(
                current,
                current + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(current),
                Err(actual) => current = actual,
            }
        }
    }

    fn lock_window(&self) -> MutexGuard<'_, ReplayWindow> {
        match self.recv_window.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovvc_core::AuthorId;
    use ovvc_media::CallId;
    use rand::rngs::OsRng;
    use x25519_dalek::{PublicKey, StaticSecret};

    #[test]
    fn send_counter_does_not_wrap() {
        let local_author = AuthorId([1; 32]);
        let peer_author = AuthorId([2; 32]);
        let local_secret = StaticSecret::random_from_rng(OsRng);
        let peer_secret = StaticSecret::random_from_rng(OsRng);
        let keys = crate::keys::derive_pairwise(
            local_author,
            peer_author,
            &local_secret,
            PublicKey::from(&peer_secret).to_bytes(),
            CallId([3; 16]),
            1,
        )
        .unwrap();

        let cipher = MediaCipher::new(keys, 1);
        cipher.send_counter.store(u64::MAX, Ordering::Relaxed);
        assert!(matches!(
            cipher.encrypt(b"last"),
            Err(CryptoError::CounterExhausted)
        ));
        assert!(matches!(
            cipher.encrypt(b"again"),
            Err(CryptoError::CounterExhausted)
        ));
    }
}
