//! Bound local process-metadata helpers without a Tokio runtime.
use std::{
    io::Read,
    os::windows::process::CommandExt,
    process::{Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use super::unavailable;
use crate::error::{DaemonError, Result};

const MAX_OUTPUT: u64 = 1024 * 1024;

pub(super) fn bounded_output(command: &mut Command, timeout: Duration) -> Result<Output> {
    let mut child = command
        // The daemon has no console. Creating one for a metadata probe can
        // stall its host initialization and must never display a window.
        .creation_flags(0x0800_0000)
        // NUL is a character-device handle on Windows. Give console hosts
        // actual redirected input with EOF instead of a console-like device.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    drop(child.stdin.take());
    let (Some(stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(unavailable());
    };
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut output = Vec::new();
        let result = stdout
            .take(MAX_OUTPUT + 1)
            .read_to_end(&mut output)
            .map(|_| output);
        let _ = sender.send(result);
    });
    let (stderr_sender, stderr_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        // Drain both redirected streams so the probe cannot block on a full
        // pipe. Diagnostics stay discarded and never enter daemon errors.
        let result = std::io::copy(&mut stderr, &mut std::io::sink()).map(|_| ());
        let _ = stderr_sender.send(result);
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(DaemonError::Config(
                    "isolated process metadata query timed out".into(),
                ));
            }
        }
    };
    let output = receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| DaemonError::Config("isolated process metadata output timed out".into()))??;
    stderr_receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| DaemonError::Config("isolated process metadata output timed out".into()))??;
    if output.len() as u64 > MAX_OUTPUT {
        return Err(unavailable());
    }
    Ok(Output {
        status,
        stdout: output,
        stderr: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dotnet_command(expression: &str) -> Command {
        let mut command = Command::new("powershell.exe");
        command.args([
            "-NoProfile",
            "-NoLogo",
            "-NonInteractive",
            "-Command",
            expression,
        ]);
        command
    }

    #[test]
    fn redirected_dotnet_output_is_drained_and_the_exit_status_is_retained() {
        for code in [0, 7] {
            let output = bounded_output(
                &mut dotnet_command(&format!(
                    "[Console]::Error.Write(('x' * 131072)); [Console]::Out.Write('hsin-probe'); exit {code}"
                )),
                super::super::WINDOWS_METADATA_TIMEOUT,
            )
            .unwrap();
            assert_eq!(output.status.code(), Some(code));
            assert_eq!(output.stdout, b"hsin-probe");
            assert_eq!(output.stderr.len(), 0);
        }
    }

    #[test]
    fn redirected_dotnet_input_reaches_eof_without_a_console() {
        let output = bounded_output(
            &mut dotnet_command("[Console]::Out.Write([Console]::In.ReadToEnd().Length); exit 0"),
            super::super::WINDOWS_METADATA_TIMEOUT,
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"0");
    }

    #[test]
    fn cmd_output_uses_the_same_bounded_pipe_capture() {
        let mut command = Command::new("cmd.exe");
        command.args(["/D", "/C", "echo hsin-cmd-probe"]);
        let output = bounded_output(&mut command, super::super::WINDOWS_METADATA_TIMEOUT).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"hsin-cmd-probe\r\n");
        assert_eq!(output.stderr.len(), 0);
    }

    #[test]
    fn a_nonterminating_probe_is_killed_without_becoming_a_process_identity() {
        let error = bounded_output(
            &mut dotnet_command("[System.Threading.Thread]::Sleep(10000); exit 0"),
            Duration::from_millis(250),
        )
        .unwrap_err();
        assert!(
            matches!(error, DaemonError::Config(message) if message == "isolated process metadata query timed out")
        );
    }
}
