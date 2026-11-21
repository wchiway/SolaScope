#requires -Version 5.1
[CmdletBinding()]
param(
    [string]$Version,
    [string]$InstallDir,
    [switch]$NoPath,
    [string]$ArchivePath,
    [string]$ChecksumPath,
    [string]$ExpectedSha256
)

# Dot-source this file to load helpers without running an installation.
$script:SolaScopePublicFiles = @(
    'solascope.exe', 'sas.exe', 'LICENSE', 'README.md', 'README.zh-CN.md',
    'docs/configuration.md', 'docs/mcp.md', 'docs/architecture.md',
    'docs/installation.md', 'docs/releases.md',
    'docs/benchmarks.md', 'docs/implementation-status.md', 'benchmarks/README.md',
    'docs/assets/solascope-overview.png', 'docs/assets/solascope-search-flow.png',
    'install.ps1', 'install.cmd'
)
$script:SolaScopeMarker = '.solascope-install.json'
$script:SolaScopeSemVer = '(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*))*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?'

function Get-SolaScopeVersion([string]$Value) {
    if ($Value -cnotmatch ('^v?' + $script:SolaScopeSemVer + '$')) {
        throw "Invalid version '$Value'; use a SemVer version, optionally prefixed with v."
    }
    return $Value -creplace '^v', ''
}

function Assert-SolaScopePlatform {
    if ($env:OS -ne 'Windows_NT' -or [Environment]::OSVersion.Version.Major -lt 10) {
        throw 'SolaScope requires Windows 10/11 x64 and Windows PowerShell 5.1 or later.'
    }
    if (-not ('SolaScopeInstallerNative' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class SolaScopeInstallerNative {
    [DllImport("kernel32.dll")] static extern void GetNativeSystemInfo(IntPtr info);
    public static short Architecture() {
        IntPtr info = Marshal.AllocHGlobal(64);
        try { GetNativeSystemInfo(info); return Marshal.ReadInt16(info); }
        finally { Marshal.FreeHGlobal(info); }
    }
}
'@
    }
    if ([SolaScopeInstallerNative]::Architecture() -ne 9) {
        throw 'Only native x64 Windows is supported; ARM64 and x86 are not supported.'
    }
}

function Assert-SolaScopeLocalPath([string]$Path) {
    if ($Path -notmatch '^[A-Za-z]:[\\/]') { throw 'Installer paths must use a local drive, not UNC or relative paths.' }
    $drive = New-Object IO.DriveInfo ([IO.Path]::GetPathRoot($Path))
    if ($drive.DriveType -ne [IO.DriveType]::Fixed -and $drive.DriveType -ne [IO.DriveType]::Removable) {
        throw 'Installer paths must use a local fixed or removable drive.'
    }
}

function Assert-SolaScopeNoReparse([string]$Path) {
    Assert-SolaScopeLocalPath $Path
    $cursor = [IO.Path]::GetFullPath($Path)
    while ($cursor) {
        if (Test-Path -LiteralPath $cursor) {
            $item = Get-Item -LiteralPath $cursor -Force -ErrorAction Stop
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Reparse points are not allowed in installer paths: $cursor"
            }
        }
        $parent = [IO.Path]::GetDirectoryName($cursor)
        if ($parent -eq $cursor) { break }
        $cursor = $parent
    }
}

function Test-SolaScopeWithin([string]$Path, [string]$Directory) {
    $base = $Directory.TrimEnd('\')
    return $Path.Equals($base, [StringComparison]::OrdinalIgnoreCase) -or
        $Path.StartsWith($base + '\', [StringComparison]::OrdinalIgnoreCase)
}

function Get-SolaScopeDestination([string]$Path) {
    if ($Path -notmatch '^[A-Za-z]:[\\/]' -or $Path -match '[\x00-\x1f"<>|?*;]') {
        throw 'InstallDir must be an absolute local-drive directory without wildcards or semicolons.'
    }
    # Reject Win32 aliases before GetFullPath normalizes them.
    foreach ($part in ($Path.Substring(3) -split '[\\/]')) {
        if ($part -eq '.' -or $part -eq '..' -or $part -match '[ .]$|:') {
            throw 'InstallDir must not contain dot segments, trailing dots/spaces, or alternate streams.'
        }
        if ($part -match '^(?i:CON|PRN|AUX|NUL|COM[1-9\u00b9\u00b2\u00b3]|LPT[1-9\u00b9\u00b2\u00b3])(?:\.|$)') {
            throw "Reserved Windows directory name: $part"
        }
    }
    $full = [IO.Path]::GetFullPath($Path).TrimEnd('\')
    $protected = @(
        [IO.Path]::GetPathRoot($full), $env:USERPROFILE, $env:LOCALAPPDATA,
        $env:APPDATA, [IO.Path]::GetTempPath(), $env:PUBLIC,
        [Environment]::GetFolderPath('Desktop'), [Environment]::GetFolderPath('MyDocuments')
    )
    foreach ($directory in $protected) {
        if ($directory -and (Test-SolaScopeWithin $directory.TrimEnd('\') $full)) {
            throw "Refusing a root, broad, or user-data directory: $full"
        }
    }
    foreach ($directory in @($env:SystemRoot, $env:ProgramFiles, ${env:ProgramFiles(x86)}, $env:ProgramData,
            (Join-Path $env:USERPROFILE '.solascope'))) {
        if ($directory -and ((Test-SolaScopeWithin $full $directory) -or
                (Test-SolaScopeWithin $directory $full))) {
            throw "Refusing a system or configuration directory: $full"
        }
    }
    Assert-SolaScopeNoReparse $full
    return $full
}

function Assert-SolaScopeOwned([string]$Path) {
    Assert-SolaScopeNoReparse $Path
    if (-not (Test-Path -LiteralPath $Path)) { return }
    if (-not (Test-Path -LiteralPath $Path -PathType Container)) { throw "Not a directory: $Path" }
    $markerPath = Join-Path $Path $script:SolaScopeMarker
    Assert-SolaScopeNoReparse $markerPath
    if (-not (Test-Path -LiteralPath $markerPath -PathType Leaf)) {
        throw "Existing InstallDir is not installer-owned. Choose a new directory: $Path"
    }
    $marker = Get-Content -LiteralPath $markerPath -Raw | ConvertFrom-Json
    if ($marker.format -ne 'SolaScope installer 1' -or $marker.directory -ine $Path) {
        throw "Invalid installation ownership marker: $markerPath"
    }
    $allowed = @($script:SolaScopePublicFiles | Where-Object { $_ -notmatch '\.exe$' }) +
        @('bin/solascope.exe', 'bin/sas.exe', $script:SolaScopeMarker)
    $pending = New-Object 'Collections.Generic.Stack[string]'
    $pending.Push($Path)
    while ($pending.Count -gt 0) {
        foreach ($item in (Get-ChildItem -LiteralPath $pending.Pop() -Force)) {
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Reparse point in existing installation: $($item.FullName)"
            }
            $relative = $item.FullName.Substring($Path.Length + 1).Replace('\', '/')
            if ($item.PSIsContainer) {
                if (@($allowed | Where-Object { $_.StartsWith($relative + '/', [StringComparison]::Ordinal) }).Count -eq 0) {
                    throw "Unrelated directory in installation; preserve it before upgrading: $relative"
                }
                $pending.Push($item.FullName)
            } elseif ($allowed -cnotcontains $relative) {
                throw "Unrelated file in installation; preserve it before upgrading: $relative"
            }
        }
    }
}

function Invoke-SolaScopeDownload([string]$Uri, [string]$OutFile) {
    if ($Uri -notmatch '^https://') { throw 'Downloads require HTTPS.' }
    for ($attempt = 1; $attempt -le 3; $attempt++) {
        try {
            Invoke-WebRequest -UseBasicParsing -Uri $Uri -OutFile $OutFile -TimeoutSec 60 `
                -MaximumRedirection 5 -UserAgent 'SolaScope-Installer' -Headers @{ 'Accept' = 'application/vnd.github+json' } `
                -ErrorAction Stop
            return
        } catch {
            if ($attempt -eq 3) { throw "Download failed after three attempts: $Uri. $($_.Exception.Message)" }
            Start-Sleep -Seconds $attempt
        }
    }
}

function Get-SolaScopeRelease([string]$RequestedVersion, [string]$Work) {
    $api = 'https://api.github.com/repos/wchiway/SolaScope/releases'
    if ($RequestedVersion) {
        $normalized = Get-SolaScopeVersion $RequestedVersion
        # Repository release tags use v<SemVer>. The tag is frozen after this lookup.
        $api += '/tags/' + [Uri]::EscapeDataString('v' + $normalized)
    } else {
        $api += '/latest'
    }
    $metadata = Join-Path $Work 'release.json'
    Invoke-SolaScopeDownload $api $metadata
    $release = Get-Content -LiteralPath $metadata -Raw | ConvertFrom-Json
    if ($release.draft -or -not $release.published_at -or
        ((-not $RequestedVersion) -and $release.prerelease)) {
        throw 'The selected release must be published; latest must be stable.'
    }
    $resolved = Get-SolaScopeVersion $release.tag_name
    if ($RequestedVersion -and $resolved -cne $normalized) { throw 'Release tag does not match requested version.' }
    if (-not $RequestedVersion -and $resolved.Split('+')[0].Contains('-')) { throw 'Latest release is not stable.' }
    $archiveName = "solascope-$resolved-x86_64-pc-windows-msvc.zip"
    $selected = @{}
    foreach ($name in @($archiveName, 'SHA256SUMS')) {
        $assets = @($release.assets | Where-Object { $_.name -ceq $name -and $_.state -eq 'uploaded' })
        if ($assets.Count -ne 1) { throw "Expected exactly one published asset named $name." }
        $uri = [Uri]$assets[0].browser_download_url
        $expectedPath = '/wchiway/SolaScope/releases/download/' + $release.tag_name + '/' + $name
        if ($uri.Scheme -cne 'https' -or $uri.Host -ine 'github.com' -or $uri.Port -ne 443 -or
            $uri.UserInfo -or $uri.Query -or $uri.Fragment -or
            [Uri]::UnescapeDataString($uri.AbsolutePath) -cne $expectedPath) {
            throw "Asset URL does not belong to the frozen release: $name"
        }
        $selected[$name] = $uri.AbsoluteUri
    }
    $archive = Join-Path $Work $archiveName
    $checksum = Join-Path $Work 'SHA256SUMS'
    Invoke-SolaScopeDownload $selected[$archiveName] $archive
    Invoke-SolaScopeDownload $selected['SHA256SUMS'] $checksum
    return @{ Version = $resolved; Archive = $archive; Checksum = $checksum; Name = $archiveName }
}

function Assert-SolaScopeChecksum([string]$Archive, [string]$ArchiveName, [string]$Checksum, [string]$Expected) {
    if ($Expected -and $Expected -notmatch '^[0-9a-fA-F]{64}$') { throw 'ExpectedSha256 must contain exactly 64 hexadecimal characters.' }
    if ($Checksum) {
        $hashes = @()
        foreach ($line in (Get-Content -LiteralPath $Checksum)) {
            if ([string]::IsNullOrWhiteSpace($line)) { continue }
            if ($line -cnotmatch '^([0-9a-fA-F]{64}) [ *](.+)$') { throw 'Malformed SHA256SUMS line.' }
            $hash = $Matches[1]
            $name = $Matches[2]
            if ($name -ceq $ArchiveName) { $hashes += $hash }
        }
        if ($hashes.Count -ne 1) { throw "SHA256SUMS must contain exactly one entry for $ArchiveName." }
        if ($Expected -and $Expected -ine $hashes[0]) { throw 'Checksum sources disagree.' }
        $Expected = $hashes[0]
    }
    if (-not $Expected) { throw 'Offline installation requires ChecksumPath or ExpectedSha256.' }
    if ((Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash -ine $Expected) {
        throw 'Archive SHA-256 mismatch; nothing was extracted or installed.'
    }
}

function Expand-SolaScopeArchive([string]$Archive, [string]$Stage) {
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($Archive)
    try {
        $seen = New-Object 'Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
        [long]$total = 0
        foreach ($entry in $zip.Entries) {
            $name = $entry.FullName
            if ($name -match '[\\:\x00-\x1f<>"|?*]' -or $name.StartsWith('/') -or $name.EndsWith('/')) {
                throw "Invalid ZIP entry (only regular public files are accepted): $name"
            }
            foreach ($part in ($name -split '/')) {
                if (-not $part -or $part -eq '.' -or $part -eq '..' -or $part -match '[ .]$' -or
                    $part -match '^(?i:CON|PRN|AUX|NUL|COM[1-9\u00b9\u00b2\u00b3]|LPT[1-9\u00b9\u00b2\u00b3])(?:\.|$)') {
                    throw "Unsafe Windows ZIP name: $name"
                }
            }
            if (-not $seen.Add($name)) { throw "Duplicate or case-colliding ZIP entry: $name" }
            # ExternalAttributes is signed Int32; mask the Unix file-type bits after widening.
            $attributes = [long]$entry.ExternalAttributes
            $kind = ($attributes -shr 16) -band 0xF000
            if (($kind -ne 0 -and $kind -ne 0x8000) -or ($attributes -band 0x458) -ne 0) {
                throw "Nonregular or reparse ZIP entry: $name"
            }
            if ($script:SolaScopePublicFiles -cnotcontains $name) { throw "Unexpected private or unknown ZIP payload: $name" }
            if ($entry.Length -lt 0 -or $entry.Length -gt 512MB -or $total -gt (512MB - $entry.Length)) {
                throw 'Archive exceeds the 512 MiB unpacked limit.'
            }
            $total += $entry.Length
        }
        foreach ($required in $script:SolaScopePublicFiles) {
            if (-not $seen.Contains($required)) { throw "Archive is missing $required." }
        }
        # Validate every entry before opening any output. Copy bounded by declared and total size.
        foreach ($entry in $zip.Entries) {
            $target = Join-Path $Stage $entry.FullName.Replace('/', '\')
            [void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($target))
            $source = $entry.Open()
            $output = $null
            try {
                $output = [IO.File]::Open($target, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
                $buffer = New-Object byte[] 65536
                [long]$written = 0
                while (($read = $source.Read($buffer, 0, $buffer.Length)) -gt 0) {
                    $written += $read
                    if ($written -gt $entry.Length) { throw 'ZIP entry expands beyond its declared size.' }
                    $output.Write($buffer, 0, $read)
                }
                if ($written -ne $entry.Length) { throw 'Truncated ZIP entry.' }
            } finally {
                if ($output) { $output.Dispose() }
                $source.Dispose()
            }
        }
    } finally { $zip.Dispose() }
    [void][IO.Directory]::CreateDirectory((Join-Path $Stage 'bin'))
    foreach ($name in @('solascope.exe', 'sas.exe')) {
        [IO.File]::Move((Join-Path $Stage $name), (Join-Path (Join-Path $Stage 'bin') $name))
    }
}

function Test-SolaScopeBinaries([string]$Directory, [string]$ExpectedVersion) {
    $versions = @()
    foreach ($name in @('solascope.exe', 'sas.exe')) {
        $start = New-Object Diagnostics.ProcessStartInfo
        $start.FileName = Join-Path (Join-Path $Directory 'bin') $name
        $start.Arguments = '--version'
        $start.UseShellExecute = $false
        $start.CreateNoWindow = $true
        $start.RedirectStandardOutput = $true
        $start.RedirectStandardError = $true
        $process = New-Object Diagnostics.Process
        $process.StartInfo = $start
        try {
            if (-not $process.Start()) { throw "Could not start $name." }
            $stdout = $process.StandardOutput.ReadToEndAsync()
            $stderr = $process.StandardError.ReadToEndAsync()
            if (-not $process.WaitForExit(30000)) {
                $process.Kill() # Only the installer-created --version process, never an existing service.
                throw "$name --version timed out."
            }
            # Complete process/pipe cleanup before publication or another install.
            $process.WaitForExit()
            $text = $stdout.GetAwaiter().GetResult().Trim()
            $errorText = $stderr.GetAwaiter().GetResult()
            if ($process.ExitCode -ne 0) { throw "$name --version exited with code $($process.ExitCode): $errorText" }
            if ($text -cnotmatch ('^solascope (' + $script:SolaScopeSemVer + ')$')) { throw "Unexpected $name version output: $text" }
            $versions += $Matches[1]
        } finally { $process.Dispose() }
    }
    if ($versions[0] -cne $versions[1] -or ($ExpectedVersion -and $versions[0] -cne $ExpectedVersion)) {
        throw 'Executable versions do not match each other or the selected release.'
    }
    return $versions[0]
}

function Publish-SolaScopeInstall([string]$Stage, [string]$Destination, [string]$ResolvedVersion) {
    Assert-SolaScopeOwned $Destination
    $backup = $Destination + '.backup-' + [Guid]::NewGuid().ToString('N')
    $oldMoved = $false
    $newMoved = $false
    try {
        if (Test-Path -LiteralPath $Destination) {
            foreach ($name in @('solascope.exe', 'sas.exe')) {
                $exe = Join-Path (Join-Path $Destination 'bin') $name
                if (Test-Path -LiteralPath $exe) {
                    try {
                        $handle = [IO.File]::Open($exe, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
                        $handle.Dispose()
                    } catch { throw "Close running SolaScope MCP/watch processes and retry; cannot replace $exe. $($_.Exception.Message)" }
                }
            }
            [IO.Directory]::Move($Destination, $backup)
            $oldMoved = $true
        }
        [IO.Directory]::Move($Stage, $Destination)
        $newMoved = $true
        [void](Test-SolaScopeBinaries $Destination $ResolvedVersion)
    } catch {
        $failure = $_
        try {
            if ($newMoved) { [IO.Directory]::Move($Destination, $Stage) }
            if ($oldMoved) { [IO.Directory]::Move($backup, $Destination) }
        } catch {
            throw "Installation failed: $failure. Rollback also failed: $_. Keep the backup at $backup for recovery."
        }
        throw $failure
    }
    if ($oldMoved) {
        try { Remove-Item -LiteralPath $backup -Recurse -Force -ErrorAction Stop }
        catch { Write-Warning "Installed successfully, but could not remove backup $backup. $($_.Exception.Message)" }
    }
}

function Get-SolaScopePathValue([AllowNull()][string]$Existing, [string]$Bin) {
    $entries = New-Object 'Collections.Generic.List[string]'
    $own = $Bin.TrimEnd('\')
    $parts = @()
    if ($Existing) { $parts = $Existing -split ';' }
    foreach ($entry in $parts) {
        $compare = [Environment]::ExpandEnvironmentVariables($entry.Trim().Trim('"')).Replace('/', '\').TrimEnd('\')
        if (-not $compare.Equals($own, [StringComparison]::OrdinalIgnoreCase)) { $entries.Add($entry) }
    }
    $entries.Add($Bin)
    return $entries -join ';'
}

function Set-SolaScopePath([string]$Bin) {
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $updated = Get-SolaScopePathValue $userPath $Bin
    if ($updated.Length -gt 32760) { throw 'User PATH would exceed the Windows environment limit.' }
    if ($userPath -cne $updated) { [Environment]::SetEnvironmentVariable('Path', $updated, 'User') }
    $env:Path = Get-SolaScopePathValue $env:Path $Bin
}

function Invoke-SolaScopeInstall {
    [CmdletBinding()]
    param([string]$Version, [string]$InstallDir, [switch]$NoPath,
        [string]$ArchivePath, [string]$ChecksumPath, [string]$ExpectedSha256)
    $ErrorActionPreference = 'Stop'
    Set-StrictMode -Version 2.0
    Assert-SolaScopePlatform
    if (-not $InstallDir) { $InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\SolaScope' }
    $destination = Get-SolaScopeDestination $InstallDir
    if ($Version) { $Version = Get-SolaScopeVersion $Version }
    if (-not $ArchivePath -and ($ChecksumPath -or $ExpectedSha256)) {
        throw 'ChecksumPath and ExpectedSha256 require ArchivePath; online mode uses release SHA256SUMS.'
    }
    if ($ArchivePath -and -not ($ChecksumPath -or $ExpectedSha256)) {
        throw 'Offline installation requires ChecksumPath or ExpectedSha256.'
    }
    $parent = [IO.Path]::GetDirectoryName($destination)
    # Never rename the directory containing the executing installer or caller-owned inputs.
    foreach ($inputDirectory in @($PSScriptRoot, [IO.Path]::GetTempPath())) {
        if ($inputDirectory -and (Test-SolaScopeWithin ([IO.Path]::GetFullPath($inputDirectory)) $destination)) {
            throw 'InstallDir must be separate from the executing scripts and temporary data.'
        }
    }
    foreach ($inputFile in @($ArchivePath, $ChecksumPath)) {
        if ($inputFile -and (Test-SolaScopeWithin ([IO.Path]::GetFullPath($inputFile)) $destination)) {
            throw 'InstallDir must not contain the input archive or checksum.'
        }
    }
    [void][IO.Directory]::CreateDirectory($parent)
    Assert-SolaScopeNoReparse $parent
    # A persistent sibling lock avoids delete/recreate races between parallel installers.
    $hasher = [Security.Cryptography.SHA256]::Create()
    try { $key = [BitConverter]::ToString($hasher.ComputeHash([Text.Encoding]::UTF8.GetBytes($destination.ToUpperInvariant()))).Replace('-', '') }
    finally { $hasher.Dispose() }
    $lockPath = Join-Path $parent ('.solascope-lock-' + $key)
    Assert-SolaScopeNoReparse $lockPath
    $lock = $null
    $work = $null
    $stage = $null
    $previousTls = [Net.ServicePointManager]::SecurityProtocol
    try {
        try { $lock = [IO.File]::Open($lockPath, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None) }
        catch { throw "Another installation may be active, or the install parent is not writable: $parent. $($_.Exception.Message)" }
        Assert-SolaScopeOwned $destination
        $work = Join-Path ([IO.Path]::GetTempPath()) ('solascope-download-' + [Guid]::NewGuid().ToString('N'))
        Assert-SolaScopeNoReparse $work
        [void][IO.Directory]::CreateDirectory($work)
        if ($ArchivePath) {
            # Resolve relative filesystem inputs against PowerShell's current location.
            if (-not [IO.Path]::IsPathRooted($ArchivePath)) {
                $ArchivePath = Join-Path (Get-Location).ProviderPath $ArchivePath
            }
            if ($ChecksumPath -and -not [IO.Path]::IsPathRooted($ChecksumPath)) {
                $ChecksumPath = Join-Path (Get-Location).ProviderPath $ChecksumPath
            }
            $ArchivePath = [IO.Path]::GetFullPath($ArchivePath)
            if ($ChecksumPath) { $ChecksumPath = [IO.Path]::GetFullPath($ChecksumPath) }
            # Reject UNC and mapped network drives before resolving or reading inputs.
            Assert-SolaScopeLocalPath $ArchivePath
            Assert-SolaScopeNoReparse $ArchivePath
            if ($ChecksumPath) {
                Assert-SolaScopeLocalPath $ChecksumPath
                Assert-SolaScopeNoReparse $ChecksumPath
            }
            # This branch never calls any release or HTTP helper.
            $source = (Get-Item -LiteralPath $ArchivePath -Force).FullName
            if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw 'ArchivePath must be a local ZIP file.' }
            $archiveName = [IO.Path]::GetFileName($source)
            $archive = Join-Path $work 'input.zip'
            Copy-Item -LiteralPath $source -Destination $archive
            $checksum = $null
            if ($ChecksumPath) {
                $checksum = Join-Path $work 'SHA256SUMS'
                Copy-Item -LiteralPath $ChecksumPath -Destination $checksum
            }
        } else {
            # Enable TLS 1.2 without disabling normal certificate or proxy handling.
            [Net.ServicePointManager]::SecurityProtocol = $previousTls -bor [Net.SecurityProtocolType]::Tls12
            $release = Get-SolaScopeRelease $Version $work
            $Version = $release.Version
            $archive = $release.Archive
            $archiveName = $release.Name
            $checksum = $release.Checksum
        }
        Assert-SolaScopeChecksum $archive $archiveName $checksum $ExpectedSha256
        $stage = Join-Path $parent ('.solascope-stage-' + [Guid]::NewGuid().ToString('N'))
        [void][IO.Directory]::CreateDirectory($stage)
        Expand-SolaScopeArchive $archive $stage
        $resolved = Test-SolaScopeBinaries $stage $Version
        @{ format = 'SolaScope installer 1'; directory = $destination; version = $resolved } |
            ConvertTo-Json | Set-Content -LiteralPath (Join-Path $stage $script:SolaScopeMarker) -Encoding UTF8
        Publish-SolaScopeInstall $stage $destination $resolved
        Write-Host "Installed SolaScope $resolved in $destination"
        if ($NoPath) {
            Write-Host 'PATH was not changed (-NoPath). Run bin\solascope.exe or bin\sas.exe by absolute path.'
        } else {
            try { Set-SolaScopePath (Join-Path $destination 'bin') }
            catch { throw "SolaScope is installed, but User PATH could not be updated. Retry with -NoPath or edit User PATH. $($_.Exception.Message)" }
            Write-Host 'User PATH updated. Open a new terminal and restart existing IDEs to inherit it.'
            Write-Host 'This PowerShell process was refreshed; a parent CMD/terminal process cannot be refreshed by this installer.'
        }
        Write-Host 'Existing user configuration was preserved. No models or optional runtimes were installed.'
    } finally {
        [Net.ServicePointManager]::SecurityProtocol = $previousTls
        foreach ($temporary in @($stage, $work)) {
            if ($temporary -and (Test-Path -LiteralPath $temporary)) {
                try { Remove-Item -LiteralPath $temporary -Recurse -Force -ErrorAction Stop }
                catch { Write-Warning "Could not remove temporary directory $temporary. $($_.Exception.Message)" }
            }
        }
        if ($lock) { $lock.Dispose() }
    }
}

if ($MyInvocation.InvocationName -ne '.') {
    try {
        Invoke-SolaScopeInstall @PSBoundParameters
        exit 0
    } catch {
        [Console]::Error.WriteLine('SolaScope installation failed: ' + $_.Exception.Message)
        exit 1
    }
}
