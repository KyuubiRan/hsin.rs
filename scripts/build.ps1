<#
.SYNOPSIS
Build hsin and hsind and copy both binaries into one test directory.

.EXAMPLE
./scripts/build.ps1

.EXAMPLE
./scripts/build.ps1 release

.EXAMPLE
./scripts/build.ps1 release windows-x64

.EXAMPLE
./scripts/build.ps1 -Profile release -Platform linux-x64
#>
param(
    [Parameter(Position = 0)]
    [Alias("Profile")]
    [ValidateSet("debug", "release")]
    [string]$Configuration = "debug",

    [Parameter(Position = 1)]
    [string]$Platform = "host",

    [Alias("Target")]
    [string]$TargetTriple,

    [string]$Output,

    [switch]$Clean
)

$ErrorActionPreference = "Stop"
$RepoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $RepoRoot

try {
    $HostTarget = rustc -vV |
        Where-Object { $_ -like "host: *" } |
        ForEach-Object { $_.Substring(6).Trim() }

    if ([string]::IsNullOrWhiteSpace($HostTarget)) {
        throw "Could not determine the rustc host target."
    }

    if ([string]::IsNullOrWhiteSpace($TargetTriple)) {
        $TargetTriple = switch ($Platform.ToLowerInvariant()) {
            "host" { $HostTarget }
            { $_ -in "macos-arm64", "mac-arm64", "darwin-arm64" } { "aarch64-apple-darwin" }
            { $_ -in "macos-x64", "mac-x64", "darwin-x64" } { "x86_64-apple-darwin" }
            "linux-arm64" { "aarch64-unknown-linux-gnu" }
            "linux-x64" { "x86_64-unknown-linux-gnu" }
            { $_ -in "windows-x64", "win-x64" } { "x86_64-pc-windows-msvc" }
            { $_ -in "windows-arm64", "win-arm64" } { "aarch64-pc-windows-msvc" }
            default { $Platform }
        }
    }

    $CargoArgs = @()
    $Builder = "cargo build"
    if ($TargetTriple -eq $HostTarget) {
        $CargoArgs = @("build", "--workspace")
        $SourceDirectory = Join-Path $RepoRoot "target/$Configuration"
    }
    elseif ($TargetTriple -like "*-unknown-linux-gnu") {
        $Builder = "cargo zigbuild"
        $CargoArgs = @("zigbuild", "--workspace", "--target", $TargetTriple)
        $SourceDirectory = Join-Path $RepoRoot "target/$TargetTriple/$Configuration"
    }
    elseif ($TargetTriple -like "*-pc-windows-msvc") {
        $Builder = "cargo xwin build"
        $CargoArgs = @("xwin", "build", "--workspace", "--target", $TargetTriple)
        if ($TargetTriple -eq "aarch64-pc-windows-msvc") {
            # ring compiles its C sources with the GCC-style clang driver on Windows
            # AArch64, which rejects the /imsvc include flags that cargo-xwin's
            # default clang-cl backend emits.
            $env:XWIN_CROSS_COMPILER = "clang"
        }
        $SourceDirectory = Join-Path $RepoRoot "target/$TargetTriple/$Configuration"
    }
    else {
        $CargoArgs = @("build", "--workspace", "--target", $TargetTriple)
        $SourceDirectory = Join-Path $RepoRoot "target/$TargetTriple/$Configuration"
    }

    if ($Configuration -eq "release") {
        $CargoArgs += "--release"
    }

    Write-Host "host:    $HostTarget"
    Write-Host "target:  $TargetTriple"
    Write-Host "profile: $Configuration"
    Write-Host "builder: $Builder"

    & cargo @CargoArgs
    if ($LASTEXITCODE -ne 0) {
        throw "Cargo build failed with exit code $LASTEXITCODE."
    }

    if ([string]::IsNullOrWhiteSpace($Output)) {
        $Output = Join-Path $RepoRoot "artifacts/$TargetTriple/$Configuration"
    }
    elseif (![IO.Path]::IsPathRooted($Output)) {
        $Output = Join-Path $RepoRoot $Output
    }

    if ($Clean -and (Test-Path $Output)) {
        Remove-Item -Recurse -Force $Output
    }
    New-Item -ItemType Directory -Force $Output | Out-Null

    $Suffix = if ($TargetTriple -like "*-windows-*") { ".exe" } else { "" }
    foreach ($Binary in @("hsin", "hsind")) {
        $Source = Join-Path $SourceDirectory "$Binary$Suffix"
        if (!(Test-Path -PathType Leaf $Source)) {
            throw "Missing build output: $Source"
        }
        Copy-Item -Force $Source (Join-Path $Output "$Binary$Suffix")
    }
    Copy-Item -Force (Join-Path $RepoRoot "README.md") (Join-Path $Output "README.md")
    Copy-Item -Force (Join-Path $RepoRoot "LICENSE") (Join-Path $Output "LICENSE")

    Write-Host ""
    Write-Host "build outputs:"
    Get-Item (Join-Path $Output "hsin$Suffix"), (Join-Path $Output "hsind$Suffix") |
        Select-Object Name, Length, FullName |
        Format-Table -AutoSize
    Write-Host "directory: $Output"
}
finally {
    Pop-Location
}
