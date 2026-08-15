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
$serviceInstalled = (Test-Path (Join-Path $serviceHome ".hsin-home")) -or
    (Test-Path (Join-Path $serviceHome "bin\hsind.exe"))

function Invoke-WithRetry {
    param([scriptblock]$Operation)
    for ($attempt = 1; ; $attempt++) {
        try {
            return & $Operation
        } catch {
            if ($attempt -ge 3) { throw }
            Start-Sleep -Seconds $attempt
        }
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

    $line = Select-String -Path "$work\SHA256SUMS" -Pattern "\s$([regex]::Escape($archive))$" |
        Select-Object -First 1
    if (-not $line) { throw "SHA256SUMS lists no digest for $archive" }
    $expected = ($line.Line -split '\s+')[0]
    $actual = (Get-FileHash "$work\$archive" -Algorithm SHA256).Hash.ToLower()
    if ($expected -ne $actual) {
        throw "checksum mismatch for ${archive}: expected $expected, got $actual"
    }

    Expand-Archive -Path "$work\$archive" -DestinationPath "$work\extracted" -Force
    New-Item -ItemType Directory -Force -Path $destination | Out-Null

    Get-Process hsind -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -like "$destination\*" } |
        Stop-Process -Force -ErrorAction SilentlyContinue

    Install-Binary "$work\extracted\hsin-$target\hsin.exe" "$destination\hsin.exe"
    Install-Binary "$work\extracted\hsin-$target\hsind.exe" "$destination\hsind.exe"
    Write-Host "installed hsin.exe and hsind.exe $releaseTag into $destination"

    if ($serviceInstalled) {
        Write-Host "updating the existing background daemon"
        & "$destination\hsin.exe" daemon update
        if ($LASTEXITCODE -ne 0) { throw "hsin daemon update exited with $LASTEXITCODE" }
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

    if (-not $serviceInstalled) {
        Write-Host ""
        Write-Host "Run 'hsin' to start. It registers and starts the background daemon by"
        Write-Host "itself the first time it needs one, and needs no administrator rights."
    }
    Remove-ReplacedBinaries
}
finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
