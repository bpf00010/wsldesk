[CmdletBinding()]
param([uri]$ReleaseUrl)
$ErrorActionPreference = 'Stop'
$bin = Join-Path $env:LOCALAPPDATA 'WSLDesk\bin'
$destination = Join-Path $bin 'wsldesk.exe'
$staged = Join-Path $bin ("wsldesk-{0}.tmp" -f [guid]::NewGuid())
New-Item -ItemType Directory -Path $bin -Force | Out-Null
try {
    if ($ReleaseUrl) {
        if ($ReleaseUrl.Scheme -ne 'https') { throw 'ReleaseUrl must use HTTPS.' }
        Invoke-WebRequest -UseBasicParsing -Uri $ReleaseUrl -OutFile $staged
    } else {
        $source = Join-Path (Split-Path $PSScriptRoot -Parent) 'dist\wsldesk.exe'
        Copy-Item -LiteralPath $source -Destination $staged
        if (Test-Path -LiteralPath "$source.sha256") {
            $expected = ((Get-Content -Raw -LiteralPath "$source.sha256").Trim() -split '\s+')[0]
            if ((Get-FileHash -LiteralPath $staged).Hash -ne $expected) { throw 'Release checksum mismatch.' }
        }
    }
    if (Test-Path -LiteralPath $destination) {
        [IO.File]::Replace($staged, $destination, $null)
    } else {
        [IO.File]::Move($staged, $destination)
    }
} finally {
    if (Test-Path -LiteralPath $staged) { Remove-Item -LiteralPath $staged }
}
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$entries = @($userPath -split ';' | Where-Object { $_ })
if ($bin -notin ($entries | ForEach-Object { [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\') })) {
    [Environment]::SetEnvironmentVariable('Path', (($entries + $bin) -join ';'), 'User')
}
if ($bin -notin ($env:Path -split ';')) { $env:Path += ";$bin" }
Write-Host '1. wsldesk doctor'
Write-Host '2. wsldesk setup --install-desktop'
Write-Host '3. wsldesk up'
