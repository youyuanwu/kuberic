use std::collections::BTreeMap;
use std::process::{ExitStatus, Output};
use std::sync::Mutex;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::instance::PgError;
use crate::owned_process::OwnedProcess;

#[derive(Default)]
struct Commands {
    next_id: u64,
    closed: bool,
    running: BTreeMap<u64, OwnedProcess>,
    error: Option<PgError>,
}

/// Registration, launch permission and teardown share one lock. The future's
/// guard reaps on cancellation; the registry also survives a dropped future so
/// synchronous host abort can join every helper and retain cleanup failures.
#[derive(Default)]
pub(crate) struct OwnedCommands(Mutex<Commands>);

impl OwnedCommands {
    pub(crate) async fn output(&self, command: &mut Command) -> Result<Output, PgError> {
        let (id, mut stdout, mut stderr, started) = {
            let mut commands = self.0.lock().unwrap();
            if commands.closed {
                return Err(PgError::Process("owned commands are closed".into()));
            }
            if let Some(error) = &commands.error {
                return Err(error.clone());
            }
            let mut process = match OwnedProcess::spawn(command) {
                Ok(process) => process,
                Err(error) => {
                    // Retaining a spawn/identity failure also covers the rare
                    // case where the undispatched root could not be reaped.
                    commands.error.get_or_insert(error.clone());
                    return Err(error);
                }
            };
            let (stdout, stderr) = process.take_output();
            let id = commands.next_id;
            commands.next_id += 1;
            commands.running.insert(id, process);
            // Ownership is registered before the supervisor may launch.
            let started = commands.running.get(&id).unwrap().start();
            (id, stdout, stderr, started)
        };
        let guard = CommandGuard { owner: self, id };
        if let Err(error) = started {
            return Err(error.with_cleanup(guard.finish()));
        }
        let mut out = Vec::new();
        let mut err = Vec::new();
        let collected = tokio::try_join!(
            async {
                stdout
                    .read_to_end(&mut out)
                    .await
                    .map_err(|error| PgError::Process(format!("read owned stdout: {error}")))
            },
            async {
                stderr
                    .read_to_end(&mut err)
                    .await
                    .map_err(|error| PgError::Process(format!("read owned stderr: {error}")))
            },
            guard.wait(),
        );
        let cleanup = guard.finish();
        match collected {
            Ok((_, _, status)) => {
                cleanup?;
                Ok(Output {
                    status,
                    stdout: out,
                    stderr: err,
                })
            }
            Err(error) => Err(PgError::Process(format!(
                "{}: {error}; stdout: {}; stderr: {}",
                command.as_std().get_program().to_string_lossy(),
                String::from_utf8_lossy(&out),
                String::from_utf8_lossy(&err),
            ))
            .with_cleanup(cleanup)),
        }
    }

    pub(crate) fn terminate(&self, close: bool) -> Result<(), PgError> {
        let mut commands = self.0.lock().unwrap();
        commands.closed |= close;
        let ids: Vec<_> = commands.running.keys().copied().collect();
        for id in ids {
            Self::remove(&mut commands, id);
        }
        commands.error.clone().map_or(Ok(()), Err)
    }

    pub(crate) fn reopen(&self) -> Result<(), PgError> {
        let mut commands = self.0.lock().unwrap();
        if let Some(error) = &commands.error {
            return Err(error.clone());
        }
        commands.closed = false;
        Ok(())
    }

    fn remove(commands: &mut Commands, id: u64) {
        if let Some(process) = commands.running.get_mut(&id) {
            match process.terminate() {
                Ok(()) => {
                    commands.running.remove(&id);
                }
                Err(error) => {
                    tracing::error!(%error, "owned helper cleanup failed");
                    commands.error.get_or_insert(error);
                }
            }
        }
    }
}

struct CommandGuard<'a> {
    owner: &'a OwnedCommands,
    id: u64,
}

impl CommandGuard<'_> {
    async fn wait(&self) -> Result<ExitStatus, PgError> {
        loop {
            {
                let mut commands = self.owner.0.lock().unwrap();
                let process = commands
                    .running
                    .get_mut(&self.id)
                    .ok_or_else(|| PgError::Process("owned command cancelled".into()))?;
                if let Some(status) = process.completion()? {
                    return Ok(status);
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn finish(self) -> Result<(), PgError> {
        let owner = self.owner;
        drop(self);
        owner.0.lock().unwrap().error.clone().map_or(Ok(()), Err)
    }
}

impl Drop for CommandGuard<'_> {
    fn drop(&mut self) {
        OwnedCommands::remove(&mut self.owner.0.lock().unwrap(), self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[tokio::test]
    async fn helper_output_preserves_streams_status_and_environment() {
        let commands = OwnedCommands::default();
        let output = commands
            .output(
                Command::new("sh")
                    .args([
                        "-c",
                        "printf '%s' \"$LC_ALL\"; printf diagnostic >&2; exit 37",
                    ])
                    .env("LC_ALL", "C"),
            )
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(37));
        assert_eq!(output.stdout, b"C");
        assert_eq!(output.stderr, b"diagnostic");
        assert!(commands.0.lock().unwrap().running.is_empty());
        commands.terminate(true).unwrap();
    }

    #[tokio::test]
    async fn helper_signal_is_not_a_supervisor_cleanup_failure() {
        let commands = OwnedCommands::default();
        let output = commands
            .output(Command::new("sh").args(["-c", "kill -TERM $$"]))
            .await
            .unwrap();
        assert_eq!(output.status.signal(), Some(15));
        commands.terminate(true).unwrap();
    }

    #[tokio::test]
    async fn helper_pipes_are_drained_concurrently() {
        let commands = OwnedCommands::default();
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            commands.output(Command::new("sh").args([
                "-c",
                "head -c 131072 /dev/zero; head -c 131072 /dev/zero >&2",
            ])),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, vec![0; 131072]);
        assert_eq!(output.stderr, vec![0; 131072]);
    }
}
