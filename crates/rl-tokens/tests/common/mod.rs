//! Shared by the integration tests: a fake Azure CLI (`fixtures/fake-az.mjs`).

#![allow(dead_code)]

use std::{ffi::OsString, path::Path};

use rl_tokens::{TokenBroker, TokenBrokerConfig, TokenMode};
use serde_json::Value;

/// A fake `az` and the folder where its runs are recorded.
pub struct FakeAz {
    state: tempfile::TempDir,
}

impl FakeAz {
    pub fn new() -> Self {
        Self {
            state: scratch_dir(),
        }
    }

    /// The command that runs the fake with `behavior` (see `fixtures/fake-az.mjs`).
    pub fn program(&self, behavior: &str) -> Vec<OsString> {
        let node = which::which("node").expect("the tests need Node.js on PATH");
        vec![
            node.into(),
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/fake-az.mjs")
                .into(),
            self.state.path().into(),
            behavior.into(),
        ]
    }

    /// An Azure CLI-only broker using the fake.
    pub fn broker(&self, behavior: &str) -> TokenBroker {
        self.broker_with(behavior, TokenBrokerConfig::default())
    }

    pub fn broker_with(&self, behavior: &str, config: TokenBrokerConfig) -> TokenBroker {
        TokenBroker::new(TokenBrokerConfig {
            mode: TokenMode::Az,
            az_program: Some(self.program(behavior)),
            ..config
        })
    }

    /// The fake's runs, in the order they started: `{args, scope, tenant, start, end}`.
    pub fn calls(&self) -> Vec<Value> {
        let mut calls: Vec<Value> = std::fs::read_dir(self.state.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .map(|path| serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap())
            .collect();
        calls.sort_by_key(|call| call["start"].as_u64());
        calls
    }
}

/// A folder under the target directory, deleted when dropped.
pub fn scratch_dir() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}
