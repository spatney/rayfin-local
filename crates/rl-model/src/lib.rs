//! The app model rayfin-local serves, analysed from the project's `rayfin/data` entities by the
//! project's own `@microsoft/rayfin-core` (`js/analyze.mjs`), and the [`Caller`] every backend
//! route acts for.

pub mod caller;

pub use caller::{Caller, CallerError, CallerResolver, Session, bearer_token};
