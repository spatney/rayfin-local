//! Access tokens: ES256 JWTs shaped like the real service's, and their verification.

use std::io;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::{
    AuthError, AuthSettings,
    keys::SigningKey,
    users::User,
    util::{b64url, b64url_decode},
};

pub(crate) const ALG: &str = "ES256";
/// The `role` claim of every user token (the real service writes its `UserRole` enum name).
pub(crate) const AUTHENTICATED_ROLE: &str = "Authenticated";
/// Clock skew tolerated on `exp` and `nbf`: the real service's default (5 minutes).
pub(crate) const LEEWAY_SECONDS: i64 = 300;

/// Why a token was refused. Callers answer every case with one generic message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokenError {
    Malformed,
    Algorithm,
    UnknownKey,
    Signature,
    Expired,
    NotYetValid,
    Issuer,
    Audience,
    MissingClaim(&'static str),
    /// The token's session was signed out.
    Revoked,
}

/// The verified claims of an access token.
#[derive(Clone, PartialEq)]
pub(crate) struct AccessClaims {
    pub(crate) sub: String,
    pub(crate) sid: String,
    pub(crate) email: Option<String>,
    pub(crate) claims: Map<String, Value>,
}

impl std::fmt::Debug for AccessClaims {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Identity claims stay out of logs.
        f.debug_struct("AccessClaims")
            .field("sub", &self.sub)
            .field("sid", &self.sid)
            .field("claims", &self.claims.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// Mints and verifies the app's access tokens.
pub(crate) struct Jwt {
    key: Arc<SigningKey>,
    issuer: String,
    audience: String,
    access_ttl_seconds: i64,
    /// Static custom claims with `${NAME}` already resolved.
    custom_claims: Vec<(String, String)>,
    scope: Option<String>,
}

impl Jwt {
    pub(crate) fn new(key: SigningKey, settings: &AuthSettings) -> Self {
        Self::with_key(Arc::new(key), settings)
    }

    /// The same signing key with other settings.
    pub(crate) fn with_settings(&self, settings: &AuthSettings) -> Self {
        Self::with_key(Arc::clone(&self.key), settings)
    }

    fn with_key(key: Arc<SigningKey>, settings: &AuthSettings) -> Self {
        let custom_claims = settings
            .custom_claims
            .iter()
            .map(|(name, value)| {
                let resolved = resolve_env(value, |var| std::env::var(var).ok());
                (name.clone(), resolved)
            })
            .collect();
        Self {
            key,
            issuer: settings.issuer.clone(),
            audience: settings.app_id.clone(),
            access_ttl_seconds: i64::from(settings.access_token_minutes) * 60,
            custom_claims,
            scope: settings.scope(),
        }
    }

    pub(crate) fn access_ttl_seconds(&self) -> i64 {
        self.access_ttl_seconds
    }

    pub(crate) fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    pub(crate) fn jwks(&self) -> Value {
        json!({ "keys": [self.key.jwk()] })
    }

    /// An access token for `user` in session `session_id`, issued at `now` (Unix seconds). The
    /// claims and their order follow the real service's token generator.
    pub(crate) fn mint(
        &self,
        user: &User,
        session_id: &str,
        now: i64,
    ) -> Result<String, AuthError> {
        let mut claims = Map::new();
        claims.insert("sub".into(), user.id.clone().into());
        if let Some(email) = &user.email {
            claims.insert("email".into(), email.clone().into());
        }
        claims.insert("name".into(), user.name().into());
        claims.insert("role".into(), AUTHENTICATED_ROLE.into());
        claims.insert("jti".into(), uuid::Uuid::new_v4().to_string().into());
        claims.insert("iat".into(), now.into());
        claims.insert("exp".into(), (now + self.access_ttl_seconds).into());
        claims.insert("nbf".into(), now.into());
        claims.insert("iss".into(), self.issuer.clone().into());
        claims.insert("aud".into(), self.audience.clone().into());
        claims.insert("sid".into(), session_id.into());
        for (name, value) in &self.custom_claims {
            claims.insert(name.clone(), value.clone().into());
        }
        if let Some(scope) = &self.scope {
            claims.insert("scope".into(), scope.clone().into());
        }
        self.encode(&claims)
    }

    /// Signs `claims` as a compact JWS with this service's key.
    pub(crate) fn encode(&self, claims: &Map<String, Value>) -> Result<String, AuthError> {
        let header = json!({ "alg": ALG, "typ": "JWT", "kid": self.key.kid() });
        let signing_input = format!(
            "{}.{}",
            b64url(&ascii_json(&header)?),
            b64url(&ascii_json(claims)?)
        );
        let signature = self.key.sign(signing_input.as_bytes())?;
        Ok(format!("{signing_input}.{}", b64url(&signature)))
    }

    /// Verifies the signature, `kid`, `alg`, `exp`/`nbf` (with leeway), `iss`, `aud`, `sub` and
    /// `sid` of `token` at `now` (Unix seconds). Session revocation is the caller's check.
    ///
    /// A token minted for longer than the current lifetime (`expiryInMinutes` lowered since) is
    /// expired: signed-out sessions are only remembered for the current lifetime, so such a token
    /// could outlive its session's sign-out.
    pub(crate) fn verify(&self, token: &str, now: i64) -> Result<AccessClaims, TokenError> {
        let mut parts = token.split('.');
        let (Some(header_part), Some(payload_part), Some(signature_part), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(TokenError::Malformed);
        };
        let header = decode_object(header_part)?;
        if header.get("alg").and_then(Value::as_str) != Some(ALG) {
            return Err(TokenError::Algorithm);
        }
        if header.get("kid").and_then(Value::as_str) != Some(self.key.kid()) {
            return Err(TokenError::UnknownKey);
        }
        let signature = b64url_decode(signature_part).ok_or(TokenError::Malformed)?;
        let signing_input = &token[..header_part.len() + 1 + payload_part.len()];
        if !self.key.verify(signing_input.as_bytes(), &signature) {
            return Err(TokenError::Signature);
        }
        let claims = decode_object(payload_part)?;
        let exp = number_claim(&claims, "exp").ok_or(TokenError::MissingClaim("exp"))?;
        if now > exp.saturating_add(LEEWAY_SECONDS) {
            return Err(TokenError::Expired);
        }
        let iat = number_claim(&claims, "iat").ok_or(TokenError::MissingClaim("iat"))?;
        if exp.saturating_sub(iat) > self.access_ttl_seconds {
            return Err(TokenError::Expired);
        }
        if number_claim(&claims, "nbf").is_some_and(|nbf| nbf > now.saturating_add(LEEWAY_SECONDS))
        {
            return Err(TokenError::NotYetValid);
        }
        if claims.get("iss").and_then(Value::as_str) != Some(self.issuer.as_str()) {
            return Err(TokenError::Issuer);
        }
        let audience_ok = match claims.get("aud") {
            Some(Value::String(aud)) => *aud == self.audience,
            Some(Value::Array(auds)) => auds.iter().any(|aud| aud.as_str() == Some(&self.audience)),
            _ => false,
        };
        if !audience_ok {
            return Err(TokenError::Audience);
        }
        let sub = string_claim(&claims, "sub").ok_or(TokenError::MissingClaim("sub"))?;
        let sid = string_claim(&claims, "sid").ok_or(TokenError::MissingClaim("sid"))?;
        let email = string_claim(&claims, "email");
        Ok(AccessClaims {
            sub,
            sid,
            email,
            claims,
        })
    }
}

pub(crate) fn decode_object(part: &str) -> Result<Map<String, Value>, TokenError> {
    let bytes = b64url_decode(part).ok_or(TokenError::Malformed)?;
    match serde_json::from_slice(&bytes) {
        Ok(Value::Object(object)) => Ok(object),
        _ => Err(TokenError::Malformed),
    }
}

pub(crate) fn number_claim(claims: &Map<String, Value>, name: &str) -> Option<i64> {
    let value = claims.get(name)?;
    value.as_i64().or_else(|| value.as_f64().map(|n| n as i64))
}

fn string_claim(claims: &Map<String, Value>, name: &str) -> Option<String> {
    claims
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// JSON with every non-ASCII character escaped (`\uXXXX`), as the real service's serializer
/// writes it: the SDK decodes token payloads with `atob`, which only round-trips ASCII.
pub(crate) fn ascii_json<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, AuthError> {
    let mut out = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, AsciiFormatter);
    value
        .serialize(&mut serializer)
        .map_err(|_| AuthError::Crypto("could not serialize the token"))?;
    Ok(out)
}

struct AsciiFormatter;

impl serde_json::ser::Formatter for AsciiFormatter {
    fn write_string_fragment<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        let mut start = 0;
        for (index, ch) in fragment.char_indices() {
            if ch.is_ascii() {
                continue;
            }
            writer.write_all(&fragment.as_bytes()[start..index])?;
            let mut units = [0u16; 2];
            for unit in ch.encode_utf16(&mut units) {
                write!(writer, "\\u{unit:04x}")?;
            }
            start = index + ch.len_utf8();
        }
        writer.write_all(&fragment.as_bytes()[start..])
    }
}

/// Replaces `${NAME}` with `lookup(NAME)`, keeping the reference when it has no value (the real
/// service's custom-claim behavior).
fn resolve_env(value: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) if end > 0 => {
                let reference = &rest[start..start + 2 + end + 1];
                match lookup(&after[..end]) {
                    Some(resolved) => out.push_str(&resolved),
                    None => out.push_str(reference),
                }
                rest = &after[end + 1..];
            }
            Some(_) => {
                out.push_str("${}");
                rest = &after[1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::unix_now_ms;

    fn jwt() -> (tempfile::TempDir, Jwt) {
        let dir = tempfile::tempdir().unwrap();
        let key = SigningKey::load_or_create(dir.path()).unwrap();
        let mut settings = AuthSettings::new("6c6f6361-6c00-4000-8000-000000000001");
        settings.custom_claims = vec![("app_version".into(), "1.0.0".into())];
        settings.scopes = vec!["read:data".into(), "write:data".into()];
        (dir, Jwt::new(key, &settings))
    }

    fn user(username: &str, email: Option<&str>) -> User {
        User {
            id: "0d7f4c1e-9a43-4b8f-b2a6-3f0c5e8d1a22".into(),
            username: username.into(),
            email: email.map(str::to_owned),
            display_name: None,
            has_password: true,
            entra_oid: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn now() -> i64 {
        unix_now_ms() / 1000
    }

    fn payload(token: &str) -> Map<String, Value> {
        decode_object(token.split('.').nth(1).unwrap()).unwrap()
    }

    #[test]
    fn minted_tokens_carry_the_real_service_claims_and_verify() {
        let (_dir, jwt) = jwt();
        let token = jwt
            .mint(
                &user("alice@example.test", Some("alice@example.test")),
                "sid-1",
                now(),
            )
            .unwrap();
        let header = decode_object(token.split('.').next().unwrap()).unwrap();
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["typ"], "JWT");
        let claims = payload(&token);
        let names: Vec<_> = claims.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            [
                "sub",
                "email",
                "name",
                "role",
                "jti",
                "iat",
                "exp",
                "nbf",
                "iss",
                "aud",
                "sid",
                "app_version",
                "scope"
            ]
        );
        assert_eq!(claims["role"], "Authenticated");
        assert_eq!(claims["scope"], "read:data write:data");
        assert_eq!(
            claims["iss"],
            "https://6c6f6361-6c00-4000-8000-000000000001.rayfin.io"
        );
        assert_eq!(
            claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(),
            3600
        );

        let verified = jwt.verify(&token, now()).unwrap();
        assert_eq!(verified.sub, "0d7f4c1e-9a43-4b8f-b2a6-3f0c5e8d1a22");
        assert_eq!(verified.sid, "sid-1");
        assert_eq!(verified.email.as_deref(), Some("alice@example.test"));
    }

    #[test]
    fn usernames_that_are_not_email_addresses_get_no_email_claim() {
        let (_dir, jwt) = jwt();
        let token = jwt.mint(&user("alice", None), "sid-1", now()).unwrap();
        let claims = payload(&token);
        assert!(!claims.contains_key("email"));
        assert_eq!(claims["name"], "alice");
        assert_eq!(jwt.verify(&token, now()).unwrap().email, None);
    }

    #[test]
    fn payloads_are_ascii_so_atob_decodes_them() {
        let (_dir, jwt) = jwt();
        let token = jwt.mint(&user("zoë 🦀", None), "sid-1", now()).unwrap();
        let raw = b64url_decode(token.split('.').nth(1).unwrap()).unwrap();
        assert!(raw.is_ascii());
        assert!(
            String::from_utf8(raw)
                .unwrap()
                .contains(r"zo\u00eb \ud83e\udd80")
        );
        assert_eq!(payload(&token)["name"], "zoë 🦀");
    }

    #[test]
    fn expiry_allows_the_leeway_and_no_more() {
        let (_dir, jwt) = jwt();
        let issued = now() - 3600;
        let token = jwt.mint(&user("alice", None), "sid-1", issued).unwrap();
        assert!(jwt.verify(&token, now() + LEEWAY_SECONDS - 5).is_ok());
        assert_eq!(
            jwt.verify(&token, now() + LEEWAY_SECONDS + 5),
            Err(TokenError::Expired)
        );
    }

    #[test]
    fn tampered_tokens_are_refused() {
        let (_dir, jwt) = jwt();
        let token = jwt.mint(&user("alice", None), "sid-1", now()).unwrap();
        let parts: Vec<&str> = token.split('.').collect();

        let mut claims = payload(&token);
        claims.insert("sub".into(), "someone-else".into());
        let forged_payload = b64url(&serde_json::to_vec(&claims).unwrap());
        let forged = format!("{}.{forged_payload}.{}", parts[0], parts[2]);
        assert_eq!(jwt.verify(&forged, now()), Err(TokenError::Signature));

        let other_kid = b64url(br#"{"alg":"ES256","typ":"JWT","kid":"another-key"}"#);
        let wrong_kid = format!("{other_kid}.{}.{}", parts[1], parts[2]);
        assert_eq!(jwt.verify(&wrong_kid, now()), Err(TokenError::UnknownKey));

        let none_alg = b64url(br#"{"alg":"none","typ":"JWT"}"#);
        let unsigned = format!("{none_alg}.{}.", parts[1]);
        assert_eq!(jwt.verify(&unsigned, now()), Err(TokenError::Algorithm));

        assert_eq!(jwt.verify("not-a-jwt", now()), Err(TokenError::Malformed));
    }

    #[test]
    fn issuer_and_audience_must_match() {
        let (_dir, jwt) = jwt();
        let mut claims = payload(&jwt.mint(&user("alice", None), "sid-1", now()).unwrap());
        claims.insert("aud".into(), "another-app".into());
        let token = jwt.encode(&claims).unwrap();
        assert_eq!(jwt.verify(&token, now()), Err(TokenError::Audience));

        claims.insert("aud".into(), json!(["x", jwt.audience.clone()]));
        claims.insert("iss".into(), "https://elsewhere".into());
        let token = jwt.encode(&claims).unwrap();
        assert_eq!(jwt.verify(&token, now()), Err(TokenError::Issuer));
    }

    #[test]
    fn env_references_resolve_or_stay() {
        let lookup = |name: &str| (name == "APP_VERSION").then(|| "2.1".to_owned());
        assert_eq!(resolve_env("v${APP_VERSION}!", lookup), "v2.1!");
        assert_eq!(resolve_env("${MISSING}-${}", lookup), "${MISSING}-${}");
        assert_eq!(
            resolve_env("open ${APP_VERSION", lookup),
            "open ${APP_VERSION"
        );
    }
}
