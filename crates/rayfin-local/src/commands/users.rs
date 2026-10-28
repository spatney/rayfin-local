//! `rayfin-local users`: the app's local accounts, the ones the sign-in page and the SDK's
//! sign-up and sign-in use, in `.rayfin-local/db.sqlite`. Works while `serve` is running.
//!
//! Also the terminal helpers `secret` shares: tables, hidden input and confirmations.

use std::fmt::Write as _;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;

use anyhow::Context as _;
use clap::Subcommand;
use rl_auth::{AuthError, AuthService, AuthSettings, User};
use rl_db::Db;
use rl_project::{LocalState, Project};

use crate::ui::{self, Color, Painter};

#[derive(Debug, Subcommand)]
pub enum UsersCommand {
    /// List the users: id, username, email, when they were added and how they sign in.
    List,
    /// Add a user who signs in with a username (usually an email address) and a password.
    Add {
        /// What the user signs in with, usually an email address.
        username: String,
        /// The password (default: asked for twice, hidden). Required when stdin is not a
        /// terminal.
        #[arg(long, value_name = "PASSWORD", allow_hyphen_values = true)]
        password: Option<String>,
    },
    /// Delete a user and sign out their sessions.
    Delete {
        /// The user's username or id.
        user: String,
        /// Do not ask for confirmation.
        #[arg(short, long)]
        yes: bool,
    },
    /// Set a user's password and sign out their sessions.
    ResetPassword {
        /// The user's username or id.
        user: String,
        /// The new password (default: asked for twice, hidden). Required when stdin is not a
        /// terminal.
        #[arg(long, value_name = "PASSWORD", allow_hyphen_values = true)]
        password: Option<String>,
    },
}

pub async fn run(dir: &Path, command: UsersCommand) -> anyhow::Result<ExitCode> {
    let project = Project::discover(dir)?;
    let state = LocalState::load_or_create(&project)?;
    let auth = open_auth(&state).await?;
    match command {
        UsersCommand::List => list(&auth).await?,
        UsersCommand::Add { username, password } => {
            add(&project, &auth, &username, password).await?;
        }
        UsersCommand::Delete { user, yes } => delete(&project, &auth, &user, yes).await?,
        UsersCommand::ResetPassword { user, password } => {
            reset_password(&project, &auth, &user, password).await?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The auth service over the project database. Only its users are used, so the token settings
/// of `rayfin.yml` don't matter here.
async fn open_auth(state: &LocalState) -> anyhow::Result<AuthService> {
    let path = state.db_path();
    let db = Db::open(&path).with_context(|| {
        format!(
            "Could not open the database {}. Close any program that has it open (other than \
             `rayfin-local serve`), or start over with `rayfin-local reset`.",
            path.display()
        )
    })?;
    AuthService::new(
        db,
        state.keys_dir(),
        AuthSettings::new(state.data().app_id.clone()),
    )
    .await
    .context("Could not open the app's local users")
}

async fn list(auth: &AuthService) -> anyhow::Result<()> {
    let users = auth
        .list_users()
        .await
        .context("Could not read the users")?;
    if users.is_empty() {
        ui::status(
            "No users yet. Add one with `rayfin-local users add <username>`, or sign up in the \
             app.",
        );
    } else {
        print!("{}", render_users(&users, Painter::stdout()));
    }
    Ok(())
}

async fn add(
    project: &Project,
    auth: &AuthService,
    username: &str,
    password: Option<String>,
) -> anyhow::Result<()> {
    let username = username.trim();
    if username.is_empty() {
        anyhow::bail!("The username is empty.");
    }
    // Before asking for a password that could not be used (`find_user` also matches ids).
    if let Some(existing) = find_user(auth, username)
        .await?
        .filter(|existing| existing.id != username)
    {
        return Err(username_taken(&existing.username));
    }
    let password = match password {
        Some(password) => password,
        None => ask_new_password(&format!("Password for {username}: "))?,
    };
    let user = auth
        .create_user(username, &password)
        .await
        .map_err(|err| match err {
            AuthError::UsernameTaken => username_taken(username),
            err => anyhow::Error::new(err).context(format!("Could not add the user {username}")),
        })?;
    ui::step(format_args!(
        "Added user {} (id {})",
        user.username, user.id
    ));
    warn_if_auth_is_off(project);
    Ok(())
}

async fn delete(
    project: &Project,
    auth: &AuthService,
    user: &str,
    yes: bool,
) -> anyhow::Result<()> {
    let user = existing_user(auth, user).await?;
    if !yes
        && !confirm(&format!(
            "Delete user {} (id {}) and sign out their sessions? [y/N] ",
            user.username, user.id
        ))?
    {
        ui::status("Cancelled; nothing was deleted.");
        return Ok(());
    }
    let user = auth
        .delete_user(&user.id)
        .await
        .with_context(|| format!("Could not delete the user {}", user.username))?;
    ui::step(format_args!(
        "Deleted user {} (id {}) and signed out their sessions",
        user.username, user.id
    ));
    note_running_server(project);
    Ok(())
}

async fn reset_password(
    project: &Project,
    auth: &AuthService,
    user: &str,
    password: Option<String>,
) -> anyhow::Result<()> {
    let user = existing_user(auth, user).await?;
    let password = match password {
        Some(password) => password,
        None => ask_new_password(&format!("New password for {}: ", user.username))?,
    };
    let user = auth
        .set_password(&user.id, &password)
        .await
        .with_context(|| format!("Could not set the password of {}", user.username))?;
    ui::step(format_args!(
        "Set the password of {} (id {}) and signed out their sessions",
        user.username, user.id
    ));
    warn_if_auth_is_off(project);
    note_running_server(project);
    Ok(())
}

async fn find_user(auth: &AuthService, id_or_username: &str) -> anyhow::Result<Option<User>> {
    auth.find_user(id_or_username)
        .await
        .context("Could not read the users")
}

async fn existing_user(auth: &AuthService, id_or_username: &str) -> anyhow::Result<User> {
    find_user(auth, id_or_username).await?.ok_or_else(|| {
        anyhow::anyhow!(
            "There is no user with the username or id \"{}\". Run `rayfin-local users list` to \
             see the users.",
            id_or_username.trim()
        )
    })
}

fn username_taken(username: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "A user {username} already exists. To change their password, run `rayfin-local users \
         reset-password {username}`."
    )
}

fn warn_if_auth_is_off(project: &Project) {
    if !project.config().services.auth.enabled {
        ui::warn(
            "services.auth is not enabled in rayfin/rayfin.yml, so the app has no sign-in to use \
             this password with.",
        );
    }
}

/// Sessions are signed out in the database, but a running `serve` only learns of signed-out
/// sessions it didn't sign out itself when it starts.
fn note_running_server(project: &Project) {
    ui::status(format_args!(
        "If `rayfin-local serve` is running, it accepts their current access tokens until they \
         expire (at most {} minutes); restart it to sign them out now.",
        project
            .config()
            .services
            .auth
            .access_token_lifetime_minutes()
    ));
}

fn render_users(users: &[User], painter: Painter) -> String {
    let mut rows = vec![["ID", "USERNAME", "EMAIL", "CREATED (UTC)", "SIGN-IN"].map(str::to_owned)];
    rows.extend(users.iter().map(|user| {
        let created = user.created_at;
        [
            user.id.clone(),
            user.username.clone(),
            user.email.clone().unwrap_or_default(),
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}",
                created.year(),
                u8::from(created.month()),
                created.day(),
                created.hour(),
                created.minute()
            ),
            sign_in(user).to_owned(),
        ]
    }));
    render_table(&rows, painter)
}

fn sign_in(user: &User) -> &'static str {
    match (user.has_password, user.entra_oid.is_some()) {
        (true, true) => "password, Entra",
        (true, false) => "password",
        (false, true) => "Entra",
        (false, false) => "none",
    }
}

/// Rows of cells in columns as wide as their widest cell; the first row is the (dimmed) header.
pub(super) fn render_table<const N: usize>(rows: &[[String; N]], painter: Painter) -> String {
    let mut widths = [0; N];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for (index, row) in rows.iter().enumerate() {
        let mut line = String::new();
        for (cell, width) in row.iter().zip(widths) {
            let _ = write!(line, "{cell:<width$}  ");
        }
        let line = line.trim_end();
        if index == 0 {
            let _ = writeln!(out, "{}", painter.paint(Color::Dim, line));
        } else {
            let _ = writeln!(out, "{line}");
        }
    }
    out
}

/// Asks twice for a new password at the terminal, without echo.
fn ask_new_password(prompt: &str) -> anyhow::Result<String> {
    if !io::stdin().is_terminal() {
        anyhow::bail!(
            "No password given: pass `--password <PASSWORD>` (stdin is not a terminal, so it \
             can't be asked for)."
        );
    }
    let password = read_hidden(prompt)?;
    if password.trim().is_empty() {
        anyhow::bail!("The password is empty; nothing was changed.");
    }
    if read_hidden("Repeat the password: ")? != password {
        anyhow::bail!("The passwords don't match; nothing was changed.");
    }
    Ok(password)
}

/// Asks on stderr and reads one line typed at the terminal without showing it.
pub(super) fn read_hidden(prompt: &str) -> anyhow::Result<String> {
    prompt_on_stderr(prompt)?;
    let mut line = String::new();
    {
        let _echo_off = EchoOff::new();
        io::stdin()
            .lock()
            .read_line(&mut line)
            .context("Could not read from the terminal")?;
    }
    // The Enter key wasn't echoed either.
    eprintln!();
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

/// Asks on stderr and reads the answer from stdin; anything but `y`/`yes` (or end of input) is no.
fn confirm(question: &str) -> anyhow::Result<bool> {
    prompt_on_stderr(question)?;
    let mut answer = String::new();
    let stdin = io::stdin();
    stdin.lock().read_line(&mut answer)?;
    if !stdin.is_terminal() {
        // A piped answer is not echoed: end the question's line.
        eprintln!();
    }
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn prompt_on_stderr(prompt: &str) -> io::Result<()> {
    let mut stderr = io::stderr().lock();
    stderr.write_all(prompt.as_bytes())?;
    stderr.flush()
}

/// Turns off the console's echo of typed input until dropped.
#[cfg(windows)]
struct EchoOff {
    mode: windows_sys::Win32::System::Console::CONSOLE_MODE,
}

#[cfg(windows)]
impl EchoOff {
    fn new() -> Option<Self> {
        use windows_sys::Win32::System::Console::{
            CONSOLE_MODE, ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE,
            SetConsoleMode,
        };
        // SAFETY: console-mode calls on this process's own stdin handle, with a valid out pointer.
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            let mut mode: CONSOLE_MODE = 0;
            if GetConsoleMode(handle, &mut mode) == 0
                || SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT) == 0
            {
                return None;
            }
            Some(Self { mode })
        }
    }
}

#[cfg(windows)]
impl Drop for EchoOff {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode};
        // SAFETY: restores the mode `new` read, on this process's own stdin handle.
        unsafe {
            SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), self.mode);
        }
    }
}

/// Turns off the terminal's echo of typed input until dropped (with `stty`, which every Unix
/// has, rather than termios bindings).
#[cfg(unix)]
struct EchoOff;

#[cfg(unix)]
impl EchoOff {
    fn new() -> Option<Self> {
        stty("-echo").then_some(Self)
    }
}

#[cfg(unix)]
impl Drop for EchoOff {
    fn drop(&mut self) {
        stty("echo");
    }
}

#[cfg(unix)]
fn stty(setting: &str) -> bool {
    use std::process::{Command, Stdio};
    Command::new("stty")
        .arg(setting)
        .stdin(Stdio::inherit())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(any(windows, unix)))]
struct EchoOff;

#[cfg(not(any(windows, unix)))]
impl EchoOff {
    fn new() -> Option<Self> {
        None
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        users: UsersCommand,
    }

    fn parse(args: &[&str]) -> UsersCommand {
        Cli::try_parse_from(std::iter::once("users").chain(args.iter().copied()))
            .unwrap()
            .users
    }

    #[test]
    fn passwords_may_start_with_a_hyphen() {
        assert!(matches!(
            parse(&["add", "ana@contoso.com", "--password", "-pass-"]),
            UsersCommand::Add { username, password: Some(password) }
                if username == "ana@contoso.com" && password == "-pass-"
        ));
        assert!(matches!(
            parse(&["reset-password", "ana", "--password", "--pass"]),
            UsersCommand::ResetPassword { user, password: Some(password) }
                if user == "ana" && password == "--pass"
        ));
    }

    const RAYFIN_YML: &str = "id: app\nname: App\nversion: 1.0.0\nservices:\n  auth:\n    \
                              enabled: true\n";

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("rayfin")).unwrap();
        std::fs::write(dir.path().join("rayfin").join("rayfin.yml"), RAYFIN_YML).unwrap();
        dir
    }

    /// The project's users, as a running `serve` sees them: through another connection.
    async fn open(dir: &Path) -> AuthService {
        let project = Project::discover(dir).unwrap();
        let state = LocalState::load_or_create(&project).unwrap();
        open_auth(&state).await.unwrap()
    }

    fn add_command(username: &str, password: &str) -> UsersCommand {
        UsersCommand::Add {
            username: username.to_owned(),
            password: Some(password.to_owned()),
        }
    }

    #[tokio::test]
    async fn adds_lists_resets_and_deletes_users() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = project();
        let root = dir.path();
        run(root, add_command(" Ana@Contoso.com ", "first-pass"))
            .await
            .unwrap();
        let auth = open(root).await;

        // Usernames are unique case-insensitively.
        let err = run(root, add_command("ana@contoso.com", "other"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err:#}");
        let err = run(root, add_command("bob", " ")).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("Password is required"),
            "{err:#}"
        );

        auth.find_or_create_external_user("oid-1", Some("eve@contoso.com"), Some("Eve"))
            .await
            .unwrap();
        let users = auth.list_users().await.unwrap();
        assert_eq!(users.len(), 2);
        let table = render_users(&users, Painter::stdout());
        let rows: Vec<Vec<&str>> = table
            .lines()
            .skip(1)
            .map(|row| row.split_whitespace().collect())
            .collect();
        assert_eq!(
            rows[0][..3],
            [users[0].id.as_str(), "Ana@Contoso.com", "Ana@Contoso.com"],
            "{table}"
        );
        assert_eq!(rows[0].last(), Some(&"password"), "{table}");
        assert_eq!(rows[1].last(), Some(&"Entra"), "{table}");

        run(
            root,
            UsersCommand::ResetPassword {
                user: "ANA@contoso.com".to_owned(),
                password: Some("second-pass".to_owned()),
            },
        )
        .await
        .unwrap();
        auth.verify_password("ana@contoso.com", "second-pass")
            .await
            .unwrap();
        assert!(
            auth.verify_password("ana@contoso.com", "first-pass")
                .await
                .is_err()
        );

        let delete = |user: &str| UsersCommand::Delete {
            user: user.to_owned(),
            yes: true,
        };
        run(root, delete(&users[0].id)).await.unwrap();
        let remaining = auth.list_users().await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, users[1].id);
        let err = run(root, delete("ana@contoso.com")).await.unwrap_err();
        assert!(err.to_string().contains("There is no user"), "{err:#}");
    }
}
