//! Listening sockets and the server loop.

use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinSet;

/// How long [`serve`] waits for open requests after the shutdown signal before dropping them.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// How many ephemeral ports [`bind`] tries before giving up on one free on both loopbacks.
const EPHEMERAL_ATTEMPTS: usize = 10;

/// Hosts served on both loopback addresses, so `http://localhost:<port>` reaches this server
/// whether a client resolves `localhost` to `127.0.0.1` or to `::1` first.
fn is_dual_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || matches!(host, "127.0.0.1" | "::1" | "[::1]")
}

/// `::1` can't be bound because IPv6 is disabled or missing; any other failure (the port is taken
/// or reserved) is a real conflict.
fn ipv6_unavailable(err: &io::Error) -> bool {
    !matches!(
        err.kind(),
        io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
    )
}

fn bind_error(addr: SocketAddr, err: io::Error) -> io::Error {
    let hint = match err.kind() {
        io::ErrorKind::AddrInUse => format!(" Port {} is in use; choose another.", addr.port()),
        io::ErrorKind::PermissionDenied => {
            format!(
                " Port {} is reserved or not permitted; choose another.",
                addr.port()
            )
        }
        _ => String::new(),
    };
    io::Error::new(
        err.kind(),
        format!("Could not listen on {addr}: {err}.{hint}"),
    )
}

async fn bind_addr(addr: SocketAddr) -> io::Result<TcpListener> {
    TcpListener::bind(addr)
        .await
        .map_err(|err| bind_error(addr, err))
}

/// Binds the server's listeners.
///
/// For `localhost`, `127.0.0.1` and `::1` this binds both `127.0.0.1` and `[::1]` (IPv6 is skipped
/// when the machine has none); an IPv6 port taken by another program is an error, since
/// `localhost` could then reach that program. Port 0 picks an ephemeral port free on both. Other
/// hosts bind just that address.
pub async fn bind(host: &str, port: u16) -> io::Result<Vec<TcpListener>> {
    if !is_dual_loopback(host) {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let listener = TcpListener::bind((host, port)).await.map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("Could not listen on {host}:{port}: {err}."),
            )
        })?;
        return Ok(vec![listener]);
    }

    let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    if port != 0 {
        let ipv4 = bind_addr(v4).await?;
        let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
        return match TcpListener::bind(v6).await {
            Ok(ipv6) => Ok(vec![ipv4, ipv6]),
            Err(err) if ipv6_unavailable(&err) => Ok(vec![ipv4]),
            Err(err) => Err(bind_error(v6, err)),
        };
    }

    for _ in 0..EPHEMERAL_ATTEMPTS {
        let ipv4 = bind_addr(v4).await?;
        let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, ipv4.local_addr()?.port()));
        match TcpListener::bind(v6).await {
            Ok(ipv6) => return Ok(vec![ipv4, ipv6]),
            Err(err) if ipv6_unavailable(&err) => return Ok(vec![ipv4]),
            // The port is free on 127.0.0.1 but taken on ::1; try another one.
            Err(_) => continue,
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AddrInUse,
        "Could not find a port that is free on both 127.0.0.1 and [::1].",
    ))
}

/// Whether [`bind`] would succeed for `host` and `port` right now (port 0 is always free).
pub fn is_port_free(host: &str, port: u16) -> bool {
    if port == 0 {
        return true;
    }
    if !is_dual_loopback(host) {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        return std::net::TcpListener::bind((host, port)).is_ok();
    }
    let Ok(_ipv4) = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)) else {
        return false;
    };
    match std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, port)) {
        Ok(_) => true,
        Err(err) => ipv6_unavailable(&err),
    }
}

/// Serves `router` on every listener until `shutdown` completes, then lets open requests finish
/// for up to 5 seconds. Handlers can extract `ConnectInfo<SocketAddr>`.
pub async fn serve(
    router: Router,
    listeners: Vec<TcpListener>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    if listeners.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "There is no listener to serve on.",
        ));
    }

    let (stop_tx, stop_rx) = watch::channel(false);
    let forward = tokio::spawn(async move {
        shutdown.await;
        let _ = stop_tx.send(true);
    });
    let stopped = |mut rx: watch::Receiver<bool>| async move {
        // An error means the sender is gone, which also means stop.
        let _ = rx.wait_for(|stop| *stop).await;
    };

    let mut servers = JoinSet::new();
    for listener in listeners {
        if let Ok(addr) = listener.local_addr() {
            tracing::debug!(%addr, "listening");
        }
        let app = router
            .clone()
            .into_make_service_with_connect_info::<SocketAddr>();
        let signal = stopped(stop_rx.clone());
        servers.spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(signal)
                .await
        });
    }

    let result = tokio::select! {
        result = join_all(&mut servers) => result,
        () = async {
            stopped(stop_rx).await;
            tokio::time::sleep(DRAIN_TIMEOUT).await;
        } => {
            tracing::debug!("requests still open after shutdown; dropping them");
            Ok(())
        }
    };
    forward.abort();
    result
}

async fn join_all(servers: &mut JoinSet<io::Result<()>>) -> io::Result<()> {
    while let Some(joined) = servers.join_next().await {
        joined.map_err(|err| io::Error::other(format!("The HTTP server task failed: {err}")))??;
    }
    Ok(())
}
