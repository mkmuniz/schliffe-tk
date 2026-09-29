# Schliffe installer — native Windows (specs.md §5.1, M8).
#
# Downloads the verified prebuilt binary, or builds from source (see below).
# Symlinks are
# deliberately not used — would need developer mode/admin on Windows; a
# plain copy works just as well, since schliffe decides what to filter by the
# file's NAME (argv[0]), not by whether it's a link or a copy.
param(
    # Download the latest release's prebuilt binary (verified against
    # SHA256SUMS) even inside a clone with Rust installed.
    [switch]$Prebuilt,
    # Always build from source (needs a clone and Rust).
    [switch]$FromSource
)
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"  # Invoke-WebRequest is much faster without it

# M11 (2026-09-27): same rules as install.sh.
#   irm https://raw.githubusercontent.com/mkmuniz/schliffe-tk/main/install.ps1 | iex
#       -> downloads the verified prebuilt binary (no Rust needed)
#   ./install.ps1 inside a clone with Rust -> builds that checkout's code
#   ./install.ps1 -Prebuilt / -FromSource  -> force one or the other
# A downloaded archive is installed only if its SHA-256 matches the
# release's SHA256SUMS; it never installs an unverified binary.
$Repo = "mkmuniz/schliffe-tk"

$ShimsDir = if ($env:SCHLIFFE_SHIMS_DIR) { $env:SCHLIFFE_SHIMS_DIR } else { Join-Path $env:USERPROFILE ".schliffe\shims" }
$BinDir = if ($env:SCHLIFFE_BIN_DIR) { $env:SCHLIFFE_BIN_DIR } else { Join-Path $env:USERPROFILE ".schliffe\bin" }

# Same list as install.sh (Layer A: git/pytest/cargo; Layer B:
# docker/npm/terraform) — keep both in sync if the list changes.
$DefaultCommands = @("git", "cargo", "pytest", "docker", "npm", "pnpm", "yarn", "pip", "pip3", "dotnet", "go", "terraform")

# Empty when piped into iex (no checkout on disk).
$HaveCheckout = $PSScriptRoot -and (Test-Path (Join-Path $PSScriptRoot "Cargo.toml"))
$HaveCargo = [bool](Get-Command cargo -ErrorAction SilentlyContinue)

function Get-Prebuilt {
    $tag = $env:SCHLIFFE_VERSION
    # github.com/<repo>/releases/latest redirects to .../releases/tag/<tag>;
    # preferred over api.github.com, which rate-limits unauthenticated
    # requests per IP (403 on shared CI runners).
    if (-not $tag) {
        try {
            $resp = Invoke-WebRequest -UseBasicParsing -Method Head -Uri "https://github.com/$Repo/releases/latest"
            $final = if ($resp.BaseResponse.ResponseUri) { $resp.BaseResponse.ResponseUri.AbsoluteUri } else { $resp.BaseResponse.RequestMessage.RequestUri.AbsoluteUri }
            if ($final -match '/releases/tag/([^/?]+)') { $tag = $Matches[1] }
        } catch { }
    }
    if (-not $tag) {
        $tag = (Invoke-RestMethod -UseBasicParsing "https://api.github.com/repos/$Repo/releases/latest").tag_name
    }
    if (-not $tag) { throw "couldn't find the latest release on GitHub" }
    $triple = "x86_64-pc-windows-msvc"
    $name = "schliffe-$tag-$triple.zip"
    $base = if ($env:SCHLIFFE_DOWNLOAD_BASE) { $env:SCHLIFFE_DOWNLOAD_BASE } else { "https://github.com/$Repo/releases/download/$tag" }
    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("schliffe-" + [guid]::NewGuid())
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
    Write-Host "schliffe: downloading $name..."
    Invoke-WebRequest -UseBasicParsing -Uri "$base/$name" -OutFile (Join-Path $tmp $name)
    Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS" -OutFile (Join-Path $tmp "SHA256SUMS")
    $expected = $null
    foreach ($line in Get-Content (Join-Path $tmp "SHA256SUMS")) {
        $parts = $line -split '\s+'
        if ($parts.Length -ge 2 -and $parts[1] -eq $name) { $expected = $parts[0].ToLower() }
    }
    $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $tmp $name)).Hash.ToLower()
    if (-not $expected -or $expected -ne $actual) {
        throw "checksum verification FAILED for $name - not installing it"
    }
    Expand-Archive -Path (Join-Path $tmp $name) -DestinationPath $tmp -Force
    $exe = Join-Path $tmp "schliffe-$tag-$triple\schliffe.exe"
    $version = & $exe --version
    Write-Host "schliffe: verified $version (SHA-256 matches the release)"
    return $exe
}

function Build-FromSource {
    if (-not $HaveCheckout) { throw "building from source needs a clone: git clone https://github.com/$Repo.git" }
    if (-not $HaveCargo) { throw "building from source needs Rust - https://rustup.rs" }
    Write-Host "schliffe: building (cargo build --release)..."
    Push-Location $PSScriptRoot
    try { cargo build --release } finally { Pop-Location }
    $exe = Join-Path $PSScriptRoot "target\release\schliffe.exe"
    if (-not (Test-Path $exe)) { throw "build finished but $exe is missing" }
    return $exe
}

if ($FromSource) {
    $SchliffeExe = Build-FromSource
} elseif ($Prebuilt -or -not ($HaveCheckout -and $HaveCargo)) {
    if ($HaveCheckout -and -not $Prebuilt) {
        Write-Host "schliffe: Rust not found - installing the latest release instead of this checkout's code"
    }
    try {
        $SchliffeExe = Get-Prebuilt
    } catch {
        Write-Host "schliffe: $($_.Exception.Message)"
        Write-Host "schliffe: falling back to building from source"
        $SchliffeExe = Build-FromSource
    }
} else {
    $SchliffeExe = Build-FromSource
}

# A copy outside the checkout (a `cargo clean` must not break the shims).
New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
$installed = Join-Path $BinDir "schliffe.exe"
Copy-Item -Path $SchliffeExe -Destination $installed -Force
$SchliffeExe = $installed

Write-Host "schliffe: using binary at $SchliffeExe"

New-Item -ItemType Directory -Force -Path $ShimsDir | Out-Null
foreach ($cmd in $DefaultCommands + @("schliffe")) {
    $dest = Join-Path $ShimsDir "$cmd.exe"
    Copy-Item -Path $SchliffeExe -Destination $dest -Force
}
Write-Host "schliffe: shims created in $ShimsDir for: $($DefaultCommands -join ', ')"

$currentUserPath = [Environment]::GetEnvironmentVariable("Path", "User")
$pathEntries = @()
if ($currentUserPath) { $pathEntries = $currentUserPath.Split(";") }

if ($pathEntries -contains $ShimsDir) {
    Write-Host "schliffe: user PATH already has $ShimsDir (nothing to do)"
} else {
    $newPath = if ($currentUserPath) { "$ShimsDir;$currentUserPath" } else { $ShimsDir }
    [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
    Write-Host "schliffe: $ShimsDir added to the user PATH (persistent)"
}

Write-Host ""
Write-Host "schliffe: note - the Claude Code hook (remote MCP servers like Figma, image"
Write-Host "        resizing) is NOT installed: hook output replacement doesn't work on"
Write-Host "        native Windows. Use Schliffe from WSL to get it."
Write-Host "schliffe: installed. Open a NEW terminal to pick up the updated PATH."
Write-Host "schliffe: test with 'git status | more' or any pipe -- if it filters, it worked."
