//! The `authorization_code` grant: verification codes from the Fabric sign-in handoff
//! (`codeType: "fabric_handoff"`) and magic links.

use axum::http::StatusCode;

use crate::{AuthService, TokenResponse, routes::ApiError, routes::Fields};

/// A verification-code exchange, validated as far as the request shape goes.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct CodeExchange {
    pub(crate) verification_code: String,
    pub(crate) code_verifier: String,
    pub(crate) redirect_uri: String,
    /// `fabric_handoff` for the broker popup; absent for magic links.
    pub(crate) code_type: Option<String>,
}

impl AuthService {
    /// Validates the request like the real service, then redeems the code.
    pub(crate) async fn authorization_code_grant(
        &self,
        fields: &Fields,
    ) -> Result<TokenResponse, ApiError> {
        let code = fields.non_blank("code")?;
        let verification_code = fields.non_blank("verificationCode")?;
        let verification_code = match (code, verification_code) {
            (Some(_), Some(_)) => {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "AMBIGUOUS_CODE_FIELDS",
                    "Request cannot contain both 'code' and 'verification_code'. Use 'code' for \
                     OAuth authorization codes or 'verification_code' for passwordless magic links.",
                ));
            }
            (None, None) => {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "MISSING_CODE",
                    "Either 'code' (for OAuth) or 'verification_code' (for passwordless) is required",
                ));
            }
            (Some(_), None) => {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "unsupported_grant_type",
                    "OAuth authorization code grant is not yet implemented. Use verification_code \
                     for passwordless authentication.",
                ));
            }
            (None, Some(verification_code)) => verification_code,
        };
        let code_verifier = fields
            .non_blank("codeVerifier")?
            .ok_or_else(|| ApiError::invalid_request("code_verifier is required for PKCE"))?;
        if !is_valid_code_verifier(code_verifier) {
            return Err(ApiError::invalid_request(
                "code_verifier must be 43-128 characters using unreserved URI charset",
            ));
        }
        let redirect_uri = fields.non_blank("redirectUri")?.ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "MISSING_REDIRECT_URI",
                "redirect_uri is required",
            )
        })?;
        let exchange = CodeExchange {
            verification_code: verification_code.to_owned(),
            code_verifier: code_verifier.to_owned(),
            redirect_uri: redirect_uri.to_owned(),
            code_type: fields.text("codeType")?.map(str::to_owned),
        };
        self.redeem_verification_code(exchange).await
    }

    /// Redeems a verification code for a session.
    ///
    /// TODO(auth-portal): look up the single-use handoff code the portal issued (a new schema
    /// step), check PKCE (`base64url(SHA-256(code_verifier))` against the stored challenge, in
    /// constant time) and the redirect URI, burn the code, then `issue_session`. Until then no
    /// code exists, and the answer is the real service's for an unknown code (401; the SDK maps
    /// it to `INVALID_CODE` after one refresh-and-replay).
    async fn redeem_verification_code(
        &self,
        _exchange: CodeExchange,
    ) -> Result<TokenResponse, ApiError> {
        Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "INVALID_CODE",
            "Verification code is invalid",
        ))
    }
}

/// RFC 7636 §4.1: 43-128 characters of `[A-Za-z0-9-._~]`.
pub(crate) fn is_valid_code_verifier(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_verifiers_follow_rfc_7636() {
        assert!(is_valid_code_verifier(&"a".repeat(43)));
        assert!(is_valid_code_verifier(&format!("{}-._~", "Z9".repeat(20))));
        assert!(!is_valid_code_verifier(&"a".repeat(42)));
        assert!(!is_valid_code_verifier(&"a".repeat(129)));
        assert!(!is_valid_code_verifier(&format!("{}+/", "a".repeat(43))));
    }
}
