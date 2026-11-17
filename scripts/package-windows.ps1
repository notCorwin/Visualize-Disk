[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [ValidateSet('x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc')]
    [string]$Target = 'x86_64-pc-windows-msvc'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($env:OS -ne 'Windows_NT') {
    throw 'This package requires Windows and the MSVC build tools.'
}

$Root = Split-Path -Parent $PSScriptRoot
$RustflagsName = 'CARGO_TARGET_' + $Target.Replace('-', '_').ToUpperInvariant() + '_RUSTFLAGS'
$PreviousRustflags = [Environment]::GetEnvironmentVariable($RustflagsName, 'Process')
Push-Location $Root
try {
    if (-not $SkipBuild) {
        # The portable package must run without a separate VC runtime install.
        if ($PreviousRustflags -notmatch 'target-feature=\+crt-static') {
            [Environment]::SetEnvironmentVariable(
                $RustflagsName, "$PreviousRustflags -C target-feature=+crt-static".Trim(), 'Process')
        }
        & cargo build --release --locked --target $Target
        if ($LASTEXITCODE -ne 0) { throw 'Windows release build failed.' }
    }

    $MetadataJson = & cargo metadata --no-deps --format-version 1 --locked
    if ($LASTEXITCODE -ne 0) { throw 'Cannot read Cargo build metadata.' }
    $Metadata = $MetadataJson | ConvertFrom-Json
    $Package = $Metadata.packages | Where-Object { $_.name -eq 'visualize-ssd' }
    $Binary = Join-Path $Metadata.target_directory "$Target/release/visualize-ssd.exe"
    if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) {
        throw "Release executable is missing: $Binary"
    }

    $Architecture = if ($Target.StartsWith('aarch64-')) { 'arm64' } else { 'x64' }
    $BundleName = "Visualize-SSD-$($Package.version)-$Architecture.exe"
    $BundleRoot = Join-Path $Metadata.target_directory 'release/bundle'
    New-Item -ItemType Directory -Force -Path $BundleRoot | Out-Null
    $Executable = Join-Path $BundleRoot $BundleName
    Copy-Item -LiteralPath $Binary -Destination $Executable -Force
    Write-Output "Packaged: $Executable"
}
finally {
    [Environment]::SetEnvironmentVariable($RustflagsName, $PreviousRustflags, 'Process')
    Pop-Location
}
