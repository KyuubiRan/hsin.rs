//! Process ids and creation times only; command lines and environments are never read.
use std::process::{Command, ExitStatus, Stdio};

use super::sync_output::bounded_output;
use super::{is_absent, process_start, unavailable};
use crate::error::Result;

#[derive(serde::Deserialize)]
pub(super) struct Member {
    pid: u32,
    start: String,
}

pub(super) fn capture(root: u32) -> Result<Vec<Member>> {
    let expression = format!(
        r"$ErrorActionPreference='Stop'; $all=@(Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId); $ids=[System.Collections.Generic.HashSet[uint32]]::new(); [void]$ids.Add({root}); $result=@(); do {{ $added=$false; foreach($p in $all){{ if($ids.Contains([uint32]$p.ParentProcessId) -and !$ids.Contains([uint32]$p.ProcessId)){{ [void]$ids.Add([uint32]$p.ProcessId); $live=Get-Process -Id $p.ProcessId -ErrorAction SilentlyContinue; if($null -ne $live){{ $result+=@{{pid=[uint32]$p.ProcessId;start=$live.StartTime.ToFileTimeUtc().ToString()}} }}; $added=$true }} }} }} while($added); ConvertTo-Json -InputObject @($result) -Compress"
    );
    let output = bounded_output(
        Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &expression])
            .stdin(Stdio::null())
            .stderr(Stdio::null()),
        std::time::Duration::from_secs(5),
    )?;
    if !output.status.success() {
        return Err(unavailable());
    }
    serde_json::from_slice(&output.stdout).map_err(|_| unavailable())
}

fn taskkill(pid: u32) -> Result<ExitStatus> {
    Ok(bounded_output(
        Command::new("taskkill.exe")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
        std::time::Duration::from_secs(5),
    )?
    .status)
}

pub(super) fn terminate(root: u32, descendants: &[Member]) -> Result<ExitStatus> {
    let status = taskkill(root)?;
    for member in descendants {
        match process_start(member.pid) {
            Ok(start) if start == member.start => {
                let _ = taskkill(member.pid)?;
            }
            Ok(_) => {}
            Err(error) if is_absent(&error) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(status)
}

pub(super) fn all_gone(descendants: &[Member]) -> Result<bool> {
    for member in descendants {
        match process_start(member.pid) {
            Ok(start) if start == member.start => return Ok(false),
            Ok(_) => {}
            Err(error) if is_absent(&error) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}
