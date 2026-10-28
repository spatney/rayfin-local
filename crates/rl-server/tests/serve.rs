//! Binding the loopback listeners and serving on them.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use rl_server::{HEALTH_PATH, ServerConfig, bind, build_router, is_port_free, serve};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn config() -> ServerConfig {
    ServerConfig {
        api_origin: "http://localhost:7171".to_owned(),
        publishable_key: "pk-local-test".to_owned(),
        workspace_id: "11111111-1111-4111-8111-111111111111".to_owned(),
        item_id: "22222222-2222-4222-8222-222222222222".to_owned(),
        tenant_id: None,
        portal_url: "http://localhost:7171/.rayfin-local/portal".to_owned(),
        allowed_origins: vec![],
        allowed_hosts: Some(vec![]),
        static_site: None,
        publishable_key_exempt: vec![],
    }
}

fn ipv6_loopback_available() -> bool {
    std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).is_ok()
}

async fn get(addr: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
        addr.port()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

#[tokio::test]
async fn localhost_on_port_zero_listens_on_both_loopbacks_and_serves() {
    let listeners = bind("localhost", 0).await.unwrap();
    let addrs: Vec<SocketAddr> = listeners.iter().map(|l| l.local_addr().unwrap()).collect();
    assert_eq!(addrs[0].ip(), Ipv4Addr::LOCALHOST);
    if ipv6_loopback_available() {
        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs[1].ip(), Ipv6Addr::LOCALHOST);
        assert_eq!(addrs[0].port(), addrs[1].port());
    } else {
        assert_eq!(addrs.len(), 1);
    }

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve(build_router(config(), vec![]), listeners, async {
        let _ = stopped.await;
    }));
    for addr in &addrs {
        let response = get(*addr, HEALTH_PATH).await;
        assert!(
            response.starts_with("HTTP/1.1 200 OK"),
            "{addr}: {response}"
        );
        assert!(response.ends_with(r#"{"ok":true}"#), "{addr}: {response}");
    }

    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("serve stops after the shutdown signal")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn a_port_taken_on_either_loopback_is_not_free() {
    let ipv4 = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = ipv4.local_addr().unwrap().port();
    assert!(!is_port_free("localhost", port));
    assert!(!is_port_free("127.0.0.1", port));
    assert!(bind("localhost", port).await.is_err());
    drop(ipv4);
    assert!(is_port_free("localhost", port));

    if ipv6_loopback_available() {
        // `localhost` may resolve to ::1 first, so a port taken there would reach another program.
        let ipv6 = std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).unwrap();
        let port = ipv6.local_addr().unwrap().port();
        if std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok() {
            assert!(!is_port_free("localhost", port));
            let err = bind("localhost", port).await.unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
            assert!(err.to_string().contains(&port.to_string()), "{err}");
        }
    }
}

#[tokio::test]
async fn other_hosts_bind_just_that_address() {
    let listeners = bind("127.0.0.2", 0).await;
    // Not every OS routes all of 127.0.0.0/8 to loopback (macOS only has 127.0.0.1).
    if let Ok(listeners) = listeners {
        assert_eq!(listeners.len(), 1);
        assert_eq!(
            listeners[0].local_addr().unwrap().ip(),
            Ipv4Addr::new(127, 0, 0, 2)
        );
    }
}
