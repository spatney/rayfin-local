//! TDS connections to a connector's database with `tiberius` (rustls on ring), a small pool, and
//! [`SqlBackend`] over them.
//!
//! Fabric SQL endpoints take a Microsoft Entra access token at login (TDS `FEDAUTH`): the
//! developer's token for `https://database.windows.net/.default` ([`Auth::Entra`]). A login
//! redirected to another gateway (`Routing`) is followed once. Connections are encrypted and
//! the server certificate is checked against the platform's roots.
//!
//! A connection that fails mid-statement or times out is dropped, never reused; a pooled one that
//! the server closed while idle is replaced once, for reads. Writes run in a transaction that a
//! dropped [`SqlTransaction`] rolls back by closing its connection.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rl_graphql::tsql::backend::{
    BoxFuture, Param, Row, SqlBackend, SqlError, SqlTransaction, Statement, Value,
};
use rl_tokens::TokenSource;
use tiberius::numeric::Numeric;
use tiberius::{AuthMethod, Client, ColumnData, Config, EncryptionLevel, Query};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

type TdsClient = Client<Compat<TcpStream>>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(40);
/// A statement taking longer is abandoned (its connection dropped).
const STATEMENT_TIMEOUT: Duration = Duration::from_secs(120);
/// Connections idle longer are closed instead of reused.
const MAX_IDLE: Duration = Duration::from_secs(5 * 60);
/// Connections older than this are closed when returned.
const MAX_AGE: Duration = Duration::from_secs(45 * 60);
const MAX_CONNECTIONS: usize = 4;

/// How a connection signs in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Auth {
    /// The developer's Entra token for `scope` (Fabric).
    Entra { scope: String },
    /// SQL authentication (a local SQL Server, for tests).
    Password { user: String, password: String },
}

/// What to connect to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub auth: Auth,
    /// Skip the certificate check (local SQL Server with a self-signed certificate).
    pub trust_server_certificate: bool,
}

/// Describes failures in terms of the connector they happened for.
#[derive(Clone, Debug)]
pub struct Context {
    /// The connector's name.
    pub connector: String,
    /// The item, for "ask for access to …" (`SQL database <id> in workspace <ws>`).
    pub item: String,
}

struct Idle {
    client: TdsClient,
    created: Instant,
    since: Instant,
}

/// A small pool of connections to one database.
pub struct Pool {
    endpoint: Endpoint,
    context: Context,
    tokens: Arc<dyn TokenSource>,
    idle: Mutex<Vec<Idle>>,
    permits: Arc<Semaphore>,
}

impl Pool {
    pub fn new(endpoint: Endpoint, context: Context, tokens: Arc<dyn TokenSource>) -> Arc<Self> {
        Arc::new(Self {
            endpoint,
            context,
            tokens,
            idle: Mutex::new(Vec::new()),
            permits: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
        })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// A connection: an idle one when there is a fresh one, else a new one.
    async fn get(self: &Arc<Self>) -> Result<Pooled, SqlError> {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|_| SqlError::Unavailable("The connection pool is closed.".into()))?;
        loop {
            let idle = self.idle.lock().pop();
            match idle {
                Some(idle) if idle.since.elapsed() < MAX_IDLE => {
                    return Ok(Pooled {
                        client: Some(idle.client),
                        created: idle.created,
                        reused: true,
                        pool: Arc::clone(self),
                        _permit: permit,
                    });
                }
                Some(_) => continue,
                None => break,
            }
        }
        let client = self.connect().await?;
        Ok(Pooled {
            client: Some(client),
            created: Instant::now(),
            reused: false,
            pool: Arc::clone(self),
            _permit: permit,
        })
    }

    async fn connect(&self) -> Result<TdsClient, SqlError> {
        let endpoint = &self.endpoint;
        let mut config = Config::new();
        config.host(&endpoint.host);
        config.port(endpoint.port);
        config.database(&endpoint.database);
        config.application_name(concat!("rayfin-local/", env!("CARGO_PKG_VERSION")));
        config.encryption(EncryptionLevel::Required);
        if endpoint.trust_server_certificate {
            config.trust_cert();
        }
        config.authentication(match &endpoint.auth {
            Auth::Entra { scope } => {
                let token = self.tokens.token(scope).await.map_err(|error| {
                    let hint = error.hint.map_or_else(
                        || " Run `az login`, then try again.".to_owned(),
                        |hint| format!(" {hint}"),
                    );
                    SqlError::Unavailable(format!(
                        "Connector '{}' signs in to its database with your Azure identity, and \
                         no token could be had: {}.{hint}",
                        self.context.connector,
                        error.message.trim_end_matches('.')
                    ))
                })?;
                AuthMethod::aad_token(token.token)
            }
            Auth::Password { user, password } => AuthMethod::sql_server(user, password),
        });
        match self.login(config.clone()).await {
            Err(tiberius::error::Error::Routing { host, port }) => {
                config.host(host);
                config.port(port);
                self.login(config).await
            }
            other => other,
        }
        .map_err(|error| self.login_error(error))
    }

    async fn login(&self, config: Config) -> Result<TdsClient, tiberius::error::Error> {
        let address = config.get_addr();
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&address))
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "the connection timed out")
            })??;
        tcp.set_nodelay(true)?;
        tokio::time::timeout(LOGIN_TIMEOUT, Client::connect(config, tcp.compat_write()))
            .await
            .map_err(|_| tiberius::error::Error::Io {
                kind: std::io::ErrorKind::TimedOut,
                message: "the login timed out".into(),
            })?
    }

    /// What a failed connection means and what to do about it.
    fn login_error(&self, error: tiberius::error::Error) -> SqlError {
        let Context { connector, item } = &self.context;
        let host = &self.endpoint.host;
        let message = match &error {
            tiberius::error::Error::Server(token) => match token.code() {
                18456 | 4060 | 916 => format!(
                    "The database of connector '{connector}' refused your Azure identity ({}). \
                     rayfin-local connects as you, even for `auth: application` connectors: ask \
                     the owner of {item} for read access (or the access your app needs), or run \
                     `az login` with the account that has it.",
                    token.message()
                ),
                40615 | 40914 => format!(
                    "The database server of connector '{connector}' ({host}) refuses connections \
                     from this network ({}). Connect from an allowed network or VPN, or ask the \
                     item's owner to allow your address.",
                    token.message()
                ),
                40613 | 40197 | 40501 | 49918 | 49919 | 49920 => format!(
                    "The database of connector '{connector}' is busy or not available right now \
                     ({}). Try again in a minute; a paused Fabric capacity must be resumed first.",
                    token.message()
                ),
                code => format!(
                    "The database of connector '{connector}' refused the connection: {} (error \
                     {code}).",
                    token.message()
                ),
            },
            tiberius::error::Error::Io { kind, message } => format!(
                "Couldn't connect to the database of connector '{connector}' at {host},{} ({kind:?}: \
                 {message}). Check your network connection, VPN and firewall (TCP port {} must be \
                 open), then try again.",
                self.endpoint.port, self.endpoint.port
            ),
            tiberius::error::Error::Tls(message) => format!(
                "The secure connection to the database of connector '{connector}' ({host}) failed: \
                 {message}. A proxy that inspects TLS traffic can cause this."
            ),
            other => format!(
                "Couldn't sign in to the database of connector '{connector}' ({host}): {other}."
            ),
        };
        SqlError::Unavailable(message)
    }

    fn give_back(&self, client: TdsClient, created: Instant) {
        if created.elapsed() < MAX_AGE {
            self.idle.lock().push(Idle {
                client,
                created,
                since: Instant::now(),
            });
        }
    }
}

/// A connection taken from the pool; returned on drop unless it broke.
struct Pooled {
    client: Option<TdsClient>,
    created: Instant,
    /// It sat idle in the pool (the server may have closed it meanwhile).
    reused: bool,
    pool: Arc<Pool>,
    _permit: OwnedSemaphorePermit,
}

impl Pooled {
    fn client(&mut self) -> &mut TdsClient {
        self.client
            .as_mut()
            .expect("a pooled connection is used until dropped")
    }

    /// Never reuse this connection.
    fn discard(&mut self) {
        self.client = None;
    }
}

impl Drop for Pooled {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            self.pool.give_back(client, self.created);
        }
    }
}

/// Runs `statement` and returns its rows (the first result set), or the number of rows it
/// changed (`rows: false`).
async fn run(
    client: &mut TdsClient,
    statement: &Statement,
    rows: bool,
) -> Result<(Vec<Row>, u64), tiberius::error::Error> {
    let mut query = Query::new(statement.text.as_str());
    for param in &statement.params {
        match param {
            Param::Null => query.bind(Option::<&str>::None),
            Param::Bool(value) => query.bind(*value),
            Param::Int(value) => query.bind(*value),
            Param::Float(value) => query.bind(*value),
            Param::Decimal { value, scale } => query.bind(Numeric::new_with_scale(*value, *scale)),
            Param::Text(value) | Param::DateTime(value) => query.bind(value.as_str()),
        }
    }
    if rows {
        let results = query.query(client).await?.into_results().await?;
        let rows = results
            .into_iter()
            .next()
            .unwrap_or_default()
            .into_iter()
            .map(|row| row.into_iter().map(value).collect())
            .collect();
        Ok((rows, 0))
    } else {
        let result = query.execute(client).await?;
        Ok((Vec::new(), result.rows_affected().iter().sum()))
    }
}

/// Whether `error` means the connection is gone (rather than the statement refused).
fn connection_lost(error: &tiberius::error::Error) -> bool {
    matches!(
        error,
        tiberius::error::Error::Io { .. } | tiberius::error::Error::Protocol(_)
    )
}

/// A failed statement as the executor reports it.
fn statement_error(connector: &str, error: tiberius::error::Error) -> SqlError {
    match error {
        tiberius::error::Error::Server(token) => SqlError::Database {
            number: token.code(),
            message: token.message().to_owned(),
        },
        other if connection_lost(&other) => SqlError::Unavailable(format!(
            "The connection to the database of connector '{connector}' was lost ({other}). Try \
             again."
        )),
        other => SqlError::Database {
            number: 0,
            message: other.to_string(),
        },
    }
}

fn timed_out(connector: &str) -> SqlError {
    SqlError::Unavailable(format!(
        "A query of connector '{connector}' took longer than {} seconds and was abandoned.",
        STATEMENT_TIMEOUT.as_secs()
    ))
}

/// One statement on a pooled connection, with the timeout; the connection is discarded when it
/// failed or timed out. `Err(None)` means the connection was lost.
async fn run_pooled(
    pooled: &mut Pooled,
    statement: &Statement,
    rows: bool,
) -> Result<(Vec<Row>, u64), Option<SqlError>> {
    let connector = pooled.pool.context.connector.clone();
    match tokio::time::timeout(STATEMENT_TIMEOUT, run(pooled.client(), statement, rows)).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(error)) => {
            if connection_lost(&error) {
                pooled.discard();
                if pooled.reused {
                    return Err(None);
                }
            }
            Err(Some(statement_error(&connector, error)))
        }
        Err(_) => {
            pooled.discard();
            Err(Some(timed_out(&connector)))
        }
    }
}

/// [`SqlBackend`] over a [`Pool`].
pub struct TdsBackend(pub Arc<Pool>);

impl SqlBackend for TdsBackend {
    fn query<'a>(&'a self, statement: &'a Statement) -> BoxFuture<'a, Result<Vec<Row>, SqlError>> {
        let pool = &self.0;
        Box::pin(async move {
            let mut pooled = pool.get().await?;
            match run_pooled(&mut pooled, statement, true).await {
                Ok((rows, _)) => Ok(rows),
                Err(Some(error)) => Err(error),
                // A pooled connection the server closed: once more on a new one.
                Err(None) => {
                    drop(pooled);
                    let mut fresh = pool.get().await?;
                    match run_pooled(&mut fresh, statement, true).await {
                        Ok((rows, _)) => Ok(rows),
                        Err(error) => Err(error.unwrap_or_else(|| {
                            SqlError::Unavailable(format!(
                                "The connection to the database of connector '{}' was lost. Try \
                                 again.",
                                pool.context.connector
                            ))
                        })),
                    }
                }
            }
        })
    }

    fn begin(&self) -> BoxFuture<'_, Result<Box<dyn SqlTransaction>, SqlError>> {
        let pool = &self.0;
        Box::pin(async move {
            let mut attempts = 0;
            loop {
                attempts += 1;
                let mut pooled = pool.get().await?;
                let reused = pooled.reused;
                let begun = tokio::time::timeout(
                    STATEMENT_TIMEOUT,
                    simple(pooled.client(), "SET XACT_ABORT ON; BEGIN TRANSACTION"),
                )
                .await;
                match begun {
                    Ok(Ok(())) => {
                        return Ok(Box::new(Transaction {
                            pooled: Some(pooled),
                        }) as Box<dyn SqlTransaction>);
                    }
                    Ok(Err(error)) => {
                        pooled.discard();
                        if reused && attempts == 1 && connection_lost(&error) {
                            continue;
                        }
                        return Err(statement_error(&pool.context.connector, error));
                    }
                    Err(_) => {
                        pooled.discard();
                        return Err(timed_out(&pool.context.connector));
                    }
                }
            }
        })
    }
}

/// A batch without parameters (transaction control: `sp_executesql` would refuse a changed
/// transaction count).
async fn simple(client: &mut TdsClient, text: &str) -> Result<(), tiberius::error::Error> {
    client.simple_query(text).await?.into_results().await?;
    Ok(())
}

/// A write in progress; dropping it uncommitted closes the connection, which rolls it back.
struct Transaction {
    pooled: Option<Pooled>,
}

impl Transaction {
    async fn run(
        &mut self,
        statement: &Statement,
        rows: bool,
    ) -> Result<(Vec<Row>, u64), SqlError> {
        let pooled = self
            .pooled
            .as_mut()
            .expect("a transaction runs until committed");
        let connector = pooled.pool.context.connector.clone();
        match tokio::time::timeout(STATEMENT_TIMEOUT, run(pooled.client(), statement, rows)).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => Err(statement_error(&connector, error)),
            Err(_) => Err(timed_out(&connector)),
        }
    }
}

impl SqlTransaction for Transaction {
    fn query<'a>(
        &'a mut self,
        statement: &'a Statement,
    ) -> BoxFuture<'a, Result<Vec<Row>, SqlError>> {
        Box::pin(async move { self.run(statement, true).await.map(|(rows, _)| rows) })
    }

    fn execute<'a>(&'a mut self, statement: &'a Statement) -> BoxFuture<'a, Result<u64, SqlError>> {
        Box::pin(async move { self.run(statement, false).await.map(|(_, count)| count) })
    }

    fn commit(mut self: Box<Self>) -> BoxFuture<'static, Result<(), SqlError>> {
        Box::pin(async move {
            let mut pooled = self.pooled.take().expect("committed once");
            let connector = pooled.pool.context.connector.clone();
            match tokio::time::timeout(
                STATEMENT_TIMEOUT,
                simple(pooled.client(), "COMMIT TRANSACTION"),
            )
            .await
            {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => {
                    pooled.discard();
                    Err(statement_error(&connector, error))
                }
                Err(_) => {
                    pooled.discard();
                    Err(timed_out(&connector))
                }
            }
        })
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        if let Some(mut pooled) = self.pooled.take() {
            // Closing the connection rolls the transaction back.
            pooled.discard();
        }
    }
}

/// A result column's value.
fn value(data: ColumnData<'static>) -> Value {
    match data {
        ColumnData::U8(v) => v.map_or(Value::Null, |v| Value::Int(v.into())),
        ColumnData::I16(v) => v.map_or(Value::Null, |v| Value::Int(v.into())),
        ColumnData::I32(v) => v.map_or(Value::Null, |v| Value::Int(v.into())),
        ColumnData::I64(v) => v.map_or(Value::Null, Value::Int),
        ColumnData::F32(v) => v.map_or(Value::Null, |v| Value::Float(v.into())),
        ColumnData::F64(v) => v.map_or(Value::Null, Value::Float),
        ColumnData::Bit(v) => v.map_or(Value::Null, Value::Bool),
        ColumnData::String(v) => v.map_or(Value::Null, |v| Value::Text(v.into_owned())),
        ColumnData::Guid(v) => v.map_or(Value::Null, |v| Value::Uuid(v.hyphenated().to_string())),
        ColumnData::Numeric(v) => v.map_or(Value::Null, |v| Value::Decimal(v.to_string())),
        ColumnData::Binary(v) => v.map_or(Value::Null, |v| Value::Other(hex(&v))),
        ColumnData::Xml(v) => v.map_or(Value::Null, |v| Value::Other(v.to_string())),
        ColumnData::DateTime(v) => v.map_or(Value::Null, |v| {
            let nanos = i64::from(v.seconds_fragments()) * 1_000_000_000 / 300;
            datetime(1900, i64::from(v.days()), nanos, None)
        }),
        ColumnData::SmallDateTime(v) => v.map_or(Value::Null, |v| {
            let nanos = i64::from(v.seconds_fragments()) * 60_000_000_000;
            datetime(1900, i64::from(v.days()), nanos, None)
        }),
        ColumnData::Date(v) => v.map_or(Value::Null, |v| datetime(1, i64::from(v.days()), 0, None)),
        ColumnData::Time(v) => v.map_or(Value::Null, |v| {
            let nanos = time_nanos(v.increments(), v.scale());
            Value::DateTime(clock(nanos))
        }),
        ColumnData::DateTime2(v) => v.map_or(Value::Null, |v| {
            let nanos = time_nanos(v.time().increments(), v.time().scale());
            datetime(1, i64::from(v.date().days()), nanos, None)
        }),
        ColumnData::DateTimeOffset(v) => v.map_or(Value::Null, |v| {
            let inner = v.datetime2();
            let nanos = time_nanos(inner.time().increments(), inner.time().scale());
            datetime(1, i64::from(inner.date().days()), nanos, Some(v.offset()))
        }),
    }
}

fn time_nanos(increments: u64, scale: u8) -> i64 {
    let per_increment = 10u64.pow(9u32.saturating_sub(u32::from(scale)));
    i64::try_from(increments.saturating_mul(per_increment)).unwrap_or(i64::MAX)
}

/// `HH:MM:SS.fffffff` of `nanos` since midnight.
fn clock(nanos: i64) -> String {
    let seconds = nanos / 1_000_000_000;
    format!(
        "{:02}:{:02}:{:02}.{:07}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60,
        nanos % 1_000_000_000 / 100
    )
}

/// The date `days` after January 1 of `epoch_year` at `nanos` past midnight, with the offset (in
/// minutes) of a `datetimeoffset` (whose date and time are UTC).
fn datetime(epoch_year: i32, days: i64, nanos: i64, offset: Option<i16>) -> Value {
    let Ok(epoch) = time::Date::from_calendar_date(epoch_year, time::Month::January, 1) else {
        return Value::Null;
    };
    let date = (epoch.to_julian_day() as i64 + days)
        .try_into()
        .ok()
        .and_then(|julian| time::Date::from_julian_day(julian).ok());
    let Some(date) = date else {
        return Value::Null;
    };
    let text = format!(
        "{:04}-{:02}-{:02}T{}",
        date.year(),
        u8::from(date.month()),
        date.day(),
        clock(nanos)
    );
    // tiberius gives a datetimeoffset's date and time in UTC.
    Value::DateTime(match offset {
        Some(_) => format!("{text}Z"),
        None => text,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_times_read_as_iso_text() {
        assert_eq!(
            datetime(1, 738_899, time_nanos(378_001_234_567, 7), None),
            Value::DateTime("2024-01-15T10:30:00.1234567".into())
        );
        assert_eq!(
            datetime(1900, 45_304, 0, Some(120)),
            Value::DateTime("2024-01-15T00:00:00.0000000Z".into())
        );
        assert_eq!(clock(time_nanos(3, 0)), "00:00:03.0000000");
    }
}
