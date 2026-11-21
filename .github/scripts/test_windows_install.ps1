#requires -Version 5.1
[CmdletBinding()]
param([string]$ArchivePath, [string]$ChecksumPath)

$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object Text.UTF8Encoding $false
Set-StrictMode -Version 2.0
$installer = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..\install.ps1'))
# Parsing uses the actual Windows PowerShell parser, even before dot-sourcing.
$tokens = $null
$errors = $null
[void][Management.Automation.Language.Parser]::ParseFile($installer, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw ($errors | Out-String) }
$realArchiveInput = $ArchivePath
$realChecksumInput = $ChecksumPath
. $installer
$ArchivePath = $realArchiveInput
$ChecksumPath = $realChecksumInput
Assert-SolaScopePlatform
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw "Assertion failed: $Message" }
}
function Assert-Throws([scriptblock]$Action, [string]$Pattern) {
    try { & $Action }
    catch {
        if ($_.Exception.Message -notmatch $Pattern) { throw "Wrong failure (expected $Pattern): $_" }
        return
    }
    throw "Expected failure matching $Pattern"
}
function New-TestZip([string]$Name, [object[]]$Entries) {
    $path = Join-Path $script:FixtureRoot $Name
    $zip = [IO.Compression.ZipFile]::Open($path, [IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($spec in $Entries) {
            $entry = $zip.CreateEntry([string]$spec.Name)
            if ($spec.ContainsKey('Attributes')) { $entry.ExternalAttributes = [int]$spec.Attributes }
            $stream = $entry.Open()
            try {
                if ($spec.ContainsKey('Source')) { $bytes = [IO.File]::ReadAllBytes($spec.Source) }
                else { $bytes = [Text.Encoding]::UTF8.GetBytes('fixture') }
                $stream.Write($bytes, 0, $bytes.Length)
            } finally { $stream.Dispose() }
        }
    } finally { $zip.Dispose() }
    return $path
}
function New-TestPackage([string]$ArchiveName, [string]$Exe) {
    $entries = @()
    foreach ($entryName in $script:SolaScopePublicFiles) {
        if ($entryName -match '\.exe$') { $entries += @{ Name = $entryName; Source = $Exe } }
        else { $entries += @{ Name = $entryName } }
    }
    return New-TestZip $ArchiveName $entries
}
function New-VersionExe([string]$Name, [string]$Version, [int]$ExitCode = 0) {
    $path = Join-Path $script:FixtureRoot $Name
    $class = 'VersionFixture' + [Guid]::NewGuid().ToString('N')
    $source = 'public class ' + $class + ' { public static int Main() { System.Console.WriteLine("solascope ' + $Version + '"); return ' + $ExitCode + '; } }'
    # Uses the .NET Framework compiler built into Windows, not an SDK/package manager.
    Add-Type -TypeDefinition $source -OutputAssembly $path -OutputType ConsoleApplication
    return $path
}
function Assert-BadZip([string]$Name, [object[]]$Entries, [string]$Pattern) {
    $zip = New-TestZip $Name $Entries
    $stage = Join-Path $script:FixtureRoot ([Guid]::NewGuid().ToString('N'))
    [void][IO.Directory]::CreateDirectory($stage)
    Assert-Throws { Expand-SolaScopeArchive $zip $stage } $Pattern
    Assert-True (@(Get-ChildItem -LiteralPath $stage -Force).Count -eq 0) 'invalid archive extracted no files'
}

$root = Join-Path ([IO.Path]::GetTempPath()) ('solascope-tests-' + [Guid]::NewGuid().ToString('N'))
$script:FixtureRoot = Join-Path $root 'fixtures'
[void][IO.Directory]::CreateDirectory($script:FixtureRoot)
$originalEnvironment = @{}
foreach ($item in (Get-ChildItem Env:)) {
    if ($item.Name -match '^SOLASCOPE__|^(HOME|USERPROFILE|LOCALAPPDATA|APPDATA|PATH)$') {
        $originalEnvironment[$item.Name] = $item.Value
    }
}
$userPathBefore = [Environment]::GetEnvironmentVariable('Path', 'User')
$locationBefore = Get-Location
$originalDownload = ${function:Invoke-SolaScopeDownload}
$originalSmoke = ${function:Test-SolaScopeBinaries}
$originalSetPath = ${function:Set-SolaScopePath}
try {
    if (($ArchivePath -and -not $ChecksumPath) -or ($ChecksumPath -and -not $ArchivePath)) {
        throw 'Supply ArchivePath and ChecksumPath together.'
    }
    if ($ArchivePath) {
        $ArchivePath = (Get-Item -LiteralPath $ArchivePath).FullName
        $ChecksumPath = (Get-Item -LiteralPath $ChecksumPath).FullName
    }
    foreach ($item in @(Get-ChildItem Env: | Where-Object { $_.Name -like 'SOLASCOPE__*' })) {
        [Environment]::SetEnvironmentVariable($item.Name, $null, 'Process')
    }
    $env:HOME = Join-Path $root 'home'
    $env:USERPROFILE = $env:HOME
    $env:LOCALAPPDATA = Join-Path $root 'local'
    $env:APPDATA = Join-Path $root 'roaming'
    $env:SOLASCOPE__EMBEDDING__PROVIDER = 'disabled'
    $env:SOLASCOPE__RERANK__PROVIDER = 'disabled'
    $env:SOLASCOPE__PRIVACY__REMOTE_OPT_IN = 'false'
    $configDirectory = Join-Path $env:USERPROFILE '.solascope'
    [void][IO.Directory]::CreateDirectory($configDirectory)
    $config = Join-Path $configDirectory '.solascope.toml'
    [IO.File]::WriteAllText($config, '# existing user config must survive')
    $configBefore = (Get-FileHash -LiteralPath $config).Hash
    Set-Location -LiteralPath $root
    # Any accidental HTTP or User PATH operation is a hard failure.
    function Invoke-SolaScopeDownload { throw 'NETWORK MUST NOT BE CALLED IN TESTS' }
    function Set-SolaScopePath { throw 'USER PATH MUST NOT BE WRITTEN IN TESTS' }

    Assert-True ((Get-SolaScopeVersion 'v1.2.3-rc.1+build') -ceq '1.2.3-rc.1+build') 'SemVer normalization'
    foreach ($bad in @('1.02.3', '1.2', '1.2.3-01', '../1.2.3', 'latest')) {
        Assert-Throws { Get-SolaScopeVersion $bad } 'Invalid version'
    }
    $bin = 'C:\Users\Example\SolaScope\bin'
    $path = Get-SolaScopePathValue ('C:\Unrelated;"c:/users/example/solascope/bin/";C:\Unrelated;' + $bin) $bin
    Assert-True ($path -ceq ('C:\Unrelated;C:\Unrelated;' + $bin)) 'only own PATH entries are deduplicated'
    Assert-True ((Get-SolaScopePathValue $null $bin) -ceq $bin) 'empty PATH'
    Assert-True ((Get-SolaScopePathValue ';C:\Other;;' $bin) -ceq (';C:\Other;;;' + $bin)) 'unrelated empty PATH entries preserved'
    Assert-Throws { Assert-SolaScopeLocalPath '\\server\share\archive.zip' } 'local drive'
    foreach ($bad in @('C:\', $env:USERPROFILE, $env:LOCALAPPDATA, $configDirectory,
            'relative\folder', 'C:\temp\..\bad', 'C:\temp\AUX', 'C:\temp\bad.')) {
        Assert-Throws { Get-SolaScopeDestination $bad } 'Refusing|absolute|dot segments|Reserved'
    }
    $foreign = Join-Path $root 'foreign'
    [void][IO.Directory]::CreateDirectory($foreign)
    [IO.File]::WriteAllText((Join-Path $foreign 'keep.txt'), 'untouched')
    Assert-Throws { Assert-SolaScopeOwned $foreign } 'not installer-owned'

    $hashInput = Join-Path $root 'input.zip'
    [IO.File]::WriteAllText($hashInput, 'checksum fixture')
    $hash = (Get-FileHash -LiteralPath $hashInput -Algorithm SHA256).Hash
    $sum = Join-Path $root 'SHA256SUMS'
    [IO.File]::WriteAllText($sum, "$hash  input.zip`n")
    Assert-SolaScopeChecksum $hashInput 'input.zip' $sum $null
    Assert-SolaScopeChecksum $hashInput 'input.zip' $null $hash
    Assert-Throws { Assert-SolaScopeChecksum $hashInput 'input.zip' $null ('0' * 64) } 'mismatch'
    Assert-Throws { Assert-SolaScopeChecksum $hashInput 'input.zip' $sum ('0' * 64) } 'disagree'
    Assert-Throws { Assert-SolaScopeChecksum $hashInput 'input.zip' $null 'bad' } '64 hexadecimal'
    Assert-Throws { Assert-SolaScopeChecksum $hashInput 'missing.zip' $sum $null } 'exactly one'
    [IO.File]::WriteAllText($sum, "$hash  input.zip`n$hash  input.zip`n")
    Assert-Throws { Assert-SolaScopeChecksum $hashInput 'input.zip' $sum $null } 'exactly one'
    [IO.File]::WriteAllText($sum, 'malformed')
    Assert-Throws { Assert-SolaScopeChecksum $hashInput 'input.zip' $sum $null } 'Malformed'

    $counter = 0
    foreach ($name in @('../evil', '/LICENSE', 'docs\mcp.md', 'C:/evil', 'LICENSE:stream',
            'docs/./mcp.md', 'docs//mcp.md', 'docs/../LICENSE', 'AUX', 'COM1.txt',
            'LICENSE.', 'LICENSE ', '.env', 'private/secret.txt', 'docs/')) {
        $counter++
        Assert-BadZip "bad-$counter.zip" @(@{ Name = $name }) 'Invalid ZIP|Unsafe Windows|Unexpected private'
    }
    Assert-BadZip 'duplicate.zip' @(@{ Name = 'LICENSE' }, @{ Name = 'LICENSE' }) 'Duplicate'
    Assert-BadZip 'case.zip' @(@{ Name = 'LICENSE' }, @{ Name = 'license' }) 'case-colliding'
    Assert-BadZip 'symlink.zip' @(@{ Name = 'LICENSE'; Attributes = -1577123840 }) 'Nonregular'
    Assert-BadZip 'reparse.zip' @(@{ Name = 'LICENSE'; Attributes = 1024 }) 'Nonregular'
    Assert-BadZip 'missing.zip' @(@{ Name = 'LICENSE' }) 'missing solascope.exe'
    # Patch only the central-directory uncompressed length to exercise the size guard without allocating 512 MiB.
    $large = New-TestZip 'large.zip' @(@{ Name = 'LICENSE' })
    $bytes = [IO.File]::ReadAllBytes($large)
    $patched = $false
    for ($i = 0; $i -le $bytes.Length - 46; $i++) {
        if ($bytes[$i] -eq 0x50 -and $bytes[$i + 1] -eq 0x4b -and $bytes[$i + 2] -eq 1 -and $bytes[$i + 3] -eq 2) {
            [Array]::Copy([BitConverter]::GetBytes([uint32](512MB + 1)), 0, $bytes, $i + 24, 4)
            $patched = $true
            break
        }
    }
    Assert-True $patched 'found central-directory fixture record'
    [IO.File]::WriteAllBytes($large, $bytes)
    $largeStage = Join-Path $root 'large-stage'
    [void][IO.Directory]::CreateDirectory($largeStage)
    Assert-Throws { Expand-SolaScopeArchive $large $largeStage } '512 MiB'

    $exe1 = New-VersionExe 'version1.exe' '1.2.3'
    $exe2 = New-VersionExe 'version2.exe' '1.2.4'
    $badExe = New-VersionExe 'bad-exit.exe' '1.2.3' 7
    $package1 = New-TestPackage 'version1.zip' $exe1
    $package2 = New-TestPackage 'version2.zip' $exe2
    $packageBad = New-TestPackage 'bad-exit.zip' $badExe
    # Require the full public contract, not merely executable names.
    $partialEntries = @()
    foreach ($entryName in $script:SolaScopePublicFiles) {
        if ($entryName -ceq 'docs/releases.md') { continue }
        if ($entryName -match '\.exe$') { $partialEntries += @{ Name = $entryName; Source = $exe1 } }
        else { $partialEntries += @{ Name = $entryName } }
    }
    Assert-BadZip 'missing-guide.zip' $partialEntries 'missing docs/releases.md'

    # Exercise frozen online metadata resolution with entirely local mocked data.
    # Build metadata assets with encoded and literal SemVer plus signs.
    $script:MockRelease = $null
    $script:DownloadCalls = @()
    function Invoke-SolaScopeDownload([string]$Uri, [string]$OutFile) {
        $script:DownloadCalls += $Uri
        if ($Uri.StartsWith('https://api.github.com/')) {
            $script:MockRelease | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $OutFile -Encoding UTF8
        } else { [IO.File]::WriteAllText($OutFile, 'mock download') }
    }
    try {
        foreach ($encoded in @($false, $true)) {
            $tag = 'v1.2.3+build.2'
            $archiveName = 'solascope-1.2.3+build.2-x86_64-pc-windows-msvc.zip'
            $urlTag = $tag
            $urlName = $archiveName
            if ($encoded) { $urlTag = [Uri]::EscapeDataString($tag); $urlName = [Uri]::EscapeDataString($archiveName) }
            $script:MockRelease = @{ tag_name = $tag; draft = $false; prerelease = $false; published_at = '2026-11-21'; assets = @(
                @{ name = $archiveName; state = 'uploaded'; browser_download_url = "https://github.com/wchiway/SolaScope/releases/download/$urlTag/$urlName" },
                @{ name = 'SHA256SUMS'; state = 'uploaded'; browser_download_url = "https://github.com/wchiway/SolaScope/releases/download/$urlTag/SHA256SUMS" }
            ) }
            $script:DownloadCalls = @()
            $mockWork = Join-Path $root ('release-mock-' + [Guid]::NewGuid().ToString('N'))
            [void][IO.Directory]::CreateDirectory($mockWork)
            $release = Get-SolaScopeRelease '1.2.3+build.2' $mockWork
            Assert-True ($release.Version -ceq '1.2.3+build.2') 'frozen metadata version with plus signs'
            Assert-True ($script:DownloadCalls.Count -eq 3) 'one metadata lookup and two downloads'
        }
        $script:MockRelease.draft = $true
        Assert-Throws { Get-SolaScopeRelease $null $mockWork } 'published'
        $script:MockRelease.draft = $false
        $script:MockRelease.assets[0].browser_download_url = 'https://example.invalid/asset.zip'
        Assert-Throws { Get-SolaScopeRelease '1.2.3+build.2' $mockWork } 'frozen release'
    } finally {
        function Invoke-SolaScopeDownload { throw 'NETWORK MUST NOT BE CALLED IN TESTS' }
    }
    # Unicode plus spaces without non-ASCII source encoding dependencies.
    $destination = Join-Path $root ('install space ' + [char]0x6D4B + [char]0x8BD5)
    $hash1 = (Get-FileHash -LiteralPath $package1).Hash
    $hash2 = (Get-FileHash -LiteralPath $package2).Hash
    $packageSum = Join-Path $root 'package-SHA256SUMS'
    [IO.File]::WriteAllText($packageSum, "$hash1  version1.zip`n")
    Invoke-SolaScopeInstall -ArchivePath $package1 -ChecksumPath $packageSum -InstallDir $destination -NoPath
    Assert-True ((Test-SolaScopeBinaries $destination '1.2.3') -ceq '1.2.3') 'first install'
    Assert-True (Test-Path -LiteralPath (Join-Path $destination 'docs/installation.md')) 'installation docs preserved'
    Assert-True (Test-Path -LiteralPath (Join-Path $destination 'docs/releases.md')) 'release docs preserved'
    Invoke-SolaScopeInstall -ArchivePath $package1 -ExpectedSha256 $hash1 -InstallDir $destination -NoPath
    Invoke-SolaScopeInstall -ArchivePath $package2 -ExpectedSha256 $hash2 -InstallDir $destination -NoPath
    Assert-True ((Test-SolaScopeBinaries $destination '1.2.4') -ceq '1.2.4') 'upgrade'
    Assert-Throws {
        Invoke-SolaScopeInstall -ArchivePath $packageBad -ExpectedSha256 (Get-FileHash -LiteralPath $packageBad).Hash -InstallDir $destination -NoPath
    } 'code 7'
    Assert-Throws {
        Invoke-SolaScopeInstall -ArchivePath $package1 -ExpectedSha256 $hash1 -Version '9.9.9' -InstallDir $destination -NoPath
    } 'versions do not match'
    Assert-True ((Test-SolaScopeBinaries $destination '1.2.4') -ceq '1.2.4') 'failed smoke preserves old install'
    $unknown = Join-Path $destination 'custom.txt'
    [IO.File]::WriteAllText($unknown, 'user file')
    Assert-Throws {
        Invoke-SolaScopeInstall -ArchivePath $package1 -ExpectedSha256 $hash1 -InstallDir $destination -NoPath
    } 'Unrelated file'
    Assert-True (([IO.File]::ReadAllText($unknown)) -ceq 'user file') 'unrelated file preserved'
    Remove-Item -LiteralPath $unknown
    $locked = [IO.File]::Open((Join-Path $destination 'bin\solascope.exe'), [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::None)
    try {
        Assert-Throws {
            Invoke-SolaScopeInstall -ArchivePath $package1 -ExpectedSha256 $hash1 -InstallDir $destination -NoPath
        } 'Close running'
    } finally { $locked.Dispose() }
    Assert-True ((Test-SolaScopeBinaries $destination '1.2.4') -ceq '1.2.4') 'locked file preserves old install'

    $lockFiles = @(Get-ChildItem -LiteralPath $root -Force -Filter '.solascope-lock-*')
    Assert-True ($lockFiles.Count -eq 1) 'one persistent install lock'
    $heldLock = [IO.File]::Open($lockFiles[0].FullName, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    try {
        Assert-Throws {
            Invoke-SolaScopeInstall -ArchivePath $package1 -ExpectedSha256 $hash1 -InstallDir $destination -NoPath
        } 'Another installation'
    } finally { $heldLock.Dispose() }
    Assert-True ((Test-SolaScopeBinaries $destination '1.2.4') -ceq '1.2.4') 'install serialization preserves old version'

    # Inject failure only after publication; the pre-publication smoke still uses real EXEs.
    $script:RollbackDestination = $destination
    function Test-SolaScopeBinaries([string]$Directory, [string]$ExpectedVersion) {
        if ($Directory -eq $script:RollbackDestination) { throw 'injected publication failure' }
        & $originalSmoke $Directory $ExpectedVersion
    }
    try {
        Assert-Throws {
            Invoke-SolaScopeInstall -ArchivePath $package1 -ExpectedSha256 $hash1 -InstallDir $destination -NoPath
        } 'injected publication failure'
    } finally { Set-Item -LiteralPath function:Test-SolaScopeBinaries -Value $originalSmoke }
    Assert-True ((Test-SolaScopeBinaries $destination '1.2.4') -ceq '1.2.4') 'rollback restores old version'
    Assert-True ((Get-FileHash -LiteralPath $config).Hash -ceq $configBefore) 'user config preserved'

    if ($ArchivePath) {
        $realDestination = Join-Path $root 'real-package'
        Invoke-SolaScopeInstall -ArchivePath $ArchivePath -ChecksumPath $ChecksumPath -InstallDir $realDestination -NoPath
        $realVersion = Test-SolaScopeBinaries $realDestination $null
        Invoke-SolaScopeInstall -ArchivePath $ArchivePath -ChecksumPath $ChecksumPath -InstallDir $realDestination -NoPath
        Assert-True ((Test-SolaScopeBinaries $realDestination $realVersion) -ceq $realVersion) 'real package repeated install'
        Assert-True ((Get-FileHash -LiteralPath $config).Hash -ceq $configBefore) 'real package config preservation'
        Write-Host "Real Windows package smoke passed: $realVersion"
    }
    Assert-True ([Environment]::GetEnvironmentVariable('Path', 'User') -ceq $userPathBefore) 'real User PATH unchanged'
    Write-Host 'Windows installer offline fixtures passed.'
} finally {
    Set-Location -LiteralPath $locationBefore.Path
    Set-Item -LiteralPath function:Invoke-SolaScopeDownload -Value $originalDownload
    Set-Item -LiteralPath function:Test-SolaScopeBinaries -Value $originalSmoke
    Set-Item -LiteralPath function:Set-SolaScopePath -Value $originalSetPath
    foreach ($item in @(Get-ChildItem Env:)) {
        if ($item.Name -match '^SOLASCOPE__|^(HOME|USERPROFILE|LOCALAPPDATA|APPDATA|PATH)$') {
            [Environment]::SetEnvironmentVariable($item.Name, $null, 'Process')
        }
    }
    foreach ($key in $originalEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($key, $originalEnvironment[$key], 'Process')
    }
    Remove-Item -LiteralPath $root -Recurse -Force
}
