//! Shared setup: the golden models, a database with rl-db's DDL, the auth service minting real
//! sessions, and the data API.

#![allow(dead_code)]

use std::sync::Arc;

use rl_auth::{AuthService, AuthSettings};
use rl_db::Db;
use rl_graphql::{DataApi, DataApiSettings, Outcome, Request};
use rl_model::{AppModel, Caller, CallerResolver};
use serde_json::{Map, Value};

pub const APP_ID: &str = "6c6f6361-6c00-4000-8000-0000000000b1";

pub fn basic_model() -> AppModel {
    let json = include_str!("../../../rl-model/tests/fixtures/basic.analysis.json");
    AppModel::from_analysis(&serde_json::from_str(json).unwrap()).unwrap()
}

pub fn explorer_model() -> AppModel {
    let json = include_str!("../../../rl-model/tests/fixtures/explorer.analysis.json");
    AppModel::from_analysis(&serde_json::from_str(json).unwrap()).unwrap()
}

pub struct App {
    pub dir: tempfile::TempDir,
    pub db: Db,
    pub model: Arc<AppModel>,
    pub auth: Arc<AuthService>,
    pub api: Arc<DataApi>,
}

impl App {
    pub async fn new(model: AppModel) -> Self {
        Self::with_settings(model, DataApiSettings::default()).await
    }

    pub async fn with_settings(model: AppModel, settings: DataApiSettings) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("db.sqlite")).unwrap();
        let model = Arc::new(model);
        rl_db::apply(&db, &model, false).await.unwrap();
        let auth = Arc::new(
            AuthService::new(
                db.clone(),
                dir.path().join("keys"),
                AuthSettings::new(APP_ID),
            )
            .await
            .unwrap(),
        );
        let callers: Arc<dyn CallerResolver> = auth.clone();
        let api = Arc::new(DataApi::new(
            db.clone(),
            Arc::clone(&model),
            callers,
            settings,
        ));
        Self {
            dir,
            db,
            model,
            auth,
            api,
        }
    }

    /// Runs SQL statements (seed data).
    pub fn seed(&self, sql: &str) {
        self.db
            .call_blocking(|conn| conn.execute_batch(sql))
            .unwrap();
    }

    /// A signed-in user: (user id, access token).
    pub async fn user(&self, email: &str) -> (String, String) {
        let user = self
            .auth
            .create_user(email, "correct horse battery")
            .await
            .unwrap();
        let tokens = self.auth.issue_session(&user).await.unwrap();
        (user.id, tokens.access_token)
    }

    /// The caller of an access token, as the routes resolve it.
    pub fn caller(&self, token: Option<&str>) -> Caller {
        self.auth.resolve(token).unwrap()
    }

    pub async fn run(&self, caller: &Caller, query: &str) -> Response {
        self.run_with(caller, query, Map::new()).await
    }

    pub async fn run_with(
        &self,
        caller: &Caller,
        query: &str,
        variables: Map<String, Value>,
    ) -> Response {
        let request = Request {
            query: query.to_owned(),
            variables,
            operation_name: None,
        };
        match self.api.execute(caller, request).await {
            Outcome::Graphql { status, body } => Response { status, body },
            Outcome::AuthenticationRequired => Response {
                status: 401,
                body: Value::Null,
            },
        }
    }
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: Value,
}

impl Response {
    pub fn data(&self) -> &Value {
        assert!(
            self.body.get("errors").is_none(),
            "unexpected errors: {}",
            self.body
        );
        &self.body["data"]
    }

    /// The `items` of list field `field`.
    pub fn items(&self, field: &str) -> Vec<Value> {
        match self.data()[field]["items"].as_array() {
            Some(items) => items.clone(),
            None => panic!("no items in {}", self.body),
        }
    }

    /// One field of every item.
    pub fn column(&self, field: &str, column: &str) -> Vec<Value> {
        self.items(field)
            .iter()
            .map(|i| i[column].clone())
            .collect()
    }

    pub fn messages(&self) -> Vec<String> {
        self.errors()
            .iter()
            .map(|e| e["message"].as_str().unwrap().to_owned())
            .collect()
    }

    pub fn codes(&self) -> Vec<String> {
        self.errors()
            .iter()
            .map(|e| e["extensions"]["code"].as_str().unwrap_or("").to_owned())
            .collect()
    }

    pub fn errors(&self) -> Vec<Value> {
        self.body["errors"].as_array().cloned().unwrap_or_default()
    }
}
