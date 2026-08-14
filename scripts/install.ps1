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

$architecture = switch ($env:PROCESSOR_ARCHITECTURE) {
    "ARM64" { "aarch64" }
    "AMD64" { "x86_64" }
    default { throw "hsin has no build for $env:PROCESSOR_ARCHITECTURE" }
}

$target = "$architecture-pc-windows-msvc"
$archive = "hsin-$target.zip"
$base = if ($env:HSIN_VERSION) {
    "https://github.com/$repo/releases/download/$env:HSIN_VERSION"
} else {
    # Asset names carry no version, so this URL keeps working across releases.
    "https://github.com/$repo/releases/latest/download"
}

$work = Join-Path ([IO.Path]::GetTempPath()) ("hsin-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $work | Out-Null

try {
    Write-Host "downloading $archive"
    # -UseBasicParsing keeps Windows PowerShell 5.1 off the Internet Explorer
    # engine, which prompts and is absent where IE has been removed.
    $download = {
        param($uri, $outFile)
        for ($attempt = 1; ; $attempt++) {
            try {
                Invoke-WebRequest -UseBasicParsing -Uri $uri -OutFile $outFile
                return
            } catch {
                if ($attempt -ge 3) { throw }
                Start-Sleep -Seconds $attempt
            }
        }
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

    # A running daemon holds its image open, so stop it before replacing it.
    Get-Process hsind -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -like "$destination\*" } |
        Stop-Process -Force -ErrorAction SilentlyContinue

    Copy-Item "$work\extracted\hsin-$target\hsin.exe", "$work\extracted\hsin-$target\hsind.exe" `
        -Destination $destination -Force

    Write-Host "installed hsin.exe and hsind.exe into $destination"

    if ($serviceInstalled) {
        Write-Host "updating the existing background daemon"
        & "$destination\hsin.exe" daemon update
        if ($LASTEXITCODE -ne 0) { throw "hsin daemon update exited with $LASTEXITCODE" }
    }

    $user = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($user -notlike "*$destination*") {
        [Environment]::SetEnvironmentVariable("Path", "$user;$destination", "User")
        Write-Host ""
        Write-Host "Added $destination to your PATH. Open a new terminal for it to apply."
    }

    if (-not $serviceInstalled) {
        Write-Host ""
        Write-Host "Run 'hsin' to start. It registers and starts the background daemon by"
        Write-Host "itself the first time it needs one, and needs no administrator rights."
    }
}
finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
