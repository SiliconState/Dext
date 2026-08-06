[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$DextBinary
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
$installer = Join-Path $PSScriptRoot "install.ps1"
$DextBinary = (Resolve-Path -LiteralPath $DextBinary).Path
$versionOutput = ((& $DextBinary --version) -join "`n").Trim()
if ($LASTEXITCODE -ne 0 -or $versionOutput -notmatch '^dext ([^\s]+)$') {
    throw "test binary did not report a valid Dext version: '$versionOutput'"
}
$version = $Matches[1]
$tag = "v$version"
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("dext-install-test-" + [Guid]::NewGuid().ToString("N"))
$package = Join-Path $work "package"
$wrongPackage = Join-Path $work "wrong-package"
$archive = Join-Path $work "dext.zip"
$wrongArchive = Join-Path $work "dext-wrong-version.zip"
$installDir = Join-Path $work "install"
$mockBin = Join-Path $work "bin"
$oldProcessPath = $env:PATH
$oldUserPath = [Environment]::GetEnvironmentVariable("Path", "User")
$oldSettings = @{}
foreach ($name in @(
    "DEXT_INSTALL_DIR",
    "DEXT_VERSION",
    "DEXT_SOURCE_FALLBACK",
    "DEXT_REQUIRE_ATTESTATION",
    "MOCK_CARGO_ARGS",
    "DEXT_TEST_BINARY"
)) {
    $oldSettings[$name] = [Environment]::GetEnvironmentVariable($name)
    [Environment]::SetEnvironmentVariable($name, $null)
}

function New-MockHttpException([int]$StatusCode) {
    $exception = [System.Exception]::new("mock HTTP $StatusCode")
    $exception | Add-Member -MemberType NoteProperty -Name Response -Value (
        [pscustomobject]@{ StatusCode = $StatusCode }
    )
    return $exception
}

$global:DextInstallerMockMode = "release"
$global:DextInstallerMockTag = $tag
$global:DextInstallerMockArchive = $archive
$global:DextInstallerMockWrongArchive = $wrongArchive

function global:Invoke-RestMethod {
    param(
        [string]$Uri,
        [hashtable]$Headers
    )
    if ($Uri -like "*/releases/latest") {
        switch ($global:DextInstallerMockMode) {
            "source" { throw (New-MockHttpException 404) }
            "bad-ref" { throw (New-MockHttpException 404) }
            "attested-source" { throw (New-MockHttpException 404) }
            "malformed-release" { return [pscustomobject]@{ name = "missing tag" } }
            default { return [pscustomobject]@{ tag_name = $global:DextInstallerMockTag } }
        }
    }
    if ($Uri -like "*/git/ref/heads/main") {
        if ($global:DextInstallerMockMode -eq "bad-ref") {
            return [pscustomobject]@{
                ref = "refs/heads/main"
                object = [pscustomobject]@{
                    type = "tag"
                    sha = "0123456789abcdef0123456789abcdef01234567"
                }
            }
        }
        return [pscustomobject]@{
            ref = "refs/heads/main"
            object = [pscustomobject]@{
                type = "commit"
                sha = "0123456789ABCDEF0123456789ABCDEF01234567"
            }
        }
    }
    throw "unexpected REST URL: $Uri"
}

function global:Invoke-WebRequest {
    param(
        [string]$Uri,
        [string]$OutFile,
        [switch]$UseBasicParsing
    )
    if ($Uri -like "*/SHA256SUMS") {
        $archiveName = "dext-$($global:DextInstallerMockTag)-x86_64-pc-windows-msvc.zip"
        $digest = if ($global:DextInstallerMockMode -eq "bad-checksum") {
            "0" * 64
        }
        else {
            $sourceArchive = if ($global:DextInstallerMockTag -eq "v9.9.9") {
                $global:DextInstallerMockWrongArchive
            }
            else {
                $global:DextInstallerMockArchive
            }
            (Get-FileHash -LiteralPath $sourceArchive -Algorithm SHA256).Hash
        }
        Set-Content -LiteralPath $OutFile -Value "$digest  $archiveName" -Encoding ASCII
        return
    }
    if ($Uri -like "*/dext-*-x86_64-pc-windows-msvc.zip") {
        $sourceArchive = if ($global:DextInstallerMockTag -eq "v9.9.9") {
            $global:DextInstallerMockWrongArchive
        }
        else {
            $global:DextInstallerMockArchive
        }
        Copy-Item -LiteralPath $sourceArchive -Destination $OutFile
        return
    }
    throw "unexpected download URL: $Uri"
}

function Invoke-InstallerCase {
    param(
        [string]$Mode,
        [string]$RequestedTag = "latest",
        [switch]$RequireAttestation
    )
    $global:DextInstallerMockMode = $Mode
    $global:DextInstallerMockTag = if ($RequestedTag -eq "latest") { $tag } else { $RequestedTag }
    $parameters = @{
        Version = $RequestedTag
        InstallDir = $installDir
        RequireAttestation = [bool]$RequireAttestation
    }
    & $installer @parameters
}

function Assert-Fails {
    param(
        [scriptblock]$Action,
        [string]$Message
    )
    try {
        & $Action | Out-Null
    }
    catch {
        if ($_.Exception.Message -notlike "*$Message*") {
            throw "expected failure containing '$Message', got '$($_.Exception.Message)'"
        }
        return
    }
    throw "expected installer failure containing '$Message'"
}

try {
    New-Item -ItemType Directory -Force -Path $package, $wrongPackage, $mockBin, $installDir | Out-Null
    Copy-Item -LiteralPath $DextBinary -Destination (Join-Path $package "dext.exe")
    $wrongBinary = Join-Path $wrongPackage "dext.exe"
    Copy-Item -LiteralPath $DextBinary -Destination $wrongBinary
    $stream = [System.IO.File]::Open($wrongBinary, [System.IO.FileMode]::Append)
    try {
        $stream.WriteByte(0)
    }
    finally {
        $stream.Dispose()
    }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [System.IO.Compression.ZipFile]::CreateFromDirectory($package, $archive)
    [System.IO.Compression.ZipFile]::CreateFromDirectory($wrongPackage, $wrongArchive)

    $cargo = @'
@echo off
setlocal
> "%MOCK_CARGO_ARGS%" echo %*
:scan
if "%~1"=="" goto missing
if "%~1"=="--root" goto found
shift
goto scan
:found
set "ROOT=%~2"
mkdir "%ROOT%\bin" 2>nul
copy /Y "%DEXT_TEST_BINARY%" "%ROOT%\bin\dext.exe" >nul
exit /b 0
:missing
exit /b 2
'@
    Set-Content -LiteralPath (Join-Path $mockBin "cargo.cmd") -Value $cargo -Encoding ASCII
    $env:MOCK_CARGO_ARGS = Join-Path $work "cargo-args.txt"
    $env:DEXT_TEST_BINARY = $DextBinary
    $env:PATH = "$installDir;$mockBin;$oldProcessPath"

    Invoke-InstallerCase -Mode "release" | Out-Null
    $installed = Join-Path $installDir "dext.exe"
    if (((& $installed --version) -join "`n").Trim() -ne "dext $version") {
        throw "release install did not preserve the expected version"
    }
    $userPathAfterInstall = [Environment]::GetEnvironmentVariable("Path", "User")
    $userPathParts = @($userPathAfterInstall -split ';' | Where-Object {
        -not [string]::IsNullOrWhiteSpace($_)
    })
    if ($userPathParts -notcontains $installDir) {
        throw "installer did not persist the install directory in the user PATH"
    }
    $installedDigest = (Get-FileHash -LiteralPath $installed -Algorithm SHA256).Hash

    Assert-Fails -Message "checksum verification failed" -Action {
        Invoke-InstallerCase -Mode "bad-checksum"
    }
    if ((Get-FileHash -LiteralPath $installed -Algorithm SHA256).Hash -ne $installedDigest) {
        throw "checksum failure replaced the existing installation"
    }

    Invoke-InstallerCase -Mode "source" | Out-Null
    $cargoArgs = Get-Content -LiteralPath $env:MOCK_CARGO_ARGS -Raw
    if ($cargoArgs -notlike "*--rev 0123456789abcdef0123456789abcdef01234567 --locked*") {
        throw "source fallback did not pin the resolved main commit: $cargoArgs"
    }

    Assert-Fails -Message "latest release response did not contain a tag" -Action {
        Invoke-InstallerCase -Mode "malformed-release"
    }
    Assert-Fails -Message "main ref does not point to a commit" -Action {
        Invoke-InstallerCase -Mode "bad-ref"
    }
    Assert-Fails -Message "attestation verification requires a tagged release" -Action {
        Invoke-InstallerCase -Mode "attested-source" -RequireAttestation
    }
    Assert-Fails -Message "release binary reported" -Action {
        Invoke-InstallerCase -Mode "release" -RequestedTag "v9.9.9"
    }
    Assert-Fails -Message "InstallDir must be a non-empty directory" -Action {
        & $installer -Version latest -InstallDir ""
    }
    if ((Get-FileHash -LiteralPath $installed -Algorithm SHA256).Hash -ne $installedDigest) {
        throw "a failed installer path replaced the existing installation"
    }

    Write-Host "Windows installer tests passed"
}
finally {
    Remove-Item Function:\Invoke-RestMethod -ErrorAction SilentlyContinue
    Remove-Item Function:\Invoke-WebRequest -ErrorAction SilentlyContinue
    Remove-Variable DextInstallerMockMode -Scope Global -ErrorAction SilentlyContinue
    Remove-Variable DextInstallerMockTag -Scope Global -ErrorAction SilentlyContinue
    Remove-Variable DextInstallerMockArchive -Scope Global -ErrorAction SilentlyContinue
    Remove-Variable DextInstallerMockWrongArchive -Scope Global -ErrorAction SilentlyContinue
    $env:PATH = $oldProcessPath
    [Environment]::SetEnvironmentVariable("Path", $oldUserPath, "User")
    foreach ($entry in $oldSettings.GetEnumerator()) {
        [Environment]::SetEnvironmentVariable([string]$entry.Key, $entry.Value)
    }
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
