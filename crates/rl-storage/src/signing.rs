//! Signed tokens: the capability in signed URLs and list continuations.
//!
//! A token is `{id}.{expires}.{mac}`, where `mac` is the base64url HMAC-SHA256 of the purpose
//! (the HTTP method it may be used with), the id and the expiry (Unix seconds). A token signed
//! for one purpose never verifies for another.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::hmac;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Purpose {
    /// `PUT` of an upload's bytes; the id is the operation id.
    Upload,
    /// `GET` of an object's bytes; the id is the object version's blob id.
    Download,
    /// A list continuation; the id is the base64url of the last path returned.
    Continuation,
}

impl Purpose {
    fn tag(self) -> &'static str {
        match self {
            Self::Upload => "PUT",
            Self::Download => "GET",
            Self::Continuation => "LIST",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokenError {
    /// Malformed, tampered with, or signed for another purpose or key.
    Invalid,
    Expired,
}

pub(crate) struct Signer {
    key: hmac::Key,
}

impl Signer {
    pub(crate) fn new(secret: &[u8; 32]) -> Self {
        Self {
            key: hmac::Key::new(hmac::HMAC_SHA256, secret),
        }
    }

    /// `id` must not contain `.`.
    pub(crate) fn sign(&self, purpose: Purpose, id: &str, expires: i64) -> String {
        let mac = hmac::sign(&self.key, message(purpose, id, expires).as_bytes());
        format!("{id}.{expires}.{}", URL_SAFE_NO_PAD.encode(mac.as_ref()))
    }

    /// The id of `token` when it was signed by this key for `purpose` and has not expired.
    pub(crate) fn verify(
        &self,
        purpose: Purpose,
        token: &str,
        now: i64,
    ) -> Result<String, TokenError> {
        let mut parts = token.split('.');
        let (Some(id), Some(expires), Some(mac), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(TokenError::Invalid);
        };
        let expires: i64 = expires.parse().map_err(|_| TokenError::Invalid)?;
        let mac = URL_SAFE_NO_PAD
            .decode(mac)
            .map_err(|_| TokenError::Invalid)?;
        // `hmac::verify` compares in constant time.
        hmac::verify(&self.key, message(purpose, id, expires).as_bytes(), &mac)
            .map_err(|_| TokenError::Invalid)?;
        if now > expires {
            return Err(TokenError::Expired);
        }
        Ok(id.to_owned())
    }
}

fn message(purpose: Purpose, id: &str, expires: i64) -> String {
    format!("rayfin-local storage\n{}\n{id}\n{expires}", purpose.tag())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_verify_only_for_their_purpose_key_and_lifetime() {
        let signer = Signer::new(&[7; 32]);
        let token = signer.sign(Purpose::Upload, "op1", 100);
        assert_eq!(
            signer.verify(Purpose::Upload, &token, 100),
            Ok("op1".into())
        );
        assert_eq!(
            signer.verify(Purpose::Upload, &token, 101),
            Err(TokenError::Expired)
        );
        assert_eq!(
            signer.verify(Purpose::Download, &token, 0),
            Err(TokenError::Invalid)
        );
        let other = Signer::new(&[8; 32]);
        assert_eq!(
            other.verify(Purpose::Upload, &token, 0),
            Err(TokenError::Invalid)
        );
        let longer = token.replacen(".100.", ".999.", 1);
        assert_eq!(
            signer.verify(Purpose::Upload, &longer, 0),
            Err(TokenError::Invalid)
        );
        let renamed = token.replacen("op1", "op2", 1);
        assert_eq!(
            signer.verify(Purpose::Upload, &renamed, 0),
            Err(TokenError::Invalid)
        );
        assert_eq!(
            signer.verify(Purpose::Upload, "op1.100", 0),
            Err(TokenError::Invalid)
        );
        assert_eq!(
            signer.verify(Purpose::Upload, &format!("{token}.x"), 0),
            Err(TokenError::Invalid)
        );
    }
}
