//! The Azure CLI provider: `az account get-access-token --scope <scope> [--tenant <tenant>]`.

use std::{
    ffi::OsString,
    io::ErrorKind,
    process::Stdio,
    time::{Duration, SystemTime},
};

use serde::Deserialize;
use serde_json::Value;
use time::{PrimitiveDateTime, UtcOffset, macros::format_description};
use tokio::{io::AsyncReadExt, process::Command};

use crate::{AccessToken, TokenError, jwt};

/// How long `az` may take, including a sign-in window it may open (the Windows account broker).
const TIMEOUT: Duration = Duration::from_secs(60);

/// Entra ID errors a fresh sign-in fixes: expired or revoked sessions, MFA and other
/// interaction-required challenges.
const RELOGIN_CODES: &[&str] = &[
    "50058", "50076", "50078", "50079", "50132", "50133", "50158", "50173", "70043", "700082",
    "700084",
];

/// Entra ID errors about the tenant: it doesn't exist, or the account isn't a member of it.
const TENANT_CODES: &[&str] = &["50020", "90002", "90072"];

/// What to try when the Azure CLI can't get a token for a resource.
const ALTERNATIVES: &str = "the Rayfin CLI's login instead (`--token-source rayfin`), or \
                            override this audience's scope in rayfin-local.toml";

pub(crate) struct AzCli {
    /// The program and leading arguments; `None` runs `az` from `PATH`.
    program: Option<Vec<OsString>>,
}

impl AzCli {
    pub(crate) fn new(program: Option<Vec<OsString>>) -> Self {
        Self {
            program: program.filter(|program| !program.is_empty()),
        }
    }

    pub(crate) async fn token(
        &self,
        scope: &str,
        tenant: Option<&str>,
    ) -> Result<AccessToken, TokenError> {
        // `which` also finds `az.cmd` on Windows, which `Command` runs through `cmd.exe` with
        // batch-file argument escaping (scopes and tenant ids need no quoting there).
        let program = match &self.program {
            Some(program) => program.clone(),
            None => vec![
                which::which("az")
                    .map_err(|_| not_installed(scope))?
                    .into_os_string(),
            ],
        };
        let mut command = Command::new(&program[0]);
        command.args(&program[1..]).args([
            "account",
            "get-access-token",
            "--scope",
            scope,
            "--output",
            "json",
        ]);
        if let Some(tenant) = tenant {
            command.args(["--tenant", tenant]);
        }
        command
            .env("AZURE_CORE_NO_COLOR", "true")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| match error.kind() {
            ErrorKind::NotFound => not_installed(scope),
            _ => failure(
                scope,
                format!(
                    "could not run the Azure CLI ({}): {error}",
                    program[0].to_string_lossy()
                ),
                "Check that `az --version` works in this terminal.".to_owned(),
            ),
        })?;

        let (mut stdout, mut stderr) = (child.stdout.take(), child.stderr.take());
        let run = async {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let (read_out, read_err, status) = tokio::join!(
                read_all(stdout.as_mut(), &mut out),
                read_all(stderr.as_mut(), &mut err),
                child.wait(),
            );
            read_out?;
            read_err?;
            Ok::<_, std::io::Error>((status?, out, err))
        };
        let outcome = tokio::time::timeout(TIMEOUT, run).await;
        let (status, out, err) = match outcome {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => {
                return Err(failure(
                    scope,
                    format!("could not read the Azure CLI's output: {error}"),
                    format!("Run `az account get-access-token --scope {scope}` to check it works."),
                ));
            }
            Err(_) => {
                kill_tree(&mut child).await;
                return Err(failure(
                    scope,
                    format!(
                        "`az account get-access-token` did not finish within {} s",
                        TIMEOUT.as_secs()
                    ),
                    format!(
                        "Run `az account get-access-token --scope {scope}` yourself to see what \
                         it waits for (a sign-in?), or use {ALTERNATIVES}."
                    ),
                ));
            }
        };
        if !status.success() {
            return Err(classify_failure(
                scope,
                tenant,
                &String::from_utf8_lossy(&err),
            ));
        }
        parse_output(scope, &out)
    }
}

async fn read_all<R: tokio::io::AsyncRead + Unpin>(
    reader: Option<&mut R>,
    buffer: &mut Vec<u8>,
) -> std::io::Result<()> {
    match reader {
        Some(reader) => reader.read_to_end(buffer).await.map(drop),
        None => Ok(()),
    }
}

/// Ends `az` and its children: on Windows `az.cmd` runs Python in a child process, which may
/// have opened a sign-in window.
async fn kill_tree(child: &mut tokio::process::Child) {
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    let _ = child.start_kill();
}

/// `az account get-access-token --output json`.
#[derive(Deserialize)]
struct Output {
    #[serde(rename = "accessToken")]
    access_token: String,
    /// Unix seconds (Azure CLI 2.54 and later).
    #[serde(default)]
    expires_on: Option<Value>,
    /// Local time, e.g. `2026-10-28 15:39:35.000000`.
    #[serde(rename = "expiresOn", default)]
    expires_on_local: Option<String>,
}

fn parse_output(scope: &str, stdout: &[u8]) -> Result<AccessToken, TokenError> {
    // Never quote the output in errors: it holds the token.
    let output = serde_json::from_slice::<Output>(stdout)
        .ok()
        .filter(|output| !output.access_token.trim().is_empty())
        .ok_or_else(|| {
            failure(
                scope,
                "`az account get-access-token` did not print a token".to_owned(),
                "Update the Azure CLI (`az upgrade`) and try again.".to_owned(),
            )
        })?;
    let expires_at = output
        .expires_on
        .as_ref()
        .and_then(unix_seconds)
        .or_else(|| output.expires_on_local.as_deref().and_then(local_time))
        .or_else(|| jwt::expiry(&output.access_token))
        // Unknown expiry: short enough not to hand out a stale token for long.
        .unwrap_or_else(|| SystemTime::now() + Duration::from_secs(5 * 60));
    Ok(AccessToken {
        token: output.access_token.trim().to_owned(),
        expires_at,
    })
}

fn unix_seconds(value: &Value) -> Option<SystemTime> {
    let seconds = match value {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse().ok()?,
        _ => return None,
    };
    (seconds > 0.0 && seconds < 1e11)
        .then(|| SystemTime::UNIX_EPOCH + Duration::from_secs_f64(seconds))
}

fn local_time(text: &str) -> Option<SystemTime> {
    let text = text.trim();
    let with_fraction =
        format_description!("[year]-[month]-[day] [hour]:[minute]:[second].[subsecond]");
    let without_fraction = format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    let local = PrimitiveDateTime::parse(text, &with_fraction)
        .or_else(|_| PrimitiveDateTime::parse(text, &without_fraction))
        .ok()?;
    // The offset can be indeterminate (multi-threaded Unix processes); the caller falls back to
    // the token's own `exp` claim then.
    let offset = UtcOffset::local_offset_at(local.assume_utc()).ok()?;
    // The offset in effect at that local time, which differs from the first guess across a
    // daylight saving change.
    let offset = UtcOffset::local_offset_at(local.assume_offset(offset)).unwrap_or(offset);
    Some(local.assume_offset(offset).into())
}

/// Maps `az`'s error output to what failed and what the developer can do about it.
fn classify_failure(scope: &str, tenant: Option<&str>, stderr: &str) -> TokenError {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let errors: Vec<&str> = lines
        .iter()
        .filter_map(|line| line.strip_prefix("ERROR:"))
        .map(str::trim)
        .filter(|line| !line.starts_with("The command failed with an unexpected error"))
        .collect();
    let detail = shorten(
        errors
            .first()
            .or_else(|| lines.last())
            .copied()
            .unwrap_or("it printed no error"),
    );
    let code = aadsts_code(&errors.join("\n")).or_else(|| aadsts_code(stderr));
    let login = match tenant {
        Some(tenant) => format!("az login --tenant {tenant}"),
        None => "az login".to_owned(),
    };
    let (message, hint) = match code {
        Some(code) => {
            let message = if detail.contains(&format!("AADSTS{code}")) {
                format!("the Azure CLI could not get a token for {scope}: {detail}")
            } else {
                format!("the Azure CLI could not get a token for {scope} (AADSTS{code}): {detail}")
            };
            let hint = if code == "65001" {
                format!(
                    "The Azure CLI app is not consented for this resource in your tenant: use \
                     {ALTERNATIVES}."
                )
            } else if RELOGIN_CODES.contains(&code.as_str()) {
                format!("Sign in again with `{login} --scope {scope}`, then retry.")
            } else if TENANT_CODES.contains(&code.as_str()) {
                "Check the tenant: pass `--tenant <tenant id>` (or set it in rayfin-local.toml) \
                 and sign in to it with `az login --tenant <tenant id>`."
                    .to_owned()
            } else {
                format!("Use {ALTERNATIVES}.")
            };
            (message, hint)
        }
        None if stderr.contains("az login") => (
            format!("the Azure CLI is not signed in: {detail}"),
            format!("Run `{login}`, then retry."),
        ),
        None => (
            format!("`az account get-access-token` failed: {detail}"),
            format!("Run `az account get-access-token --scope {scope}` to see the whole error."),
        ),
    };
    failure(scope, message, hint)
}

/// The first `AADSTS<digits>` error code in `text`, without the prefix.
pub(crate) fn aadsts_code(text: &str) -> Option<String> {
    text.match_indices("AADSTS").find_map(|(at, prefix)| {
        let digits: String = text[at + prefix.len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        (!digits.is_empty()).then_some(digits)
    })
}

/// Keeps the useful part of an error line: from the `AADSTS` code on, without trace ids.
pub(crate) fn shorten(line: &str) -> String {
    let line = match line.find("AADSTS") {
        Some(at) if at <= 60 => &line[at..],
        _ => line,
    };
    let line = line.split(" Trace ID:").next().unwrap_or(line).trim();
    const MAX: usize = 400;
    if line.chars().count() <= MAX {
        line.to_owned()
    } else {
        format!("{}…", line.chars().take(MAX).collect::<String>())
    }
}

fn not_installed(scope: &str) -> TokenError {
    failure(
        scope,
        "the Azure CLI (`az`) is not installed or not on PATH".to_owned(),
        "Install the Azure CLI (https://aka.ms/installazurecli) and run `az login`, or use the \
         Rayfin CLI's login (`--token-source rayfin`)."
            .to_owned(),
    )
}

pub(crate) fn failure(scope: &str, message: String, hint: String) -> TokenError {
    TokenError {
        scope: scope.to_owned(),
        message,
        hint: Some(hint),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POWER_BI: &str = "https://analysis.windows.net/powerbi/api/.default";

    #[test]
    fn not_signed_in_suggests_az_login_for_the_tenant() {
        let stderr = "ERROR: Please run 'az login' to setup account.\n";
        let error = classify_failure(POWER_BI, None, stderr);
        assert_eq!(
            error.message,
            "the Azure CLI is not signed in: Please run 'az login' to setup account."
        );
        assert_eq!(error.hint.as_deref(), Some("Run `az login`, then retry."));
        let error = classify_failure(POWER_BI, Some("contoso.onmicrosoft.com"), stderr);
        assert_eq!(
            error.hint.as_deref(),
            Some("Run `az login --tenant contoso.onmicrosoft.com`, then retry.")
        );
    }

    #[test]
    fn consent_errors_show_the_code_and_suggest_the_rayfin_cli_or_a_scope_override() {
        let stderr = "WARNING: Run the command below to authenticate interactively; additional \
                      arguments may be added as needed:\n\
                      az login --tenant \"t\" --scope \"https://ai.azure.com/.default\" \
                      --claims-challenge \"eyJhY2Nlc3NfdG9rZW4iOiB7fX0=\"\n\
                      ERROR: V2Error: invalid_grant AADSTS65001: The user or administrator has \
                      not consented to use the application with ID '04b07795'. Trace ID: abc \
                      Correlation ID: def Timestamp: 2026-10-28 21:25:22Z\n";
        let error = classify_failure("https://ai.azure.com/.default", None, stderr);
        assert_eq!(
            error.message,
            "the Azure CLI could not get a token for https://ai.azure.com/.default: AADSTS65001: \
             The user or administrator has not consented to use the application with ID \
             '04b07795'."
        );
        let hint = error.hint.unwrap();
        assert!(hint.contains("--token-source rayfin"), "{hint}");
        assert!(hint.contains("rayfin-local.toml"), "{hint}");
        assert!(!error.message.contains("claims-challenge"));
    }

    #[test]
    fn other_entra_errors_name_their_code() {
        // As printed by az 2.91 for a resource that doesn't exist in the tenant.
        let stderr = "WARNING: V2Error: invalid_resource AADSTS500011: The resource principal \
                      named api://x was not found in the tenant named t. Trace ID: 1\n\
                      ERROR: V2Error: invalid_resource AADSTS500011: The resource principal \
                      named api://x was not found in the tenant named t. Trace ID: 2\n\
                      Run the command below to authenticate interactively; additional arguments \
                      may be added as needed:\naz logout\naz login\n";
        let error = classify_failure("api://x/.default", None, stderr);
        assert!(
            error
                .message
                .contains("AADSTS500011: The resource principal named api://x")
        );
        assert!(!error.message.contains("Trace ID"));
        assert!(
            error
                .hint
                .unwrap()
                .starts_with("Use the Rayfin CLI's login instead")
        );

        let stderr = "WARNING: V2Error: invalid_grant AADSTS50020: User account '{EUII Hidden}' \
                      does not exist in tenant 'Contoso'.\n\
                      ERROR: User canceled the flow. Status: Response_Status.Status_UserCanceled\n";
        let error = classify_failure(POWER_BI, Some("contoso"), stderr);
        assert!(
            error
                .message
                .contains("(AADSTS50020): User canceled the flow.")
        );
        assert!(
            error
                .hint
                .unwrap()
                .contains("az login --tenant <tenant id>")
        );

        let stderr = "ERROR: AADSTS700082: The refresh token has expired due to inactivity.\n";
        let error = classify_failure(POWER_BI, None, stderr);
        assert_eq!(
            error.hint.unwrap(),
            format!("Sign in again with `az login --scope {POWER_BI}`, then retry.")
        );
    }

    #[test]
    fn unexpected_failures_keep_the_relevant_line() {
        let stderr = "ERROR: The command failed with an unexpected error. Here is the \
                      traceback:\nERROR: Unable to get authority configuration for \
                      https://login.microsoftonline.com/x.\nTraceback (most recent call last):\n";
        let error = classify_failure(POWER_BI, None, stderr);
        assert_eq!(
            error.message,
            "`az account get-access-token` failed: Unable to get authority configuration for \
             https://login.microsoftonline.com/x."
        );
    }

    #[test]
    fn expiry_comes_from_expires_on_then_local_time_then_the_token() {
        let token = |json: &str| parse_output(POWER_BI, json.as_bytes()).unwrap().expires_at;
        let unix = |seconds| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
        assert_eq!(
            token(
                r#"{"accessToken":"t","expiresOn":"2000-01-01 00:00:00.000000","expires_on":1793227175}"#
            ),
            unix(1_793_227_175)
        );
        let instant = time::OffsetDateTime::from_unix_timestamp(1_793_227_175).unwrap();
        // Unix can't tell the local offset in a multi-threaded process; parse_output then falls
        // back to the token's claims, covered below.
        if let Ok(offset) = UtcOffset::local_offset_at(instant) {
            let local = instant.to_offset(offset);
            let local = format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.000000",
                local.year(),
                u8::from(local.month()),
                local.day(),
                local.hour(),
                local.minute(),
                local.second()
            );
            assert_eq!(
                token(&format!(r#"{{"accessToken":"t","expiresOn":"{local}"}}"#)),
                unix(1_793_227_175)
            );
        }
        let jwt = jwt::fake_jwt(&serde_json::json!({ "exp": 1_793_227_175 }));
        assert_eq!(
            token(&format!(r#"{{"accessToken":"{jwt}"}}"#)),
            unix(1_793_227_175)
        );
    }

    #[test]
    fn unreadable_output_is_an_error_that_does_not_quote_it() {
        let error =
            parse_output(POWER_BI, b"{\"accessToken\": 42, \"secret\": \"eyJ\"}").unwrap_err();
        assert!(!error.message.contains("eyJ"));
        assert!(parse_output(POWER_BI, br#"{"accessToken":""}"#).is_err());
    }
}
