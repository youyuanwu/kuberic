use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::owned_command::helper_receipt::{
    CompletionCell, HelperLaunch, HelperOutcome, PostmasterLaunch,
};
use rustix::process::{Pid, WaitOptions, getpid, set_child_subreaper, wait};

pub(crate) const ARGUMENT: &str = "--internal-postgres-supervisor";
pub(crate) const CLEAR_PGDATA_ARGUMENT: &str = "--internal-postgres-clear-pgdata";
pub(crate) const START: u8 = 1;
pub(crate) const STOP: u8 = 2;
pub(crate) const COMPLETION_ARGUMENT: &str = "--native-completion-cell";
pub(crate) const POSTMASTER_COMPLETION_ARGUMENT: &str = "--postmaster-completion-cell";

enum NativeCell {
    Helper(CompletionCell),
    Postmaster(CompletionCell<PostmasterLaunch>),
}

impl NativeCell {
    fn send_selection(&self) -> io::Result<()> {
        match self {
            Self::Helper(cell) => {
                crate::owned_command::helper_receipt::send_selection(io::stdin(), &cell.selection)
            }
            Self::Postmaster(cell) => {
                crate::owned_command::helper_receipt::send_selection(io::stdin(), &cell.selection)
            }
        }
    }

    fn complete(self, outcome: HelperOutcome) -> io::Result<()> {
        match self {
            Self::Helper(cell) => cell.complete(outcome),
            Self::Postmaster(cell) => cell.complete(outcome),
        }
    }
}

pub(crate) fn executable() -> io::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let mut directory = executable
        .parent()
        .ok_or_else(|| io::Error::other("missing executable directory"))?;
    // Cargo integration tests use the package binary beside deps/.
    if cfg!(feature = "testing") && directory.file_name().is_some_and(|name| name == "deps") {
        directory = directory.parent().unwrap();
    }
    Ok(directory.join("postgres-replicated"))
}

fn clear_pgdata(mut arguments: impl Iterator<Item = std::ffi::OsString>) -> io::Result<()> {
    let data = PathBuf::from(
        arguments
            .next()
            .ok_or_else(|| io::Error::other("missing PGDATA deletion path"))?,
    );
    #[cfg(feature = "testing")]
    let gate = match (arguments.next(), arguments.next()) {
        (None, None) => None,
        (Some(entry), Some(marker)) => Some((entry, PathBuf::from(marker))),
        _ => return Err(io::Error::other("incomplete PGDATA deletion gate")),
    };
    if arguments.next().is_some() {
        return Err(io::Error::other("unexpected PGDATA deletion argument"));
    }
    // This re-exec is owned by the same generation registry as backup/rewind.
    // Its owner must reap it before releasing build authority, including Drop.
    std::fs::create_dir_all(&data)?;
    for entry in std::fs::read_dir(&data)? {
        let entry = entry?;
        let directory = entry.file_type()?.is_dir();
        #[cfg(feature = "testing")]
        if let Some((name, marker)) = &gate
            && entry.file_name() == *name
        {
            std::fs::write(marker, std::process::id().to_string())?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while marker.try_exists()? {
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::other("PGDATA deletion gate timed out"));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        if directory {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
    }
    std::fs::File::open(data)?.sync_all()
}

/// Internal re-exec entry point; run before creating threads or a Tokio runtime.
#[doc(hidden)]
pub fn run_if_requested() -> io::Result<bool> {
    let mut arguments = std::env::args_os().skip(1);
    match arguments.next() {
        Some(argument) if argument == CLEAR_PGDATA_ARGUMENT => {
            clear_pgdata(arguments)?;
            return Ok(true);
        }
        Some(argument) if argument == ARGUMENT => {}
        _ => return Ok(false),
    }
    let mut postgres = arguments
        .next()
        .ok_or_else(|| io::Error::other("missing supervised executable"))?;
    let cell = if postgres == COMPLETION_ARGUMENT || postgres == POSTMASTER_COMPLETION_ARGUMENT {
        let postmaster = postgres == POSTMASTER_COMPLETION_ARGUMENT;
        let launch = arguments
            .next()
            .ok_or_else(|| io::Error::other("missing native helper launch selection"))?;
        let cell = if postmaster {
            let launch: PostmasterLaunch =
                serde_json::from_slice(launch.as_encoded_bytes()).map_err(io::Error::other)?;
            NativeCell::Postmaster(CompletionCell::create(launch)?)
        } else {
            let launch: HelperLaunch =
                serde_json::from_slice(launch.as_encoded_bytes()).map_err(io::Error::other)?;
            NativeCell::Helper(CompletionCell::create(launch)?)
        };
        postgres = arguments
            .next()
            .ok_or_else(|| io::Error::other("missing supervised native helper"))?;
        Some(cell)
    } else {
        None
    };
    // These handlers belong only to the re-exec, never the application host.
    // Caught handlers reset on exec, preserving PostgreSQL's signal semantics.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let _entered = runtime.enter();
    let _interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let _terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    set_child_subreaper(Some(getpid()))?;
    if let Some(cell) = &cell {
        cell.send_selection()?;
    }
    // No launch occurs before the owner retains our pidfd. STOP completes an
    // undispatched run normally, rather than killing its ownership root.
    let mut message = [0];
    let undispatched_eof = match io::stdin().read_exact(&mut message) {
        Ok(()) => false,
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => true,
        Err(error) => return Err(error),
    };
    if undispatched_eof {
        message[0] = STOP;
    }
    let dispatched = message[0] == START;
    let mut launcher = match message[0] {
        START => {
            let child = Command::new(postgres)
                .args(arguments)
                .stdin(Stdio::null())
                .spawn()?;
            Some(Pid::from_raw(child.id() as i32).expect("newly spawned launcher"))
        }
        STOP => None,
        _ => return Err(io::Error::other("invalid supervisor start command")),
    };
    let mut status = launcher.is_none().then_some(0i32);
    let mut cancelled = !dispatched;
    let mut owner_closed = undispatched_eof;
    let mut cleanup_deadline = None;
    // This isolated process owns exactly one launch lineage, including orphans
    // from double forks and setsid. Only ECHILD proves successful completion.
    loop {
        if !owner_closed {
            let input = io::stdin();
            let mut fds = [rustix::event::PollFd::new(
                &input,
                rustix::event::PollFlags::IN,
            )];
            rustix::event::poll(
                &mut fds,
                Some(&rustix::event::Timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                }),
            )?;
            if !fds[0].revents().is_empty() {
                match rustix::io::read(&input, &mut message) {
                    Ok(0) => {
                        owner_closed = true;
                        cancelled = true;
                    }
                    Ok(1) if message[0] == STOP => cancelled = true,
                    Ok(_) => return Err(io::Error::other("invalid supervisor owner command")),
                    Err(rustix::io::Errno::INTR) => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        if cancelled && cleanup_deadline.is_none() {
            crate::owned_process::terminate_supervised_lineage()?;
            cleanup_deadline = Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
        }
        match wait(WaitOptions::NOHANG) {
            Ok(Some((pid, waited))) => {
                if launcher == Some(pid) && (waited.exited() || waited.signaled()) {
                    status = Some(waited.as_raw());
                    launcher = None;
                }
            }
            Ok(None) | Err(rustix::io::Errno::INTR) => {
                if cleanup_deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                    return Err(io::Error::other("supervisor ECHILD reaping timed out"));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(rustix::io::Errno::CHILD) => {
                // The socket separates the launcher's status from stdout/stderr
                // and from our own exit, which certifies complete tree reaping.
                let status = status
                    .ok_or_else(|| io::Error::other("supervisor lost launcher exit status"))?;
                if let Some(cell) = cell {
                    cell.complete(HelperOutcome {
                        dispatched,
                        cancelled,
                        launcher_status: status,
                    })?;
                }
                let bytes = status.to_ne_bytes();
                if !owner_closed {
                    match rustix::io::write(io::stdin(), &bytes) {
                        Ok(length) if length == bytes.len() => {}
                        Err(rustix::io::Errno::PIPE | rustix::io::Errno::CONNRESET) => {}
                        Err(error) => return Err(error.into()),
                        Ok(_) => {
                            return Err(io::Error::new(
                                io::ErrorKind::WriteZero,
                                "supervisor completion was not written",
                            ));
                        }
                    }
                }
                return Ok(true);
            }
            Err(error) => return Err(error.into()),
        }
    }
}
