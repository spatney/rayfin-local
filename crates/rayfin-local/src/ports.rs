//! Choosing the backend and frontend ports.

use anyhow::bail;

/// The backend port when nothing else is configured or remembered.
pub const DEFAULT_API_PORT: u16 = 7171;

/// How many ports after a busy preferred port are tried.
const SEARCH_RANGE: u16 = 50;

/// Where a port setting came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortSource {
    /// A command-line flag: used as is, or an error.
    Flag,
    /// A `rayfin-local.toml` setting (the key, such as `[server] port`): used as is, or an error.
    Config(&'static str),
    /// The remembered or default port: the next free one is used when it is busy.
    Preferred,
}

/// A port to use for one server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortRequest {
    pub port: u16,
    pub source: PortSource,
    /// The flag that sets this port, for hints (`--port`).
    pub flag: &'static str,
}

impl PortRequest {
    /// The first of `flag_value`, `config_value` and `preferred`.
    pub fn resolve(
        flag: &'static str,
        flag_value: Option<u16>,
        config_key: &'static str,
        config_value: Option<u16>,
        preferred: u16,
    ) -> Self {
        let (port, source) = match (flag_value, config_value) {
            (Some(port), _) => (port, PortSource::Flag),
            (None, Some(port)) => (port, PortSource::Config(config_key)),
            (None, None) => (preferred, PortSource::Preferred),
        };
        Self { port, source, flag }
    }
}

/// The port to listen on for `request`. `taken` is another server's port, never chosen.
/// Explicit ports (flag or config) are used as is; a preferred one that is busy moves to the
/// next free port.
pub fn choose_port(
    request: PortRequest,
    taken: Option<u16>,
    is_free: impl Fn(u16) -> bool,
) -> anyhow::Result<u16> {
    let PortRequest { port, source, flag } = request;
    let explicit_setting = match source {
        PortSource::Flag => Some(format!("`{flag} {port}`")),
        PortSource::Config(key) => Some(format!("`{key} = {port}` in rayfin-local.toml")),
        PortSource::Preferred => None,
    };
    if let Some(setting) = explicit_setting {
        if port != 0 && taken == Some(port) {
            bail!(
                "Port {port} ({setting}) is already the backend's port. Choose another one with \
                 `{flag}`."
            );
        }
        if !is_free(port) {
            bail!(
                "Port {port} ({setting}) is already in use. Stop the program using it, or choose \
                 another port with `{flag}`."
            );
        }
        return Ok(port);
    }

    let last = port.saturating_add(SEARCH_RANGE);
    if let Some(free) =
        (port..=last).find(|&candidate| taken != Some(candidate) && is_free(candidate))
    {
        if free != port {
            tracing::info!("port {port} is in use; using {free}");
        }
        return Ok(free);
    }
    bail!(
        "Ports {port} to {last} are all in use. Stop some of the programs using them, or choose a \
         port with `{flag}`."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preferred(port: u16) -> PortRequest {
        PortRequest::resolve("--port", None, "[server] port", None, port)
    }

    #[test]
    fn flag_beats_config_beats_preferred() {
        let request = PortRequest::resolve("--port", Some(1), "[server] port", Some(2), 3);
        assert_eq!((request.port, request.source), (1, PortSource::Flag));
        let request = PortRequest::resolve("--port", None, "[server] port", Some(2), 3);
        assert_eq!(
            (request.port, request.source),
            (2, PortSource::Config("[server] port"))
        );
        assert_eq!(preferred(3).source, PortSource::Preferred);
    }

    #[test]
    fn a_busy_preferred_port_moves_to_the_next_free_one() {
        let busy = [7171, 7172];
        let port = choose_port(preferred(7171), None, |p| !busy.contains(&p)).unwrap();
        assert_eq!(port, 7173);
        assert_eq!(choose_port(preferred(7171), None, |_| true).unwrap(), 7171);
    }

    #[test]
    fn a_preferred_port_skips_the_taken_port() {
        let port = choose_port(preferred(5173), Some(5173), |_| true).unwrap();
        assert_eq!(port, 5174);
    }

    #[test]
    fn explicit_ports_are_never_moved() {
        let flag = PortRequest::resolve("--port", Some(7171), "[server] port", None, 7171);
        let err = choose_port(flag, None, |p| p != 7171).unwrap_err();
        assert!(err.to_string().contains("`--port 7171`"), "{err}");

        let config = PortRequest::resolve("--port", None, "[server] port", Some(8080), 7171);
        let err = choose_port(config, None, |p| p != 8080).unwrap_err();
        assert!(err.to_string().contains("rayfin-local.toml"), "{err}");

        let frontend =
            PortRequest::resolve("--frontend-port", Some(7171), "[frontend] port", None, 5173);
        let err = choose_port(frontend, Some(7171), |_| true).unwrap_err();
        assert!(err.to_string().contains("backend's port"), "{err}");
    }

    #[test]
    fn the_search_gives_up_after_fifty_ports() {
        let err = choose_port(preferred(7171), None, |p| p > 7221).unwrap_err();
        assert!(err.to_string().contains("7171 to 7221"), "{err}");
        assert_eq!(
            choose_port(preferred(7171), None, |p| p == 7221).unwrap(),
            7221
        );
        assert_eq!(
            choose_port(preferred(u16::MAX), None, |_| true).unwrap(),
            u16::MAX
        );
    }
}
