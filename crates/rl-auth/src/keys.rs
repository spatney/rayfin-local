//! The ES256 (P-256) signing key: created on first run, kept in `<keys dir>/es256.pk8`.

use std::{
    fs::OpenOptions,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

use ring::{
    rand::SystemRandom,
    signature::{self, ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair},
};
use serde_json::{Value, json};

use crate::{
    AuthError,
    util::{b64url, sha256},
};

/// The key file in the keys folder: the PKCS#8 (DER) private key.
pub(crate) const KEY_FILE: &str = "es256.pk8";

pub(crate) struct SigningKey {
    pair: EcdsaKeyPair,
    /// The uncompressed public point: `0x04 || x || y`.
    public: Vec<u8>,
    kid: String,
    rng: SystemRandom,
}

impl SigningKey {
    /// Loads the key from `dir`, creating the folder and a new key on first use.
    pub(crate) fn load_or_create(dir: &Path) -> Result<Self, AuthError> {
        let path = dir.join(KEY_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => Self::from_pkcs8(&bytes).map_err(|reason| AuthError::Key { path, reason }),
            Err(error) if error.kind() == ErrorKind::NotFound => Self::create(dir, path),
            Err(source) => Err(AuthError::KeyIo { path, source }),
        }
    }

    fn create(dir: &Path, path: PathBuf) -> Result<Self, AuthError> {
        let io_error = |source| AuthError::KeyIo {
            path: path.clone(),
            source,
        };
        create_private_dir(dir).map_err(io_error)?;
        let document =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
                .map_err(|_| AuthError::Crypto("could not generate the ES256 signing key"))?;
        let bytes = if write_new_private_file(&path, document.as_ref()).map_err(io_error)? {
            document.as_ref().to_vec()
        } else {
            // Another rayfin-local process created the key first; use that one.
            std::fs::read(&path).map_err(io_error)?
        };
        Self::from_pkcs8(&bytes).map_err(|reason| AuthError::Key { path, reason })
    }

    fn from_pkcs8(bytes: &[u8]) -> Result<Self, String> {
        let rng = SystemRandom::new();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, bytes, &rng)
            .map_err(|error| error.to_string())?;
        let public = pair.public_key().as_ref().to_vec();
        let kid = b64url(&sha256(&public)[..16]);
        Ok(Self {
            pair,
            public,
            kid,
            rng,
        })
    }

    pub(crate) fn kid(&self) -> &str {
        &self.kid
    }

    /// A JWS ES256 signature: the fixed-size `r || s`.
    pub(crate) fn sign(&self, message: &[u8]) -> Result<Vec<u8>, AuthError> {
        self.pair
            .sign(&self.rng, message)
            .map(|signature| signature.as_ref().to_vec())
            .map_err(|_| AuthError::Crypto("signing the access token failed"))
    }

    pub(crate) fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, &self.public)
            .verify(message, signature)
            .is_ok()
    }

    /// The public key as a JWK.
    pub(crate) fn jwk(&self) -> Value {
        let (x, y) = self.public[1..].split_at(32);
        json!({
            "kty": "EC",
            "use": "sig",
            "kid": self.kid,
            "alg": "ES256",
            "crv": "P-256",
            "x": b64url(x),
            "y": b64url(y),
        })
    }
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Writes `bytes` to `path` only if `path` does not exist yet (`Ok(false)` then), never leaving a
/// partly written key behind. The file is readable by the owner only on Unix; on Windows it
/// inherits the folder's permissions.
fn write_new_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<bool> {
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    let placed = match std::fs::hard_link(&temp, path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(false),
        // File systems without hard links: fall back to a plain rename.
        Err(_) => std::fs::rename(&temp, path).map(|()| true),
    };
    let _ = std::fs::remove_file(&temp);
    placed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_persists_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let keys = dir.path().join("nested").join("keys");
        let first = SigningKey::load_or_create(&keys).unwrap();
        let signature = first.sign(b"message").unwrap();
        assert_eq!(signature.len(), 64);

        let second = SigningKey::load_or_create(&keys).unwrap();
        assert_eq!(first.kid(), second.kid());
        assert!(second.verify(b"message", &signature));
        assert!(!second.verify(b"other message", &signature));
        let leftovers: Vec<_> = std::fs::read_dir(&keys).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "only the key file remains");
    }

    #[test]
    fn a_corrupt_key_is_reported_with_its_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(KEY_FILE), b"not a key").unwrap();
        let error = SigningKey::load_or_create(dir.path()).err().unwrap();
        assert!(matches!(error, AuthError::Key { .. }));
        assert!(error.to_string().contains(KEY_FILE));
    }
}
