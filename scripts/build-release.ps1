$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
Push-Location $repo
try {
    cargo build --locked -p wsl-guest --target x86_64-unknown-linux-musl --release
    if ($LASTEXITCODE) { throw 'Guest release build failed.' }
    cargo build --locked -p win-host --release --features embed-guest
    if ($LASTEXITCODE) { throw 'Host release build failed.' }
    New-Item -ItemType Directory -Path dist -Force | Out-Null
    Copy-Item -LiteralPath target/release/wsldesk.exe -Destination dist/wsldesk.exe -Force
    $hash = (Get-FileHash -LiteralPath dist/wsldesk.exe -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText((Join-Path $repo 'dist/wsldesk.exe.sha256'), "$hash  wsldesk.exe`n")
    Get-Item -LiteralPath dist/wsldesk.exe,dist/wsldesk.exe.sha256
} finally {
    Pop-Location
}
