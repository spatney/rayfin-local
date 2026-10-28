//! Reading (never verifying) the claims of access tokens: their expiry, and who they belong to.

use std::time::{Duration, SystemTime};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD_INDIFFERENT as BASE64URL};
use serde_json::{Map, Value};

/// The claims in a JWT's payload, or `None` when `token` isn't a JWT (opaque or encrypted
/// tokens). The signature is not checked: use this only to describe a token you already trust.
pub fn jwt_claims(token: &str) -> Option<Map<String, Value>> {
    let mut parts = token.trim().split('.');
    let (_header, payload, _signature) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    match serde_json::from_slice(&BASE64URL.decode(payload).ok()?).ok()? {
        Value::Object(claims) => Some(claims),
        _ => None,
    }
}

/// When a JWT expires (its `exp` claim).
pub(crate) fn expiry(token: &str) -> Option<SystemTime> {
    let exp = jwt_claims(token)?.get("exp")?.as_f64()?;
    // `exp` is seconds since the epoch; anything outside this range is not a real expiry.
    (exp > 0.0 && exp < 1e11).then(|| SystemTime::UNIX_EPOCH + Duration::from_secs_f64(exp))
}

/// A string claim, if present and not empty.
pub(crate) fn claim<'a>(claims: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    claims.get(name)?.as_str().filter(|value| !value.is_empty())
}

#[cfg(test)]
pub(crate) fn fake_jwt(claims: &Value) -> String {
    let encode = |value: &Value| BASE64URL.encode(value.to_string().as_bytes());
    format!(
        "{}.{}.signature",
        encode(&serde_json::json!({ "alg": "RS256", "typ": "JWT" })),
        encode(claims)
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reads_claims_and_expiry_of_a_jwt() {
        let token = fake_jwt(&json!({ "aud": "https://ai.azure.com", "exp": 1_900_000_000 }));
        assert_eq!(
            claim(&jwt_claims(&token).unwrap(), "aud"),
            Some("https://ai.azure.com")
        );
        assert_eq!(
            expiry(&token),
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_900_000_000))
        );
    }

    #[test]
    fn opaque_and_encrypted_tokens_have_no_claims() {
        assert_eq!(jwt_claims("not-a-jwt"), None);
        assert_eq!(jwt_claims("a.b.c.d.e"), None);
        assert_eq!(expiry(&fake_jwt(&json!({ "aud": "x" }))), None);
    }
}
