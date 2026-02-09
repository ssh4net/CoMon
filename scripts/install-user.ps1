param(
  [string]$Root = "$HOME\.local"
)

$ErrorActionPreference = "Stop"
$repoDir = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$comonHome = if ($env:COMON_HOME -and $env:COMON_HOME.Trim()) {
  $env:COMON_HOME
} else {
  Join-Path $HOME ".comon"
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
  throw "cargo not found in PATH. Install Rust first: https://rustup.rs"
}

if (Test-Path -LiteralPath $comonHome) {
  $homeItem = Get-Item -LiteralPath $comonHome -Force
  if ($homeItem.Attributes -band [IO.FileAttributes]::ReparsePoint) {
    throw "Refusing to use COMON_HOME ($comonHome): symlink/reparse point is not allowed."
  }
  if (-not $homeItem.PSIsContainer) {
    throw "Refusing to use COMON_HOME ($comonHome): expected a directory."
  }
} else {
  New-Item -ItemType Directory -Path $comonHome | Out-Null
}

cargo install --path $repoDir --locked --force --root $Root

$binDir = Join-Path $Root "bin"
Write-Host "Installed comon to $(Join-Path $binDir 'comon.exe')"
Write-Host "Prepared COMON_HOME at $comonHome"

$pathEntries = $env:PATH -split ";"
if (-not ($pathEntries -contains $binDir)) {
  Write-Host "Add to PATH: $binDir"
}
