//! Process startup: parses the command line and dispatches to the requested
//! mode.
//!
//! # Launching the TUI
//!
//! Before the terminal is touched (so failures print plainly on stderr):
//!
//! 1. the pane endpoints: a local path is made absolute against the working
//!    directory (a leading `~` is the home directory) and must exist; a
//!    remote path must be absolute (`host:` alone means the start
//!    directory);
//! 2. the scpto settings (`$SCPTO_CONFIG`, ...), the sshto groups
//!    (`$SSHTO_CONFIG`, ...), the ssh host aliases (`hosts::discover`), the
//!    persisted state, and the partial-file records. A settings or groups
//!    file that cannot be used is replaced by the defaults; that, and a
//!    state or partials problem, is shown in the status bar instead of
//!    stopping scpto;
//! 3. the Tokio runtime, the sessions (`Sessions::new` prepares the private
//!    ControlPath directory), and the transfer engine.
//!
//! Then the terminal enters TUI mode, the background color and the image
//! protocol are queried (before any `EventStream` exists), the `App` is
//! built, and `ui::runtime::run` drives it until quit.
//!
//! Status bar priority at startup (one message): a settings or groups
//! problem first, else the ssh configuration warning `App::new` shows, else
//! a state or partials problem. Each shows the first problem and how many
//! more there are.
//!
//! The edit temp directory is `<ControlPath dir>/edit`: inside the
//! per-process directory `Sessions::new` created with mode 0700 under a
//! verified private base, so a crash leaves nothing behind that the next
//! start does not remove.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{bail, Context};

use crate::cli::{self, EndpointArg, Launch};
use crate::connections::{Connections, SshClient};
use crate::groups::{self, Groups};
use crate::hosts;
use crate::session::{self, Sessions};
use crate::settings::{self, ConfigPath, Settings};
use crate::state::{self, PartialsFile, PersistedState};
use crate::term::{self, TerminalGuard};
use crate::ui::app::core::StatusMessage;
use crate::ui::app::App;
use crate::ui::effects::session::ProductionBackends;
use crate::ui::effects::{transfer, Ctx, Tasks};
use crate::ui::runtime;
use crate::vfs::Endpoint;

/// Environment variable naming the sshto group settings file.
const SSHTO_CONFIG_ENV: &str = "SSHTO_CONFIG";

/// Runs scpto with the arguments after the program name and returns the
/// exit code.
pub fn main<I>(args: I) -> anyhow::Result<ExitCode>
where
    I: IntoIterator<Item = OsString>,
{
    match cli::parse(args) {
        Ok(cli::Invocation::Version) => {
            writeln!(std::io::stdout(), "{}", cli::version_line())?;
            Ok(ExitCode::SUCCESS)
        }
        Ok(cli::Invocation::Help) => {
            write!(std::io::stdout(), "{}", cli::USAGE)?;
            Ok(ExitCode::SUCCESS)
        }
        Ok(cli::Invocation::Launch(launch)) => {
            launch_tui(launch)?;
            Ok(ExitCode::SUCCESS)
        }
        Err(error) => {
            write!(std::io::stderr(), "scpto: {error}\n\n{}", cli::USAGE)?;
            Ok(ExitCode::from(cli::USAGE_EXIT_CODE))
        }
    }
}

/// A pane endpoint and its optional absolute start directory.
type PaneStart = (Endpoint, Option<PathBuf>);

/// Everything loaded before the terminal is touched.
struct Loaded {
    settings: Settings,
    groups: Groups,
    hosts: hosts::HostList,
    state_dir: Option<PathBuf>,
    persisted: PersistedState,
    partials: PartialsFile,
    /// Settings and groups problems (shown first).
    config_warnings: Vec<String>,
    /// State and partials problems.
    state_warnings: Vec<String>,
}

fn launch_tui(launch: Launch) -> anyhow::Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .context("HOME must be set to an absolute path")?;
    let cwd = std::env::current_dir().context("cannot read the working directory")?;
    let left = match launch.left {
        Some(arg) => pane_start(arg, &cwd, &home)?,
        None => (Endpoint::Local, Some(cwd.clone())),
    };
    let right = launch
        .right
        .map(|arg| pane_start(arg, &cwd, &home))
        .transpose()?;
    let client = SshClient::default();
    let loaded = load(&client, &home)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;
    runtime.block_on(run(client, loaded, left, right))
}

/// One command-line endpoint as a pane start (see the module doc).
fn pane_start(arg: EndpointArg, cwd: &Path, home: &Path) -> anyhow::Result<PaneStart> {
    match arg {
        EndpointArg::Local(path) => {
            let path = expand_home(&path, home);
            let absolute = cwd.join(&path);
            let resolved = std::fs::canonicalize(&absolute)
                .with_context(|| format!("{}", absolute.display()))?;
            if !resolved.is_dir() {
                bail!("{}: not a directory", resolved.display());
            }
            Ok((Endpoint::Local, Some(resolved)))
        }
        EndpointArg::Remote { host, path } => {
            if let Some(path) = &path {
                if !path.is_absolute() {
                    bail!(
                        "{host}:{}: remote paths must be absolute (use {host}: for the start directory)",
                        path.display()
                    );
                }
            }
            Ok((Endpoint::Remote { host, sudo: false }, path))
        }
    }
}

/// `~` and `~/rest` relative to `home`; other paths unchanged.
fn expand_home(path: &Path, home: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// Loads configuration and state (blocking file I/O, before the TUI).
fn load(client: &SshClient, home: &Path) -> anyhow::Result<Loaded> {
    let xdg_config = std::env::var_os("XDG_CONFIG_HOME");
    let mut config_warnings = Vec::new();
    let settings_path = ConfigPath::resolve(
        std::env::var_os(settings::CONFIG_ENV).as_deref(),
        xdg_config.as_deref(),
        home,
        "scpto",
    );
    let settings = settings::load(&settings_path).unwrap_or_else(|error| {
        config_warnings.push(format!("{error}; using default settings"));
        Settings::default()
    });
    let groups_path = ConfigPath::resolve(
        std::env::var_os(SSHTO_CONFIG_ENV).as_deref(),
        xdg_config.as_deref(),
        home,
        "sshto",
    );
    let groups = groups::load(&groups_path).unwrap_or_else(|error| {
        config_warnings.push(format!("{error}; hosts are not grouped"));
        Groups::default()
    });
    let hosts = hosts::discover(client, home);

    let mut state_warnings = Vec::new();
    let state_dir = state::state_dir(std::env::var_os("XDG_STATE_HOME").as_deref(), Some(home));
    let persisted = match &state_dir {
        Some(dir) => {
            let (persisted, warning) = state::load_state(dir);
            state_warnings.extend(warning.map(|error| error.to_string()));
            persisted
        }
        None => {
            state_warnings
                .push("No state directory: directories and bookmarks are not saved".into());
            PersistedState::default()
        }
    };
    let partials = match PartialsFile::open(state_dir.clone()) {
        Ok(partials) => partials,
        Err(error) => {
            state_warnings.push(format!("{error}; interrupted transfers cannot resume"));
            // Records of this run stay in memory; the unreadable file is
            // left as it is for the user to inspect.
            PartialsFile::open(None).context("cannot hold partial-file records")?
        }
    };
    Ok(Loaded {
        settings,
        groups,
        hosts,
        state_dir,
        persisted,
        partials,
        config_warnings,
        state_warnings,
    })
}

async fn run(
    client: SshClient,
    loaded: Loaded,
    left: PaneStart,
    right: Option<PaneStart>,
) -> anyhow::Result<()> {
    let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
    let sessions = Arc::new(
        Sessions::new(
            client.clone(),
            &loaded.settings,
            events.clone(),
            Arc::new(ProductionBackends),
        )
        .context("cannot prepare the ssh session directory")?,
    );
    let connections: Arc<dyn Connections> = Arc::clone(&sessions) as Arc<dyn Connections>;
    let engine = transfer::start_engine(
        &loaded.settings,
        Arc::clone(&connections),
        Arc::new(loaded.partials),
        events.clone(),
    );
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let edit_temp = session::control_dir(uid, std::process::id()).join("edit");

    term::install_panic_hook();
    let mut guard = TerminalGuard::enter().context("cannot set up the terminal")?;
    let background = term::query_background();
    let image_picker = term::query_image_picker();

    let (mut app, effects) = App::new(
        loaded.settings.clone(),
        loaded.hosts,
        loaded.groups,
        loaded.persisted.clone(),
        background,
        left,
        right,
    );
    if let Some(text) = summarize(&loaded.config_warnings) {
        app.core.set_status(StatusMessage::error(text));
    } else if app.core.status().is_none() {
        if let Some(text) = summarize(&loaded.state_warnings) {
            app.core.set_status(StatusMessage::error(text));
        }
    }

    let ctx = Ctx {
        sessions,
        connections,
        events,
        settings: Arc::new(loaded.settings),
        ssh: client,
        state_dir: loaded.state_dir,
        persisted: Arc::new(tokio::sync::Mutex::new(loaded.persisted)),
        engine,
        image_picker: Arc::new(image_picker),
        edit_temp,
        requests: Arc::new(Tasks::new()),
        timers: Arc::new(Tasks::new()),
        syncs: Arc::new(Tasks::new()),
    };
    let result = runtime::run(&mut guard, app, effects, ctx, receiver).await;
    drop(guard);
    result.context("terminal input or output failed")
}

/// The first message and how many more there are; `None` when empty.
fn summarize(messages: &[String]) -> Option<String> {
    let first = messages.first()?;
    Some(match messages.len() - 1 {
        0 => first.clone(),
        more => format!("{first} (and {more} more)"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_paths_become_absolute_and_must_be_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonical");
        std::fs::create_dir(root.join("sub")).expect("mkdir");
        std::fs::write(root.join("file"), b"x").expect("write");

        let start = pane_start(EndpointArg::Local("sub".into()), &root, Path::new("/h"))
            .expect("relative directory");
        assert_eq!(start, (Endpoint::Local, Some(root.join("sub"))));
        let start = pane_start(EndpointArg::Local("~/sub".into()), Path::new("/"), &root)
            .expect("home directory");
        assert_eq!(start.1, Some(root.join("sub")));

        let error = pane_start(EndpointArg::Local("file".into()), &root, &root)
            .expect_err("a file is not a pane directory");
        assert!(error.to_string().ends_with("not a directory"));
        assert!(pane_start(EndpointArg::Local("absent".into()), &root, &root).is_err());
    }

    #[test]
    fn remote_paths_must_be_absolute() {
        let remote = |path: Option<&str>| EndpointArg::Remote {
            host: "web".into(),
            path: path.map(PathBuf::from),
        };
        let web = Endpoint::Remote {
            host: "web".into(),
            sudo: false,
        };
        let cwd = Path::new("/");
        assert_eq!(
            pane_start(remote(Some("/srv")), cwd, cwd).expect("absolute"),
            (web.clone(), Some(PathBuf::from("/srv")))
        );
        assert_eq!(
            pane_start(remote(None), cwd, cwd).expect("start directory"),
            (web, None)
        );
        let error = pane_start(remote(Some("docs")), cwd, cwd).expect_err("relative");
        assert!(error.to_string().contains("remote paths must be absolute"));
    }

    #[test]
    fn home_expansion_only_touches_a_leading_tilde() {
        let home = Path::new("/home/me");
        assert_eq!(expand_home(Path::new("~"), home), PathBuf::from("/home/me"));
        assert_eq!(
            expand_home(Path::new("~/a/b"), home),
            PathBuf::from("/home/me/a/b")
        );
        assert_eq!(expand_home(Path::new("a/~"), home), PathBuf::from("a/~"));
        assert_eq!(expand_home(Path::new("~x"), home), PathBuf::from("~x"));
    }

    #[test]
    fn summaries_count_the_remaining_messages() {
        assert_eq!(summarize(&[]), None);
        assert_eq!(summarize(&["a".into()]), Some("a".into()));
        assert_eq!(
            summarize(&["a".into(), "b".into(), "c".into()]),
            Some("a (and 2 more)".into())
        );
    }
}
