//! A router that can be replaced while the backend serves it.

use std::convert::Infallible;
use std::sync::{Arc, PoisonError, RwLock};
use std::task::{Context, Poll};

use axum::Router;
use axum::extract::Request;
use axum::response::Response;
use tower::Service;
use tower::util::{Oneshot, ServiceExt as _};

/// A router that can be replaced while it is served: [`serve`](crate::serve) the router of
/// [`RouterHandle::router`], and each request goes to the router current when it arrives.
/// Requests in flight finish on the router they started with; the listeners and the connections
/// open on them are kept. Cheap to clone.
#[derive(Clone)]
pub struct RouterHandle {
    current: Arc<RwLock<Router>>,
}

impl RouterHandle {
    pub fn new(router: Router) -> Self {
        Self {
            current: Arc::new(RwLock::new(router)),
        }
    }

    /// Serves `router` from now on.
    pub fn replace(&self, router: Router) {
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = router;
    }

    /// The router to serve: it hands every request to the current one.
    pub fn router(&self) -> Router {
        Router::new().fallback_service(self.clone())
    }

    fn current(&self) -> Router {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Service<Request> for RouterHandle {
    type Response = Response;
    type Error = Infallible;
    type Future = Oneshot<Router, Request>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request) -> Self::Future {
        self.current().oneshot(request)
    }
}
