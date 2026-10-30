//! `rayfin-local version`: the version, the host contract's version and the Rayfin SDK it
//! supports.

use std::io::Write as _;
use std::process::ExitCode;

use serde_json::json;

use crate::events::CONTRACT_VERSION;

pub fn run(json: bool) -> ExitCode {
    let version = env!("CARGO_PKG_VERSION");
    let line = if json {
        json!({
            "version": version,
            "contract": CONTRACT_VERSION,
            "sdk": {
                "supported": rl_project::supported_sdk_range(),
                "tested": rl_project::TESTED_SDK_VERSION,
            },
        })
        .to_string()
    } else {
        format!("rayfin-local {version}")
    };
    let _ = writeln!(std::io::stdout(), "{line}");
    ExitCode::SUCCESS
}
