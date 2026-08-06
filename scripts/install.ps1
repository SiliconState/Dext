[CmdletBinding()]
param(
    [string]$Version = $(if ($env:DEXT_VERSION) { $env:DEXT_VERSION } else { "latest" }),
    [string]$InstallDir = $(
        if ($env:DEXT_INSTALL_DIR) { $env:DEXT_INSTALL_DIR }
        elseif ($env:LOCALAPPDATA) { Join-Path $env:LOCALAPPDATA "Dext\bin" }
        else { Join-Path $HOME ".local\bin" }
    ),
    [switch]$NoSourceFallback,
    [switch]$RequireAttestation,
    [switch]$Help
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
$repository = "https://github.com/SiliconState/Dext"
$apiUrl = "https://api.github.com/repos/SiliconState/Dext/releases/latest"
$mainCommitUrl = "https://api.github.com/repos/SiliconState/Dext/git/ref/heads/main"

if ($Help) {
    @"
Install Dext for the current Windows user.

Usage: install.ps1 [-Version vX.Y.Z] [-InstallDir DIR] [-NoSourceFallback] [-RequireAttestation]

Environment:
  DEXT_VERSION              Release tag to install (default: latest)
  DEXT_INSTALL_DIR          Binary directory (default: %LOCALAPPDATA%\Dext\bin)
  DEXT_SOURCE_FALLBACK      Set to 0 to fail when no tagged release exists
  DEXT_REQUIRE_ATTESTATION  Set to 1 to require GitHub CLI provenance verification
"@ | Write-Host
    return
}

foreach ($setting in @("DEXT_SOURCE_FALLBACK", "DEXT_REQUIRE_ATTESTATION")) {
    $value = [Environment]::GetEnvironmentVariable($setting)
    if ($null -ne $value -and $value -notin @("0", "1")) {
        throw "$setting must be 0 or 1"
    }
}

$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("dext-install-" + [Guid]::NewGuid().ToString("N"))

function Write-Install([string]$Message) {
    Write-Host "dext-install: $Message"
}

function Get-Sha256([string]$Path) {
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Install-DextBinary([string]$Source) {
    if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) {
        throw "installer did not produce a regular Dext binary"
    }
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $destination = Join-Path $InstallDir "dext.exe"
    $staged = Join-Path $InstallDir (".dext-install-" + [Guid]::NewGuid().ToString("N") + ".exe")
    try {
        Copy-Item -LiteralPath $Source -Destination $staged
        Move-Item -LiteralPath $staged -Destination $destination -Force
    }
    finally {
        Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue
    }
}

function Get-LatestTag {
    try {
        $release = Invoke-RestMethod -Uri $apiUrl -Headers @{
            Accept = "application/vnd.github+json"
            "User-Agent" = "dext-installer"
        }
        $tag = [string]$release.tag_name
        if ([string]::IsNullOrWhiteSpace($tag)) {
            throw "latest release response did not contain a tag"
        }
        return $tag
    }
    catch {
        $response = $_.Exception.Response
        if ($null -ne $response -and [int]$response.StatusCode -eq 404) {
            return $null
        }
        throw "could not query the latest GitHub release: $($_.Exception.Message)"
    }
}

function Get-MainCommit {
    try {
        $commit = Invoke-RestMethod -Uri $mainCommitUrl -Headers @{
            Accept = "application/vnd.github+json"
            "User-Agent" = "dext-installer"
        }
    }
    catch {
        throw "could not resolve the current main commit: $($_.Exception.Message)"
    }
    if ([string]$commit.object.type -ne "commit") {
        throw "main ref does not point to a commit"
    }
    $sha = ([string]$commit.object.sha).ToLowerInvariant()
    if ($sha -notmatch '^[0-9a-f]{40}$') {
        throw "main commit response is malformed"
    }
    return $sha
}

function Install-Release([string]$Tag) {
    $archive = "dext-$Tag-x86_64-pc-windows-msvc.zip"
    $archivePath = Join-Path $tempDir $archive
    $checksumsPath = Join-Path $tempDir "SHA256SUMS"
    $base = "$repository/releases/download/$Tag"
    Write-Install "downloading $archive"
    Invoke-WebRequest -Uri "$base/$archive" -OutFile $archivePath
    Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile $checksumsPath

    $expectedDigests = @()
    foreach ($line in Get-Content -LiteralPath $checksumsPath) {
        if ($line -match '^([0-9A-Fa-f]{64})\s+(.+)$' -and $Matches[2] -eq $archive) {
            $expectedDigests += $Matches[1].ToLowerInvariant()
        }
    }
    if ($expectedDigests.Count -ne 1) {
        throw "SHA256SUMS does not contain exactly one entry for $archive"
    }
    if ((Get-Sha256 $archivePath) -ne $expectedDigests[0]) {
        throw "checksum verification failed for $archive"
    }
    if ($RequireAttestation -or $env:DEXT_REQUIRE_ATTESTATION -eq "1") {
        $gh = Get-Command gh -ErrorAction SilentlyContinue
        if ($null -eq $gh) {
            throw "GitHub CLI is required by RequireAttestation"
        }
        & $gh.Source attestation verify $archivePath --repo "SiliconState/Dext" | Out-Null
        if ($LASTEXITCODE -ne 0) {
            throw "GitHub build-provenance verification failed for $archive"
        }
        Write-Install "verified release checksum and GitHub build provenance"
    }
    else {
        Write-Install "verified release checksum"
    }

    $unpacked = Join-Path $tempDir "unpacked"
    New-Item -ItemType Directory -Path $unpacked | Out-Null
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [System.IO.Compression.ZipFile]::OpenRead($archivePath)
    try {
        $binaryEntries = @()
        foreach ($entry in $zip.Entries) {
            $name = $entry.FullName.Replace('\', '/')
            if ($name.StartsWith('/') -or $name -match '(^|/)\.\.(/|$)') {
                throw "release archive has an unsafe layout"
            }
            if ($name -eq "dext.exe") {
                $binaryEntries += $entry
            }
        }
        if ($binaryEntries.Count -ne 1) {
            throw "release archive does not contain exactly one root dext.exe"
        }
        $unpackedBinary = Join-Path $unpacked "dext.exe"
        [System.IO.Compression.ZipFileExtensions]::ExtractToFile(
            $binaryEntries[0],
            $unpackedBinary,
            $false
        )
    }
    finally {
        $zip.Dispose()
    }
    Install-DextBinary $unpackedBinary
    Write-Install "verified and installed release $Tag"
}

function Install-Source {
    if ($RequireAttestation -or $env:DEXT_REQUIRE_ATTESTATION -eq "1") {
        throw "attestation verification requires a tagged release; source fallback is disabled"
    }
    $cargo = Get-Command cargo -ErrorAction SilentlyContinue
    if ($null -eq $cargo) {
        throw "no tagged release exists yet and Rust/Cargo is unavailable; install Rust or pass -Version for an existing release"
    }
    $commit = Get-MainCommit
    Write-Install "building Dext from main commit $commit with Cargo"
    $cargoRoot = Join-Path $tempDir "cargo-root"
    & $cargo.Source install --git "$repository.git" --rev $commit --locked --root $cargoRoot dext
    if ($LASTEXITCODE -ne 0) {
        throw "Cargo source installation failed"
    }
    Install-DextBinary (Join-Path $cargoRoot "bin\dext.exe")
}

try {
    if (-not [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
        [System.Runtime.InteropServices.OSPlatform]::Windows
    )) {
        throw "this installer is for Windows; use scripts/install.sh on Linux or macOS"
    }
    if (-not [Environment]::Is64BitOperatingSystem) {
        throw "Dext supports 64-bit Windows"
    }
    $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
    if ($arch -ne "X64") {
        throw "no Dext release archive matches Windows/$arch"
    }
    New-Item -ItemType Directory -Path $tempDir | Out-Null

    $tag = $null
    if ($Version -eq "latest") {
        $tag = Get-LatestTag
        if ([string]::IsNullOrWhiteSpace($tag)) {
            if ($NoSourceFallback -or $env:DEXT_SOURCE_FALLBACK -eq "0") {
                throw "no tagged Dext release exists yet"
            }
            Install-Source
        }
    }
    else {
        $tag = if ($Version.StartsWith("v")) { $Version } else { "v$Version" }
    }

    if (-not [string]::IsNullOrWhiteSpace($tag)) {
        if ($tag -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+$') {
            throw "release version must have form vX.Y.Z"
        }
        Install-Release $tag
    }

    $binary = Join-Path $InstallDir "dext.exe"
    & $binary --version
    if ($LASTEXITCODE -ne 0) {
        throw "installed Dext binary did not start"
    }
    $pathParts = @($env:PATH -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $userParts = @($userPath -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    if ($userParts -notcontains $InstallDir) {
        $nextUserPath = (@($userParts) + $InstallDir) -join ';'
        try {
            [Environment]::SetEnvironmentVariable("Path", $nextUserPath, "User")
            Write-Install "added $InstallDir to your user PATH; open a new shell to use it there"
        }
        catch {
            Write-Warning "Dext was installed, but user PATH could not be updated: $($_.Exception.Message)"
            Write-Install "add $InstallDir to PATH to run dext from a new shell"
        }
    }
    if ($pathParts -notcontains $InstallDir) {
        $env:PATH = "$InstallDir;$($env:PATH)"
    }
    Write-Install "done"
}
finally {
    Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
}
