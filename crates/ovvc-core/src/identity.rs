use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;

use crate::error::{CoreError, Result};
use crate::model::{AuthorId, EventBody, SignedEvent};

#[derive(Clone)]
pub struct Identity {
    signing_key: SigningKey,
}

impl Identity {
    pub fn generate() -> Self {
        Self {
            signing_key: SigningKey::generate(&mut OsRng),
        }
    }

    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            signing_key: SigningKey::from_bytes(&seed),
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 32 {
            return Err(CoreError::InvalidInput(format!(
                "identity seed must be 32 bytes, got {}",
                bytes.len()
            )));
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(bytes);
        Ok(Self::from_seed(seed))
    }

    pub fn load(path: &Path) -> Result<Self> {
        // `symlink_metadata` does not follow the path: a seed swapped in via a
        // symlink (or a FIFO/device that would block or read forever) must be
        // refused rather than silently trusted.
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() {
            return Err(CoreError::InvalidInput(
                "identity key must be a regular file".to_string(),
            ));
        }
        if metadata.len() != 32 {
            return Err(CoreError::InvalidInput(format!(
                "identity seed must be 32 bytes, got {}",
                metadata.len()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                let mut permissions = metadata.permissions();
                permissions.set_mode(0o600);
                fs::set_permissions(path, permissions)?;
            }
        }
        let bytes = fs::read(path)?;
        Self::from_bytes(&bytes)
    }

    pub fn create(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let identity = Self::generate();
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        let mut file = options.open(path)?;
        file.write_all(&identity.signing_key.to_bytes())?;
        file.sync_all()?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(path)?.permissions();
            perms.set_mode(0o600);
            fs::set_permissions(path, perms)?;
        }

        Ok(identity)
    }

    pub fn load_or_create(path: &Path) -> Result<(Self, bool)> {
        if path.exists() {
            Ok((Self::load(path)?, false))
        } else {
            Ok((Self::create(path)?, true))
        }
    }

    pub fn author_id(&self) -> AuthorId {
        AuthorId(self.signing_key.verifying_key().to_bytes())
    }

    pub fn seed(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }

    pub fn public_key_hex(&self) -> String {
        hex::encode(self.signing_key.verifying_key().to_bytes())
    }

    pub fn sign_body(&self, body: EventBody) -> Result<SignedEvent> {
        SignedEvent::sign(body, &self.signing_key)
    }

    pub fn sign_bytes(&self, bytes: &[u8]) -> Vec<u8> {
        use ed25519_dalek::Signer;
        self.signing_key.sign(bytes).to_bytes().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_rejects_wrong_size_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let short = dir.path().join("short.key");
        fs::write(&short, [0u8; 31]).unwrap();
        assert!(Identity::load(&short).is_err());

        #[cfg(unix)]
        {
            let real = dir.path().join("real.key");
            fs::write(&real, [7u8; 32]).unwrap();
            let link = dir.path().join("link.key");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert!(Identity::load(&link).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn load_repairs_world_readable_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        fs::write(&path, [9u8; 32]).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        Identity::load(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
