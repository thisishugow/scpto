//! Executes `Effect::OpenShell` and `Effect::RunVpn` (with the runtime,
//! which suspends the TUI), `Effect::Reveal`, and the Finder-drop
//! existence check `Effect::CheckDroppedPaths`.
//!
//! # Behavior
//!
//! - `shell` (async): local → `$SHELL` (fallback `/bin/sh`) with
//!   `-l` in the directory; remote → `Connections::ssh_command(endpoint,
//!   true)`, then `--`, the host alias, and one remote command word
//!   `cd <dir> && exec "$SHELL" -l` with `<dir>` quoted by
//!   `connections::quote` (so `$SHELL` expands remotely). Returns the
//!   [`Foreground`] with `AfterForeground::Shell`; a failure sends
//!   `ShellExited { result: Err }` and returns `None`. `shell_finished`
//!   sends `ShellExited { result: Ok(()) }` whatever the shell's exit
//!   status, or `Err(Error::CommandFailed { status: NotStarted, .. })` when
//!   it could not be started.
//! - `vpn`: `/bin/sh -c <command>` as `AfterForeground::Vpn(endpoint)`.
//!   `vpn_finished` sends `VpnFinished { result }`: `Ok` on exit 0, else
//!   `Error::CommandFailed { program: "VPN command", status, .. }`.
//! - `reveal`: a local file → `open -R <path>` on macOS, `xdg-open
//!   <parent directory>` on Linux (background child, `process_group(0)`,
//!   stdin null, `kill_on_drop`); a remote file is first downloaded over
//!   the Browse channel to `~/Downloads` (a taken name becomes `name
//!   (1).ext`, the first free number), then revealed. Answered with
//!   `Revealed`.
//! - `check_dropped_paths`: every path must be absolute and exist
//!   (`tokio::fs::symlink_metadata`); answered with `DroppedPathsChecked`.
//!
//! # Public API
//!
//! ```text
//! pub async fn shell(ctx: Ctx, location: Location) -> Option<Foreground>;
//! pub fn shell_finished(ctx: &Ctx, status: std::io::Result<std::process::ExitStatus>);
//! pub fn vpn(endpoint: Endpoint, command: String) -> Foreground;
//! pub fn vpn_finished(ctx: &Ctx, endpoint: Endpoint, status: std::io::Result<std::process::ExitStatus>);
//! pub fn reveal(ctx: &Ctx, location: Location);
//! pub fn check_dropped_paths(ctx: &Ctx, text: String, paths: Vec<PathBuf>);
//! ```
//!
//! [`Foreground`]: super::Foreground

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

use crate::connections::{quote, Connections, FsChannel};
use crate::error::{Error, ExitStatus, FsError};
use crate::transfer::stream::{copy_file, CopySpec};
use crate::ui::action::BgEvent;
use crate::vfs::{Endpoint, EntryKind, Fs, Location};

use super::{AfterForeground, Ctx, Foreground};

/// The shell of a local `t` when `$SHELL` is unset or empty.
const FALLBACK_SHELL: &str = "/bin/sh";

/// Program name of the shell in errors.
const SHELL_PROGRAM: &str = "shell";

/// Program name of the VPN command in errors.
const VPN_PROGRAM: &str = "VPN command";

/// The program that shows a file in the desktop's file manager.
#[cfg(target_os = "macos")]
const OPENER: &str = "open";

/// The program that shows a file in the desktop's file manager.
#[cfg(not(target_os = "macos"))]
const OPENER: &str = "xdg-open";

/// Builds the `t` shell for `location` (see the module documentation).
/// A failure is sent as `ShellExited { result: Err }` and gives `None`.
pub async fn shell(ctx: Ctx, location: Location) -> Option<Foreground> {
    let login_shell = std::env::var_os("SHELL");
    match shell_command(ctx.connections.as_ref(), &location, login_shell).await {
        Ok(command) => Some(Foreground {
            command,
            then: AfterForeground::Shell,
        }),
        Err(error) => {
            // A send error means the app has exited; nobody waits for the shell.
            drop(ctx.events.send(BgEvent::ShellExited { result: Err(error) }));
            None
        }
    }
}

/// Reports the `t` shell after it exited. Its exit status is the user's
/// business; only a shell that could not be started is an error.
pub fn shell_finished(ctx: &Ctx, status: std::io::Result<std::process::ExitStatus>) {
    let result = status
        .map(drop)
        .map_err(|error| not_started(SHELL_PROGRAM, &error));
    // A send error means the app has exited; the result is not needed.
    drop(ctx.events.send(BgEvent::ShellExited { result }));
}

/// The VPN command of a `vpn` rule, run by `/bin/sh -c` in the foreground.
pub fn vpn(endpoint: Endpoint, command: String) -> Foreground {
    let mut shell = Command::new("/bin/sh");
    shell.arg("-c").arg(command);
    Foreground {
        command: shell,
        then: AfterForeground::Vpn(endpoint),
    }
}

/// Reports the VPN command after it exited: success on exit 0.
pub fn vpn_finished(
    ctx: &Ctx,
    endpoint: Endpoint,
    status: std::io::Result<std::process::ExitStatus>,
) {
    let result = match status {
        Ok(status) if status.success() => Ok(()),
        // The command's stderr went to the terminal, where the user saw it.
        Ok(status) => Err(Error::CommandFailed {
            program: VPN_PROGRAM.into(),
            status: status.into(),
            stderr: String::new(),
        }),
        Err(error) => Err(not_started(VPN_PROGRAM, &error)),
    };
    // A send error means the app has exited; nothing is left to reconnect.
    drop(ctx.events.send(BgEvent::VpnFinished { endpoint, result }));
}

/// Shows `location` in the file manager, downloading a remote file to
/// `~/Downloads` first; answers with `BgEvent::Revealed`.
pub fn reveal(ctx: &Ctx, location: Location) {
    let connections = ctx.connections.clone();
    let events = ctx.events.clone();
    tokio::spawn(async move {
        let result = reveal_location(connections.as_ref(), &location).await;
        // A send error means the app has exited; the result is not needed.
        drop(events.send(BgEvent::Revealed { location, result }));
    });
}

/// Checks a Finder drop: answers `DroppedPathsChecked` with `all_exist`
/// true only when there is at least one path and every path is absolute
/// and exists locally (a dangling symlink exists).
pub fn check_dropped_paths(ctx: &Ctx, text: String, paths: Vec<PathBuf>) {
    let events = ctx.events.clone();
    tokio::spawn(async move {
        let all_exist = all_exist(&paths).await;
        // A send error means the app has exited; the drop is moot.
        drop(events.send(BgEvent::DroppedPathsChecked {
            text,
            paths,
            all_exist,
        }));
    });
}

/// The foreground command of the `t` shell. `login_shell` is `$SHELL`.
async fn shell_command(
    connections: &dyn Connections,
    location: &Location,
    login_shell: Option<OsString>,
) -> Result<Command, Error> {
    let host = match &location.endpoint {
        Endpoint::Local => {
            let program = login_shell
                .filter(|shell| !shell.is_empty())
                .unwrap_or_else(|| FALLBACK_SHELL.into());
            let mut command = Command::new(program);
            command.arg("-l").current_dir(&location.path);
            return Ok(command);
        }
        Endpoint::Remote { host, .. } => host,
    };
    // Quoted before the master is checked, so a path that cannot be passed
    // fails without touching the connection.
    let dir = location.path.to_str().ok_or_else(|| FsError::Unsupported {
        operation: "Opening a shell".into(),
        reason: format!(
            "the remote path {} is not valid UTF-8",
            location.path.display()
        ),
    })?;
    // `$SHELL` is in double quotes inside the one remote word, so the
    // remote shell expands it to the login user's shell.
    let remote_command = format!("cd {} && exec \"$SHELL\" -l", quote(dir)?);
    let argv = connections.ssh_command(&location.endpoint, true).await?;
    let (program, options) = argv.split_first().ok_or_else(|| {
        Error::Connect(crate::error::ConnectError::SshUnavailable {
            message: "the session gave no ssh program to run".into(),
        })
    })?;
    let mut command = Command::new(program);
    command
        .args(options)
        .args(["--", host.as_str(), remote_command.as_str()]);
    Ok(command)
}

/// `Error::CommandFailed` for a program that could not be started.
fn not_started(program: &str, error: &std::io::Error) -> Error {
    Error::CommandFailed {
        program: program.into(),
        status: ExitStatus::NotStarted,
        stderr: error.to_string(),
    }
}

async fn reveal_location(connections: &dyn Connections, location: &Location) -> Result<(), Error> {
    if location.endpoint.is_local() {
        return reveal_local(&location.path).await;
    }
    let local = connections.fs(&Endpoint::Local, FsChannel::Browse).await?;
    let downloads = local.home().await?.join("Downloads");
    let file = download(connections, local.as_ref(), location, &downloads).await?;
    reveal_local(&file).await
}

/// Downloads the remote file `location` over its browsing channel into the
/// existing directory `downloads` on `local`, under its own name or the
/// first free `name (N).ext`. Returns the downloaded file.
async fn download(
    connections: &dyn Connections,
    local: &dyn Fs,
    location: &Location,
    downloads: &Path,
) -> Result<PathBuf, Error> {
    let name = location
        .path
        .file_name()
        .ok_or_else(|| FsError::IsADirectory {
            path: location.path.clone(),
        })?;
    let remote = connections
        .fs(&location.endpoint, FsChannel::Browse)
        .await?;
    let entry = remote.stat(&location.path).await?;
    match entry.kind {
        EntryKind::File => {}
        EntryKind::Dir => {
            return Err(FsError::IsADirectory {
                path: location.path.clone(),
            }
            .into())
        }
        // `stat` follows symlinks, so only special files remain.
        EntryKind::Symlink { .. } | EntryKind::Special(_) => {
            return Err(FsError::Unsupported {
                operation: "Revealing".into(),
                reason: format!("{} is not a regular file", location.path.display()),
            }
            .into())
        }
    }
    // A missing ~/Downloads is reported, not created.
    let downloads_entry = local.stat(downloads).await?;
    if downloads_entry.kind != EntryKind::Dir {
        return Err(FsError::NotADirectory {
            path: downloads.to_path_buf(),
        }
        .into());
    }
    let destination = free_path(local, downloads, name).await?;
    copy_file(
        CopySpec {
            source: remote.as_ref(),
            source_path: &location.path,
            destination: local,
            destination_path: &destination,
            // The size only bounds a resume and the progress of a server-side
            // copy; this copy starts at 0 and crosses endpoints, so an
            // unreported size changes nothing.
            size: entry.size.unwrap_or(0),
            mtime: entry.mtime,
            mode: entry.mode,
            resume_from: 0,
        },
        &|_| {},
    )
    .await?;
    Ok(destination)
}

/// The first path in `dir` named `name`, `name (1)`, `name (2)`, ... that
/// does not exist on `fs` (a dangling symlink exists).
async fn free_path(fs: &dyn Fs, dir: &Path, name: &OsStr) -> Result<PathBuf, FsError> {
    let mut number = 0u64;
    loop {
        let candidate = dir.join(numbered_name(name, number));
        match fs.lstat(&candidate).await {
            Ok(_) => number += 1,
            Err(FsError::NotFound { .. }) => return Ok(candidate),
            Err(error) => return Err(error),
        }
    }
}

/// `name` for 0, else `stem (number).ext`: `a.txt` → `a (1).txt`, `a` →
/// `a (1)`, `.bashrc` → `.bashrc (1)`, `a.tar.gz` → `a.tar (1).gz`.
fn numbered_name(name: &OsStr, number: u64) -> OsString {
    if number == 0 {
        return name.to_os_string();
    }
    let path = Path::new(name);
    let mut numbered = path.file_stem().unwrap_or(name).to_os_string();
    numbered.push(format!(" ({number})"));
    if let Some(extension) = path.extension() {
        numbered.push(".");
        numbered.push(extension);
    }
    numbered
}

/// Shows the local `path` with [`OPENER`], waiting for it to exit. The
/// child gets no terminal streams (they belong to the TUI) and its own
/// process group, so terminal signals never reach it.
async fn reveal_local(path: &Path) -> Result<(), Error> {
    tokio::fs::symlink_metadata(path)
        .await
        .map_err(|error| FsError::from_io(&error, path))?;
    let mut command = opener(path);
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true)
        .status()
        .await
        .map_err(|error| not_started(OPENER, &error))?;
    if status.success() {
        return Ok(());
    }
    Err(Error::CommandFailed {
        program: OPENER.into(),
        status: status.into(),
        stderr: opener_failure(status.code()),
    })
}

/// `open -R <path>`: Finder shows the file selected.
#[cfg(target_os = "macos")]
fn opener(path: &Path) -> Command {
    let mut command = Command::new(OPENER);
    command.arg("-R").arg(path);
    command
}

/// `xdg-open <parent directory>`: the file manager shows the directory
/// holding the file (`/` for `/`).
#[cfg(not(target_os = "macos"))]
fn opener(path: &Path) -> Command {
    let mut command = Command::new(OPENER);
    command.arg(path.parent().unwrap_or(path));
    command
}

/// What a failed `open` exit means. Its message went to the discarded
/// stderr; the existence check before it covers the common cause.
#[cfg(target_os = "macos")]
fn opener_failure(_code: Option<i32>) -> String {
    String::new()
}

/// What a failed `xdg-open` exit means (its documented exit codes).
#[cfg(not(target_os = "macos"))]
fn opener_failure(code: Option<i32>) -> String {
    match code {
        Some(1) => "invalid command line".into(),
        Some(2) => "the directory does not exist".into(),
        Some(3) => "no application to open directories was found".into(),
        Some(4) => "the file manager failed to open the directory".into(),
        _ => String::new(),
    }
}

async fn all_exist(paths: &[PathBuf]) -> bool {
    if paths.is_empty() {
        return false;
    }
    for path in paths {
        // Any failure to stat means the token is not an existing path, so
        // the drop is treated as text; the reason does not matter here.
        if !path.is_absolute() || tokio::fs::symlink_metadata(path).await.is_err() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::Arc;

    use tokio::sync::mpsc::UnboundedReceiver;

    use crate::transfer::stream::test_support::MemFs;
    use crate::ui::effects::fs::test_support::{ctx, it_config, sftp, MapConnections};
    use crate::vfs::local::LocalFs;
    use crate::vfs::SpecialKind;

    fn remote(host: &str) -> Endpoint {
        Endpoint::Remote {
            host: host.into(),
            sudo: false,
        }
    }

    fn args(command: &Command) -> Vec<OsString> {
        command
            .as_std()
            .get_args()
            .map(OsStr::to_os_string)
            .collect()
    }

    fn exit(code: i32) -> std::process::ExitStatus {
        std::process::ExitStatus::from_raw(code << 8)
    }

    async fn next(events: &mut UnboundedReceiver<BgEvent>) -> BgEvent {
        events.recv().await.expect("answered")
    }

    #[tokio::test]
    async fn a_local_shell_is_the_login_shell_in_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let location = Location {
            endpoint: Endpoint::Local,
            path: dir.path().to_path_buf(),
        };
        let connections = MapConnections::local();
        for (login_shell, program) in [
            (Some(OsString::from("/bin/zsh")), "/bin/zsh"),
            (Some(OsString::new()), FALLBACK_SHELL),
            (None, FALLBACK_SHELL),
        ] {
            let command = shell_command(&connections, &location, login_shell)
                .await
                .unwrap();
            assert_eq!(command.as_std().get_program(), program);
            assert_eq!(args(&command), ["-l"]);
            assert_eq!(command.as_std().get_current_dir(), Some(dir.path()));
        }
    }

    #[tokio::test]
    async fn a_remote_shell_goes_through_the_master_into_the_directory() {
        let mut connections = MapConnections::local();
        connections.ssh_argv = ["/usr/bin/ssh", "-o", "ControlPath=/tmp/c/%C", "-t"]
            .map(String::from)
            .to_vec();
        let location = Location {
            endpoint: Endpoint::Remote {
                host: "web".into(),
                sudo: true,
            },
            path: PathBuf::from("/srv/it's here"),
        };
        let command = shell_command(&connections, &location, None).await.unwrap();
        assert_eq!(command.as_std().get_program(), "/usr/bin/ssh");
        assert_eq!(
            args(&command),
            [
                "-o",
                "ControlPath=/tmp/c/%C",
                "-t",
                "--",
                "web",
                r#"cd '/srv/it'\''s here' && exec "$SHELL" -l"#,
            ]
        );
    }

    #[tokio::test]
    async fn the_remote_shell_word_enters_the_directory_and_expands_shell() {
        // The remote command word, run by a real POSIX shell with `$SHELL`
        // pointing at a script that prints where and how it was started,
        // must exec that script with `-l` in the quoted directory.
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("it's a $dir");
        std::fs::create_dir(&dir).unwrap();
        let login_shell = parent.path().join("login-shell");
        std::fs::write(&login_shell, "#!/bin/sh\npwd -P\necho \"$@\"\n").unwrap();
        std::fs::set_permissions(&login_shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut connections = MapConnections::local();
        connections.ssh_argv = vec!["ssh".into()];
        let location = Location {
            endpoint: remote("web"),
            path: dir.clone(),
        };
        let command = shell_command(&connections, &location, None).await.unwrap();
        let word = args(&command).pop().unwrap();
        let output = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&word)
            .env("SHELL", &login_shell)
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let printed = String::from_utf8(output.stdout).unwrap();
        let printed: Vec<&str> = printed.lines().collect();
        assert_eq!(printed.len(), 2, "{printed:?}");
        assert_eq!(Path::new(printed[0]), dir.canonicalize().unwrap());
        assert_eq!(printed[1], "-l");
    }

    #[tokio::test]
    async fn shell_failures_are_reported_and_give_no_command() {
        let temp = tempfile::tempdir().unwrap();
        let mut connections = MapConnections::local();
        connections.ssh_argv = vec!["ssh".into()];
        let (ctx, mut events) = ctx(Arc::new(connections), temp.path());

        let newline = Location {
            endpoint: remote("web"),
            path: PathBuf::from("/srv/a\nb"),
        };
        assert!(shell(ctx.clone(), newline).await.is_none());
        match next(&mut events).await {
            BgEvent::ShellExited {
                result: Err(Error::NewlineInPath { path }),
            } => assert_eq!(path, PathBuf::from("/srv/a\nb")),
            other => panic!("expected a newline error, got {other:?}"),
        }

        let non_utf8 = Location {
            endpoint: remote("web"),
            path: PathBuf::from(OsStr::from_bytes(b"/srv/\xff")),
        };
        assert!(shell(ctx.clone(), non_utf8).await.is_none());
        assert!(matches!(
            next(&mut events).await,
            BgEvent::ShellExited {
                result: Err(Error::Fs(FsError::Unsupported { .. }))
            }
        ));

        let local = Location {
            endpoint: Endpoint::Local,
            path: temp.path().to_path_buf(),
        };
        let foreground = shell(ctx, local).await.expect("a local shell");
        assert!(matches!(foreground.then, AfterForeground::Shell));
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_finished_shell_is_ok_whatever_its_exit_status() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, mut events) = ctx(Arc::new(MapConnections::local()), temp.path());
        shell_finished(&ctx, Ok(exit(1)));
        assert!(matches!(
            next(&mut events).await,
            BgEvent::ShellExited { result: Ok(()) }
        ));
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
        shell_finished(&ctx, Err(missing));
        match next(&mut events).await {
            BgEvent::ShellExited {
                result:
                    Err(Error::CommandFailed {
                        program,
                        status: ExitStatus::NotStarted,
                        ..
                    }),
            } => assert_eq!(program, SHELL_PROGRAM),
            other => panic!("expected a start failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_vpn_command_runs_in_sh_and_reports_its_exit() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, mut events) = ctx(Arc::new(MapConnections::local()), temp.path());
        for (script, expected) in [("exit 0", None), ("echo x | grep -q y", Some(1))] {
            let mut foreground = vpn(remote("web"), script.into());
            assert!(matches!(&foreground.then, AfterForeground::Vpn(ep) if *ep == remote("web")));
            let status = foreground.command.stdout(Stdio::null()).status().await;
            vpn_finished(&ctx, remote("web"), status);
            match (next(&mut events).await, expected) {
                (
                    BgEvent::VpnFinished {
                        endpoint,
                        result: Ok(()),
                    },
                    None,
                ) => {
                    assert_eq!(endpoint, remote("web"))
                }
                (
                    BgEvent::VpnFinished {
                        result:
                            Err(Error::CommandFailed {
                                program,
                                status: ExitStatus::Code(code),
                                ..
                            }),
                        ..
                    },
                    Some(expected),
                ) => {
                    assert_eq!(program, VPN_PROGRAM);
                    assert_eq!(code, expected);
                }
                (other, _) => panic!("{script}: unexpected {other:?}"),
            }
        }
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
        vpn_finished(&ctx, remote("web"), Err(missing));
        assert!(matches!(
            next(&mut events).await,
            BgEvent::VpnFinished {
                result: Err(Error::CommandFailed {
                    status: ExitStatus::NotStarted,
                    ..
                }),
                ..
            }
        ));
    }

    #[test]
    fn numbered_names_keep_the_extension() {
        let numbered = |name: &str, number| numbered_name(OsStr::new(name), number);
        assert_eq!(numbered("a.txt", 0), "a.txt");
        assert_eq!(numbered("a.txt", 1), "a (1).txt");
        assert_eq!(numbered("a", 2), "a (2)");
        assert_eq!(numbered(".bashrc", 1), ".bashrc (1)");
        assert_eq!(numbered("a.tar.gz", 3), "a.tar (3).gz");
    }

    fn serving_remote(memfs: &Arc<MemFs>) -> MapConnections {
        let mut connections = MapConnections::local();
        connections
            .fs
            .insert(remote("web"), Arc::clone(memfs) as Arc<dyn Fs>);
        connections
    }

    fn remote_location(path: &str) -> Location {
        Location {
            endpoint: remote("web"),
            path: PathBuf::from(path),
        }
    }

    #[tokio::test]
    async fn downloads_take_the_first_free_name() {
        let memfs = MemFs::new(remote("web"));
        memfs.add_dir("/srv");
        memfs.add_file("/srv/a.txt", b"remote", 1_600_000_000, 0o640);
        let connections = serving_remote(&memfs);
        let downloads = tempfile::tempdir().unwrap();
        std::fs::write(downloads.path().join("a.txt"), b"mine").unwrap();
        // A dangling symlink takes its name too.
        std::os::unix::fs::symlink("/nowhere", downloads.path().join("a (1).txt")).unwrap();
        let local = LocalFs::new();

        let file = download(
            &connections,
            &local,
            &remote_location("/srv/a.txt"),
            downloads.path(),
        )
        .await
        .unwrap();
        assert_eq!(file, downloads.path().join("a (2).txt"));
        assert_eq!(std::fs::read(&file).unwrap(), b"remote");
        let metadata = std::fs::metadata(&file).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o640);
        assert_eq!(
            std::fs::read(downloads.path().join("a.txt")).unwrap(),
            b"mine"
        );
        assert!(!downloads.path().join("a (2).txt.scpto-part").exists());
    }

    #[tokio::test]
    async fn only_regular_files_are_downloaded() {
        let memfs = MemFs::new(remote("web"));
        memfs.add_dir("/srv");
        memfs.add_special("/srv/fifo", SpecialKind::Fifo);
        memfs.add_file("/srv/f", b"f", 0, 0o644);
        let connections = serving_remote(&memfs);
        let downloads = tempfile::tempdir().unwrap();
        let local = LocalFs::new();
        let get = |path: &'static str, into: PathBuf| {
            let connections = &connections;
            let local = &local;
            async move { download(connections, local, &remote_location(path), &into).await }
        };

        assert_eq!(
            get("/srv", downloads.path().to_path_buf()).await,
            Err(Error::Fs(FsError::IsADirectory {
                path: PathBuf::from("/srv")
            }))
        );
        assert_eq!(
            get("/", downloads.path().to_path_buf()).await,
            Err(Error::Fs(FsError::IsADirectory {
                path: PathBuf::from("/")
            }))
        );
        assert!(matches!(
            get("/srv/fifo", downloads.path().to_path_buf()).await,
            Err(Error::Fs(FsError::Unsupported { .. }))
        ));
        assert!(matches!(
            get("/srv/missing", downloads.path().to_path_buf()).await,
            Err(Error::Fs(FsError::NotFound { .. }))
        ));
        // A missing Downloads directory is an error, not created.
        let missing = downloads.path().join("Downloads");
        assert_eq!(
            get("/srv/f", missing.clone()).await,
            Err(Error::Fs(FsError::NotFound {
                path: missing.clone()
            }))
        );
        assert!(!missing.exists());
        assert_eq!(std::fs::read_dir(downloads.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn revealing_a_missing_file_fails_before_any_program_runs() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, mut events) = ctx(Arc::new(MapConnections::local()), temp.path());
        let missing = Location {
            endpoint: Endpoint::Local,
            path: temp.path().join("missing"),
        };
        reveal(&ctx, missing.clone());
        match next(&mut events).await {
            BgEvent::Revealed { location, result } => {
                assert_eq!(location, missing);
                assert_eq!(
                    result,
                    Err(Error::Fs(FsError::NotFound {
                        path: missing.path.clone()
                    }))
                );
            }
            other => panic!("expected Revealed, got {other:?}"),
        }

        // A remote endpoint that is not connected fails the download.
        let gone = remote_location("/srv/a.txt");
        reveal(&ctx, gone.clone());
        match next(&mut events).await {
            BgEvent::Revealed { location, result } => {
                assert_eq!(location, gone);
                assert!(matches!(result, Err(Error::Connect(_))), "{result:?}");
            }
            other => panic!("expected Revealed, got {other:?}"),
        }
    }

    #[test]
    fn the_opener_shows_the_file_or_its_directory() {
        let command = opener(Path::new("/Users/me/a b.txt"));
        assert_eq!(command.as_std().get_program(), OPENER);
        #[cfg(target_os = "macos")]
        assert_eq!(args(&command), ["-R", "/Users/me/a b.txt"]);
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(args(&command), ["/Users/me"]);
            assert_eq!(args(&opener(Path::new("/"))), ["/"]);
            assert_eq!(
                opener_failure(Some(3)),
                "no application to open directories was found"
            );
            assert_eq!(opener_failure(Some(9)), "");
        }
    }

    #[tokio::test]
    async fn dropped_paths_must_all_be_existing_absolute_paths() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a file");
        std::fs::write(&file, b"x").unwrap();
        let dangling = dir.path().join("dangling");
        std::os::unix::fs::symlink("/nowhere", &dangling).unwrap();
        let (ctx, mut events) = ctx(Arc::new(MapConnections::local()), dir.path());
        let cases = [
            (vec![file.clone(), dir.path().to_path_buf(), dangling], true),
            (vec![file.clone(), dir.path().join("missing")], false),
            (vec![file.clone(), PathBuf::from("relative")], false),
            (vec![], false),
        ];
        for (paths, expected) in cases {
            check_dropped_paths(&ctx, "pasted".into(), paths.clone());
            match next(&mut events).await {
                BgEvent::DroppedPathsChecked {
                    text,
                    paths: got,
                    all_exist,
                } => {
                    assert_eq!(text, "pasted");
                    assert_eq!(got, paths);
                    assert_eq!(all_exist, expected, "{paths:?}");
                }
                other => panic!("expected DroppedPathsChecked, got {other:?}"),
            }
        }
    }

    /// Downloads a seed file over real SFTP, as a remote reveal does.
    #[tokio::test]
    async fn docker_downloads_a_remote_file() {
        let Some(config) = it_config() else {
            return;
        };
        for host in ["it-modern-plain", "it-legacy-plain"] {
            let endpoint = remote(host);
            let mut connections = MapConnections::local();
            connections
                .fs
                .insert(endpoint.clone(), sftp(&config, &endpoint).await);
            let downloads = tempfile::tempdir().unwrap();
            let location = Location {
                endpoint,
                path: PathBuf::from("/home/plain/seed/hello.txt"),
            };
            let local = LocalFs::new();
            for expected in ["hello.txt", "hello (1).txt"] {
                let file = download(&connections, &local, &location, downloads.path())
                    .await
                    .unwrap();
                assert_eq!(file, downloads.path().join(expected), "{host}");
                assert_eq!(std::fs::read(&file).unwrap(), b"hello scpto\n");
            }
        }
    }
}
