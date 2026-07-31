param([Parameter(Mandatory = $true)][string]$Target)
$ErrorActionPreference = "Stop"

$metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
$version = ($metadata.packages | Where-Object name -eq "hsin").version
$root = "dist/hsin-$version-$Target"
Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $root | Out-Null
Copy-Item "target/$Target/release/hsin.exe" "$root/hsin.exe"
Copy-Item "target/$Target/release/hsind.exe" "$root/hsind.exe"
Copy-Item README.md "$root/README.md"
Copy-Item LICENSE "$root/LICENSE"
$combined = (Get-Item "$root/hsin.exe").Length + (Get-Item "$root/hsind.exe").Length
if ($combined -gt 25MB) {
    throw "stripped binaries exceed 25 MiB combined"
}
# Compress the directory itself, not its contents, so the archive expands into
# one top-level folder like the tar archives do.
Compress-Archive -Force -Path $root -DestinationPath "$root.zip"
if ((Get-Item "$root.zip").Length -gt 15MB) {
    throw "release archive exceeds 15 MiB"
}

$archive = Split-Path -Leaf "$root.zip"
$digest = (Get-FileHash "$root.zip" -Algorithm SHA256).Hash.ToLower()
Set-Content -NoNewline -Encoding ascii "$root.zip.sha256" "$digest  $archive`n"
