param([switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
$projectRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
Push-Location -LiteralPath $projectRoot
try {
    if (-not $SkipBuild) {
        & cargo build --release --offline --locked --bin microphone-cleaner --jobs 2
        if ($LASTEXITCODE -ne 0) { throw 'Release compilation failed.' }
    }
    & python (Join-Path $PSScriptRoot 'audit_pe.py') (Join-Path $projectRoot 'target/release/microphone-cleaner.exe')
    if ($LASTEXITCODE -ne 0) { throw 'Executable resource audit failed.' }
    $manifest = Get-Content -LiteralPath (Join-Path $projectRoot 'Cargo.toml') -Raw
    $version = [regex]::Match($manifest, '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
    if ($version -notmatch '^\d+\.\d+\.\d+$') { throw 'A stable semantic version is required.' }
    $distribution = Join-Path $projectRoot 'dist'
    New-Item -ItemType Directory -Path $distribution -Force | Out-Null
    $filename = "MicrophoneCleaner-v$version.exe"
    $destination = Join-Path $distribution $filename
    Copy-Item -LiteralPath (Join-Path $projectRoot 'target/release/microphone-cleaner.exe') -Destination $destination
    $checksum = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash.ToLowerInvariant()
    [System.IO.File]::WriteAllText("$destination.sha256", "$checksum  $filename`n", [System.Text.Encoding]::ASCII)
    Copy-Item -LiteralPath (Join-Path $projectRoot 'LICENSE') -Destination (Join-Path $distribution 'LICENSE')
    Copy-Item -LiteralPath (Join-Path $projectRoot 'THIRD_PARTY_NOTICES.txt') -Destination (Join-Path $distribution 'THIRD_PARTY_NOTICES.txt')
    Write-Output "$filename | SHA-256 $checksum"
} finally {
    Pop-Location
}
