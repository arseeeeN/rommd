# Install or upgrade rommd on Windows, and start it at login.
#
#   irm https://github.com/arseeeeN/rommd/releases/latest/download/install.ps1 | iex
#
# Environment: ROMMD_NO_SERVICE to skip starting at login, ROMMD_REPO to
# install from another GitHub owner/name.
$ErrorActionPreference = 'Stop'

$repo = if ($env:ROMMD_REPO) { $env:ROMMD_REPO } else { 'arseeeeN/rommd' }
$asset = 'rommd-x86_64-pc-windows-msvc.exe'
$base = "https://github.com/$repo/releases/latest/download"
$dir = Join-Path $env:LOCALAPPDATA 'Programs\rommd'
$exe = Join-Path $dir 'rommd.exe'
$tmp = Join-Path ([IO.Path]::GetTempPath()) "rommd-install-$PID"
New-Item -ItemType Directory -Force -Path $tmp, $dir | Out-Null

try {
    Write-Host "Downloading $asset ..."
    Invoke-WebRequest -UseBasicParsing "$base/$asset" -OutFile "$tmp\$asset"
    Invoke-WebRequest -UseBasicParsing "$base/SHA256SUMS" -OutFile "$tmp\SHA256SUMS"

    $line = Get-Content "$tmp\SHA256SUMS" | Where-Object { $_ -match " $([regex]::Escape($asset))$" }
    $expected = ($line -split ' ')[0]
    $actual = (Get-FileHash -Algorithm SHA256 "$tmp\$asset").Hash.ToLower()
    if (-not $expected -or $expected -ne $actual) { throw "checksum mismatch for $asset; not installing" }

    # A running rommd locks its binary; stop it before replacing.
    if (Get-ScheduledTask -TaskName rommd -ErrorAction SilentlyContinue) { Stop-ScheduledTask -TaskName rommd }
    Get-Process rommd -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe } | Stop-Process -Force
    Start-Sleep -Milliseconds 500
    Move-Item -Force "$tmp\$asset" $exe
    Write-Host "Installed $(& $exe --version) to $exe"

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (($userPath -split ';') -notcontains $dir) {
        [Environment]::SetEnvironmentVariable('Path', "$userPath;$dir", 'User')
        Write-Host "Added $dir to your PATH (open a new terminal to use it)."
    }

    if (-not $env:ROMMD_NO_SERVICE) { & $exe service install }
    Write-Host "Configure rommd in $env:APPDATA\rommd\config.toml (created on first start)."
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
