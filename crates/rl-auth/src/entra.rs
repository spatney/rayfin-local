//! The external Entra exchange routes (`/api/auth/v1/brokered/*`).
//!
//! TODO(entra-exchange): `POST /brokered/token` takes `Authorization: Bearer <Entra token>`,
//! validates it, maps it to a user with `find_or_create_external_user` and answers with
//! `issue_session`. Until then both routes answer the way the SDK reads "not enabled" (400,
//! `EXCHANGE_NOT_ENABLED`) or "not available" (404, `NOT_AVAILABLE`).

use std::sync::Arc;

use axum::{
    extract::State,
    response::{IntoResponse, Response},
};

use crate::{AuthService, routes::ApiError};

const NOT_ENABLED: &str = "External Entra exchange is not enabled for this project";

pub(crate) async fn brokered_token(State(auth): State<Arc<AuthService>>) -> Response {
    auth.brokered_unavailable().into_response()
}

pub(crate) async fn brokered_authorize_external(State(auth): State<Arc<AuthService>>) -> Response {
    auth.brokered_unavailable().into_response()
}

impl AuthService {
    fn brokered_unavailable(&self) -> ApiError {
        if !self.settings.enabled {
            return ApiError::not_found();
        }
        if !(self.settings.fabric_enabled && self.settings.external_entra_exchange) {
            return ApiError::mode_disabled(NOT_ENABLED);
        }
        ApiError::not_found()
    }
}
