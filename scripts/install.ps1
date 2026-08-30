# Install the latest hsin release for this machine.
#
#   irm https://raw.githubusercontent.com/KyuubiRan/hsin.rs/main/scripts/install.ps1 | iex
#
# Environment:
#   HSIN_INSTALL_DIR  where to put the binaries (default: %LOCALAPPDATA%\Programs\hsin)
#   HSIN_VERSION      release tag to install, such as v0.2.0 (default: the latest)
$ErrorActionPreference = "Stop"

$repo = "KyuubiRan/hsin.rs"
$destination = if ($env:HSIN_INSTALL_DIR) { $env:HSIN_INSTALL_DIR } else { "$env:LOCALAPPDATA\Programs\hsin" }
$serviceHome = if ($env:HSIN_HOME) { [IO.Path]::GetFullPath($env:HSIN_HOME) } else { "$env:LOCALAPPDATA\hsin" }

function Invoke-WithRetry {
    param([scriptblock]$Operation, [int]$Attempts = 3)
    for ($attempt = 1; ; $attempt++) {
        try {
            return & $Operation
        } catch {
            if ($attempt -ge $Attempts) { throw }
            Start-Sleep -Seconds $attempt
        }
    }
}

function Get-Sha256 {
    param([string]$Path)

    try {
        return Invoke-WithRetry -Attempts 5 -Operation {
            if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
                throw "downloaded file is missing"
            }

            $stream = $null
            $hasher = $null
            try {
                $stream = [IO.File]::Open(
                    $Path,
                    [IO.FileMode]::Open,
                    [IO.FileAccess]::Read,
                    [IO.FileShare]::Read
                )
                $hasher = [Security.Cryptography.SHA256]::Create()
                $digest = $hasher.ComputeHash($stream)
                return ([BitConverter]::ToString($digest)).Replace("-", "").ToLowerInvariant()
            } finally {
                if ($hasher) { $hasher.Dispose() }
                if ($stream) { $stream.Dispose() }
            }
        }
    } catch {
        if (-not (Test-Path -LiteralPath $Path -PathType Leaf -ErrorAction SilentlyContinue)) {
            throw "downloaded archive disappeared before checksum verification; Windows Security or another security product may have quarantined it; check Protection History before retrying"
        }
        throw "cannot read downloaded archive for checksum verification after 5 attempts: $($_.Exception.Message)"
    }
}

function Get-ServiceRegistrationState {
    param([string]$Cli, [string]$Home)

    if (-not (Test-Path -LiteralPath $Cli -PathType Leaf -ErrorAction SilentlyContinue)) {
        return $null
    }
    $previousErrorActionPreference = $ErrorActionPreference
    try {
        # Doctor checks the exact per-HSIN_HOME task name without bootstrapping the daemon.
        $ErrorActionPreference = "Continue"
        $output = & $Cli --json doctor 2>$null | Out-String
        $exitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
    if ($exitCode -ne 0 -or [string]::IsNullOrWhiteSpace($output)) { return $null }

    try {
        $report = $output | ConvertFrom-Json
    } catch {
        return $null
    }
    $codes = @($report.findings | ForEach-Object { [string]$_.code })
    if ($codes -contains "service_check_failed") { return $null }
    if ($codes -contains "service_definition_missing") { return $false }
    if ($codes -contains "service_definition_orphaned") { return $true }
    return Test-Path -LiteralPath (Join-Path $Home ".hsin-home") -PathType Leaf
}

function Invoke-DaemonUpdate {
    param([string]$Cli)

    if (-not (Test-Path -LiteralPath $Cli -PathType Leaf -ErrorAction SilentlyContinue)) {
        return 1
    }
    $previousErrorActionPreference = $ErrorActionPreference
    try {
        # A native program may write useful diagnostics to stderr. Its exit code is
        # the reliable result and is handled as a warning by the caller.
        $ErrorActionPreference = "Continue"
        & $Cli daemon update | Out-Host
        return [int]$LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
}

function Normalize-Version {
    param([string]$Version)
    if ($Version.StartsWith("v")) { return $Version.Substring(1) }
    return $Version
}

$releaseTag = if ($env:HSIN_VERSION) {
    $env:HSIN_VERSION
} else {
    $release = Invoke-WithRetry {
        Invoke-RestMethod -UseBasicParsing `
            -Headers @{ "User-Agent" = "hsin-installer" } `
            -Uri "https://api.github.com/repos/$repo/releases/latest"
    }
    if (-not $release.tag_name) { throw "GitHub returned no latest hsin release tag" }
    [string]$release.tag_name
}

if ($env:HSIN_CURRENT_VERSION -and
    (Normalize-Version $env:HSIN_CURRENT_VERSION) -eq (Normalize-Version $releaseTag)) {
    Write-Host "hsin $(Normalize-Version $env:HSIN_CURRENT_VERSION) is already the latest release"
    return
}

$architecture = switch ($env:PROCESSOR_ARCHITECTURE) {
    "ARM64" { "aarch64" }
    "AMD64" { "x86_64" }
    default { throw "hsin has no build for $env:PROCESSOR_ARCHITECTURE" }
}

$target = "$architecture-pc-windows-msvc"
$archive = "hsin-$target.zip"
$base = "https://github.com/$repo/releases/download/$releaseTag"
$work = Join-Path ([IO.Path]::GetTempPath()) ("hsin-" + [Guid]::NewGuid().ToString("N"))
$deferredDeletes = [Collections.Generic.List[string]]::new()
New-Item -ItemType Directory -Force -Path $work | Out-Null

function Install-Binary {
    param([string]$Source, [string]$Destination)

    Get-ChildItem "$Destination.old.*" -ErrorAction SilentlyContinue |
        Remove-Item -Force -ErrorAction SilentlyContinue
    $staged = "$Destination.new"
    Remove-Item -Force $staged -ErrorAction SilentlyContinue
    Copy-Item -Force $Source $staged
    (Get-Item $staged).IsReadOnly = $false

    $old = $null
    if (Test-Path $Destination) {
        (Get-Item $Destination).IsReadOnly = $false
        $old = "$Destination.old.$PID"
        Move-Item -Force $Destination $old
        $deferredDeletes.Add($old)
    }

    try {
        Move-Item -Force $staged $Destination
    } catch {
        if ($old -and (Test-Path $old) -and -not (Test-Path $Destination)) {
            Move-Item -Force $old $Destination
            $deferredDeletes.Remove($old) | Out-Null
        }
        throw
    }
}

function Remove-ReplacedBinaries {
    foreach ($path in @($deferredDeletes)) {
        Remove-Item -Force $path -ErrorAction SilentlyContinue
        if (-not (Test-Path $path)) { $deferredDeletes.Remove($path) | Out-Null }
    }
    if ($deferredDeletes.Count -eq 0 -or -not $env:HSIN_UPDATE_PARENT_PID) { return }

    $paths = ($deferredDeletes | ForEach-Object { "'" + $_.Replace("'", "''") + "'" }) -join ","
    $cleanup = "Wait-Process -Id $env:HSIN_UPDATE_PARENT_PID -ErrorAction SilentlyContinue; " +
        "Start-Sleep -Milliseconds 250; @($paths) | Remove-Item -Force -ErrorAction SilentlyContinue"
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($cleanup))
    Start-Process powershell.exe -WindowStyle Hidden -ArgumentList @(
        "-NoLogo", "-NoProfile", "-NonInteractive", "-EncodedCommand", $encoded
    ) | Out-Null
}

try {
    Write-Host "downloading $archive from $releaseTag"
    $download = {
        param($uri, $outFile)
        Invoke-WithRetry {
            # -UseBasicParsing keeps Windows PowerShell 5.1 off the Internet Explorer
            # engine, which prompts and is absent where IE has been removed.
            Invoke-WebRequest -UseBasicParsing -Uri $uri -OutFile $outFile
        } | Out-Null
    }
    & $download "$base/$archive" "$work\$archive"
    & $download "$base/SHA256SUMS" "$work\SHA256SUMS"

    $archivePath = Join-Path $work $archive
    $line = Select-String -LiteralPath "$work\SHA256SUMS" -Pattern "\s$([regex]::Escape($archive))$" |
        Select-Object -First 1
    if (-not $line) { throw "SHA256SUMS lists no digest for $archive" }
    $expected = ($line.Line -split '\s+')[0]
    $actual = Get-Sha256 $archivePath
    if ($expected -ne $actual) {
        throw "checksum mismatch for ${archive}: expected $expected, got $actual"
    }

    Expand-Archive -LiteralPath $archivePath -DestinationPath "$work\extracted" -Force
    New-Item -ItemType Directory -Force -Path $destination | Out-Null

    Get-Process hsind -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -like "$destination\*" } |
        Stop-Process -Force -ErrorAction SilentlyContinue

    Install-Binary "$work\extracted\hsin-$target\hsin.exe" "$destination\hsin.exe"
    Install-Binary "$work\extracted\hsin-$target\hsind.exe" "$destination\hsind.exe"
    Write-Host "installed hsin.exe and hsind.exe $releaseTag into $destination"

    $serviceRegistered = Get-ServiceRegistrationState "$destination\hsin.exe" $serviceHome
    if ($null -eq $serviceRegistered) {
        Write-Warning "Could not determine whether the background daemon is registered; the program update is complete."
        Write-Warning "Run 'hsin doctor' before retrying 'hsin daemon update'."
    } elseif ($serviceRegistered) {
        Write-Host "updating the existing background daemon"
        $daemonExitCode = Invoke-DaemonUpdate "$destination\hsin.exe"
        if ($daemonExitCode -ne 0) {
            Write-Warning "hsin was updated, but the background daemon could not be updated or started (exit code $daemonExitCode)."
            Write-Warning "Run 'hsin doctor' and check Windows Security Protection History before retrying 'hsin daemon update'."
        }
    }

    $user = [Environment]::GetEnvironmentVariable("Path", "User")
    if (($user -split ";") -notcontains $destination) {
        $updatedUserPath = if ([string]::IsNullOrWhiteSpace($user)) {
            $destination
        } else {
            "$user;$destination"
        }
        [Environment]::SetEnvironmentVariable("Path", $updatedUserPath, "User")
        Write-Host ""
        Write-Host "Added $destination to your user PATH."
    }

    if (($env:Path -split ";") -notcontains $destination) {
        $env:Path = if ([string]::IsNullOrWhiteSpace($env:Path)) {
            $destination
        } else {
            "$env:Path;$destination"
        }
        Write-Host "hsin is now available in this PowerShell session."
    }

    if ($serviceRegistered -eq $false) {
        Write-Host ""
        Write-Host "Run 'hsin' to start. It registers and starts the background daemon by"
        Write-Host "itself the first time it needs one, and needs no administrator rights."
    }
    Remove-ReplacedBinaries
}
finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
