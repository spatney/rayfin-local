//! Small helpers: base64url, SHA-256, constant-time comparison, randomness, timestamps and web
//! origins.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::rand::{SecureRandom, SystemRandom};
use time::OffsetDateTime;
use url::{Host, Url};

use crate::AuthError;

pub(crate) fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub(crate) fn b64url_decode(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text).ok()
}

pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

/// Compares secrets without an early exit, so timing does not reveal how much of a guess matched.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

pub(crate) fn random_bytes<const N: usize>() -> Result<[u8; N], AuthError> {
    let mut out = [0u8; N];
    SystemRandom::new()
        .fill(&mut out)
        .map_err(|_| AuthError::Crypto("the system random number generator failed"))?;
    Ok(out)
}

pub(crate) fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

pub(crate) fn datetime_from_ms(ms: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// The origin of an `http(s)` URL as browsers serialize it (`https://App.Example:443/cb` →
/// `https://app.example`); `None` for other schemes, URLs with credentials and non-URLs.
pub(crate) fn web_origin(value: &str) -> Option<String> {
    let url = Url::parse(value.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let origin = url.origin();
    origin.is_tuple().then(|| origin.ascii_serialization())
}

/// Whether an origin from [`web_origin`] is on this machine: `localhost`, `*.localhost`,
/// `127.0.0.0/8` or `[::1]`, any port.
pub(crate) fn is_loopback_origin(origin: &str) -> bool {
    Url::parse(origin)
        .ok()
        .and_then(|url| url.host().map(|host| is_loopback_host(&host)))
        .unwrap_or(false)
}

/// Whether a request's `Host` (`name[:port]`) names this machine.
pub(crate) fn is_loopback_authority(authority: &str) -> bool {
    !authority.contains(['@', '/', '?', '#']) && is_loopback_origin(&format!("http://{authority}"))
}

fn is_loopback_host(host: &Host<&str>) -> bool {
    match host {
        Host::Domain(domain) => {
            let domain = domain.trim_end_matches('.');
            domain.eq_ignore_ascii_case("localhost")
                || domain.to_ascii_lowercase().ends_with(".localhost")
        }
        Host::Ipv4(ip) => ip.is_loopback(),
        Host::Ipv6(ip) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
        }
    }
}

/// A UTC timestamp the way the real service's JSON serializer writes `DateTimeOffset`
/// (`2026-10-13T20:00:00.123+00:00`, fraction trimmed).
pub(crate) fn format_timestamp(at: OffsetDateTime) -> String {
    let dt = at.to_offset(time::UtcOffset::UTC);
    let base = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        dt.year(),
        u8::from(dt.month()),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    );
    match dt.millisecond() {
        0 => format!("{base}+00:00"),
        millis => {
            let fraction = format!("{millis:03}");
            format!("{base}.{}+00:00", fraction.trim_end_matches('0'))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_match_the_dotnet_format() {
        assert_eq!(
            format_timestamp(OffsetDateTime::UNIX_EPOCH),
            "1970-01-01T00:00:00+00:00"
        );
        assert_eq!(
            format_timestamp(datetime_from_ms(1_791_748_800_120)),
            "2026-10-11T20:00:00.12+00:00"
        );
    }

    #[test]
    fn constant_time_eq_compares_whole_slices() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn origins_are_serialized_like_browsers_do() {
        assert_eq!(
            web_origin("https://App.Contoso.com:443/auth/callback?x=1").as_deref(),
            Some("https://app.contoso.com")
        );
        assert_eq!(
            web_origin("http://localhost:5175/").as_deref(),
            Some("http://localhost:5175")
        );
        assert_eq!(
            web_origin("http://[::1]:8080").as_deref(),
            Some("http://[::1]:8080")
        );
        for not_web in [
            "null",
            "file:///C:/app/index.html",
            "javascript:alert(1)",
            "http://user:pw@localhost:5175",
            "localhost:5175",
        ] {
            assert_eq!(web_origin(not_web), None, "{not_web}");
        }
    }

    #[test]
    fn only_this_machine_is_loopback() {
        for origin in [
            "http://localhost:5175",
            "https://app.localhost",
            "http://127.0.0.1:3000",
            "http://127.1.2.3",
            "http://[::1]:8080",
        ] {
            assert!(is_loopback_origin(origin), "{origin}");
        }
        for origin in [
            "http://localhost.evil.example",
            "http://192.168.1.10:5173",
            "http://0.0.0.0:5173",
            "https://contoso.com",
        ] {
            assert!(!is_loopback_origin(origin), "{origin}");
        }
        assert!(is_loopback_authority("LocalHost:7171"));
        assert!(is_loopback_authority("[::1]:7171"));
        assert!(!is_loopback_authority("evil.example:7171"));
        assert!(!is_loopback_authority("localhost@evil.example"));
    }
}
