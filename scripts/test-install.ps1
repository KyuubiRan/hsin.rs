$ErrorActionPreference = "Stop"

$installer = Join-Path $PSScriptRoot "install.ps1"
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    $installer,
    [ref]$tokens,
    [ref]$errors
)
if ($errors.Count -gt 0) {
    $messages = ($errors | ForEach-Object { $_.Message }) -join "; "
    throw "install.ps1 has parse errors: $messages"
}

$helperNames = @(
    "Invoke-WithRetry",
    "Get-Sha256",
    "Get-ServiceRegistrationState",
    "Invoke-DaemonUpdate"
)
foreach ($name in $helperNames) {
    $function = $ast.Find({
        param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
            $node.Name -eq $name
    }, $true)
    if (-not $function) { throw "install.ps1 defines no $name function" }
    Invoke-Expression $function.Extent.Text
}

$work = Join-Path ([IO.Path]::GetTempPath()) ("hsin-installer-test-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $work | Out-Null
try {
    $payload = Join-Path $work "payload.bin"
    [IO.File]::WriteAllBytes($payload, [Text.Encoding]::ASCII.GetBytes("hsin"))
    $actual = Get-Sha256 $payload
    if ($actual -ne "bf792fe9efd6433680228d566b523b73f6464b38c77e4a1197f20afe24ea911e") {
        throw "Get-Sha256 returned $actual"
    }

    $fakeCli = Join-Path $work "fake-hsin.cmd"
    [IO.File]::WriteAllLines($fakeCli, @("@echo off", "exit /b 7"))
    $exitCode = Invoke-DaemonUpdate $fakeCli
    if ($exitCode -ne 7) { throw "Invoke-DaemonUpdate returned $exitCode instead of 7" }

    [IO.File]::WriteAllLines($fakeCli, @(
        "@echo off",
        'echo {"findings":[{"code":"service_definition_missing"}]}',
        "exit /b 0"
    ))
    if ((Get-ServiceRegistrationState $fakeCli $work) -ne $false) {
        throw "a missing task must not be treated as registered"
    }

    [IO.File]::WriteAllLines($fakeCli, @(
        "@echo off",
        'echo {"findings":[{"code":"service_definition_orphaned"}]}',
        "exit /b 0"
    ))
    if ((Get-ServiceRegistrationState $fakeCli $work) -ne $true) {
        throw "an orphaned task must still be treated as registered"
    }

    [IO.File]::WriteAllLines($fakeCli, @(
        "@echo off",
        'echo {"findings":[{"code":"service_check_failed"}]}',
        "exit /b 0"
    ))
    if ($null -ne (Get-ServiceRegistrationState $fakeCli $work)) {
        throw "a failed task query must produce an unknown registration state"
    }
} finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
