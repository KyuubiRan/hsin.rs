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
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let Some(stdout) = child.stdout.take() else {
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
    if output.len() as u64 > MAX_OUTPUT {
        return Err(unavailable());
    }
    Ok(Output {
        status,
        stdout: output,
        stderr: Vec::new(),
    })
}
