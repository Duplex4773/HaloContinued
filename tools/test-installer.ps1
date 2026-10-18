#requires -Version 7.0
param(
  [Parameter(Mandatory)][string]$Iscc,
  [string]$PackageZip,
  [string]$PreviousPackageZip
)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (!$PackageZip) { $PackageZip = Join-Path $repo 'dist/HaloContinued-1.0.5-windows-x64.zip' }
$PackageZip = (Resolve-Path -LiteralPath $PackageZip).Path
if ($PreviousPackageZip) { $PreviousPackageZip = (Resolve-Path -LiteralPath $PreviousPackageZip).Path }
$Iscc = (Resolve-Path -LiteralPath $Iscc).Path
$id = [guid]::NewGuid().ToString().ToUpperInvariant()
$name = "Halo Continued installer test $id"
$startupName = "HaloContinued.Installer.Test.$id"
$mutexName = "Local\HaloContinued.Installer.Test.$id"
$root = Join-Path $repo "validation-local/installer-test-$id"
$payload = Join-Path $root 'payload'
$install = Join-Path $root 'installed app'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$uninstallKey = "Software\Microsoft\Windows\CurrentVersion\Uninstall\{$id}_is1"
$startLink = Join-Path ([Environment]::GetFolderPath('Programs')) "$name.lnk"
$desktopLink = Join-Path ([Environment]::GetFolderPath('DesktopDirectory')) "$name.lnk"
$setup = Join-Path $root 'TestSetup.exe'
$uninstaller = Join-Path $install 'unins000.exe'
$sequence = 0

function Assert-True([bool]$Condition, [string]$Message) {
  if (!$Condition) { throw $Message }
}
function Read-Startup {
  if (Test-Path -LiteralPath $runKey) {
    (Get-ItemProperty -LiteralPath $runKey).PSObject.Properties[$startupName].Value
  }
}
function Invoke-Setup([string]$Tasks, [switch]$ExpectFailure, [string]$Installer = $setup) {
  $script:sequence++
  $arguments = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-',
    ('/DIR="' + $install + '"'), ('/LOG="' + (Join-Path $root "setup-$sequence.log") + '"'))
  if ($Tasks) { $arguments += $Tasks }
  $process = Start-Process -FilePath $Installer -ArgumentList $arguments -WindowStyle Hidden -Wait -PassThru
  if ($ExpectFailure) { Assert-True ($process.ExitCode -ne 0) 'Setup ignored the application mutex.' }
  else { Assert-True ($process.ExitCode -eq 0) "Setup failed ($($process.ExitCode)). See $root." }
}
function Invoke-Uninstall([switch]$ExpectFailure) {
  $script:sequence++
  $arguments = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART',
    ('/LOG="' + (Join-Path $root "uninstall-$sequence.log") + '"'))
  $process = Start-Process -FilePath $uninstaller -ArgumentList $arguments -WindowStyle Hidden -Wait -PassThru
  if ($ExpectFailure) { Assert-True ($process.ExitCode -ne 0) 'Uninstall ignored the application mutex.' }
  else { Assert-True ($process.ExitCode -eq 0) "Uninstall failed ($($process.ExitCode)). See $root." }
}

# Test only a renamed fixture with private identities. Never stop or start the
# production app, alter its startup entry, or send commands to physical devices.
[IO.Directory]::CreateDirectory($root) | Out-Null
$mutex = $null
try {
  Expand-Archive -LiteralPath $PackageZip -DestinationPath $payload
  $version = [Diagnostics.FileVersionInfo]::GetVersionInfo((Join-Path $payload 'HaloContinued.exe')).ProductVersion
  Assert-True ($version -match '^\d+\.\d+\.\d+$') 'Packaged app has no numeric product version.'
  $expected = @('HaloContinued.exe', 'README.md', 'LICENSE-HaloBattery.txt', 'THIRD-PARTY-NOTICES.txt', 'SHA256SUMS.txt')
  foreach ($file in Get-ChildItem -LiteralPath $payload -File -Recurse) {
    $relative = [IO.Path]::GetRelativePath($payload, $file.FullName)
    Assert-True ($relative -in $expected -or $relative -match '^licenses[\\/][^\\/]+$') "Unexpected package file: $relative"
  }
  foreach ($file in $expected) { Assert-True (Test-Path -LiteralPath (Join-Path $payload $file)) "Missing package file: $file" }
  $packVersion = $version + '-installer-test-' + $id
  $packOutput = Join-Path $repo "dist/HaloContinued-$packVersion-windows-x64"
  [IO.Directory]::CreateDirectory((Join-Path $packOutput 'licenses')) | Out-Null
  'private settings' | Set-Content -LiteralPath (Join-Path $packOutput 'config.json')
  'private marker' | Set-Content -LiteralPath (Join-Path $packOutput 'portable.txt')
  'obsolete notice' | Set-Content -LiteralPath (Join-Path $packOutput 'licenses/obsolete-test-notice.txt')
  try {
    & (Join-Path $PSScriptRoot 'package-rust.ps1') -Version $packVersion -Executable (Join-Path $payload 'HaloContinued.exe') | Out-Host
    $archive = [IO.Compression.ZipFile]::OpenRead("$packOutput.zip")
    try {
      foreach ($entry in $archive.Entries) {
        Assert-True ($entry.FullName -notmatch '(config\.json|portable\.txt|obsolete-test-notice\.txt)$') 'Repackaging leaked stale files or user data.'
      }
    } finally { $archive.Dispose() }
    Assert-True ((Get-Content -LiteralPath (Join-Path $packOutput 'config.json') -Raw).Trim() -eq 'private settings') 'Repackaging changed existing user data.'
  } finally {
    $distRoot = [IO.Path]::GetFullPath((Join-Path $repo 'dist')) + [IO.Path]::DirectorySeparatorChar
    $resolved = [IO.Path]::GetFullPath($packOutput)
    if (!$resolved.StartsWith($distRoot, [StringComparison]::OrdinalIgnoreCase) -or
        ![IO.Path]::GetFileName($resolved).Contains($id)) { throw 'Unexpected test packaging path.' }
    Remove-Item -LiteralPath $resolved -Recurse -Force
    if (Test-Path -LiteralPath "$resolved.zip") { Remove-Item -LiteralPath "$resolved.zip" -Force }
  }
  $source = Get-Content -LiteralPath (Join-Path $repo 'installer/HaloContinued.iss') -Raw
  $source = $source.Replace('#define ProductName "Halo Continued"', "#define ProductName `"$name`"")
  $source = $source.Replace('#define ProductId "{{D31E62BF-458D-43EA-A5BB-6B69A1717F78}"', "#define ProductId `"{{$id}`"")
  $source = $source.Replace('#define ProductMutex "Local\HaloBatteryNext"', "#define ProductMutex `"$mutexName`"")
  $source = $source.Replace('#define StartupValue "HaloBatteryNext"', "#define StartupValue `"$startupName`"")
  $source = $source.Replace('SetupMutex=Local\HaloContinued.Setup', "SetupMutex=$mutexName.Setup")
  $source = $source.Replace('SetupIconFile=..\crates\app\app.ico', "SetupIconFile=$repo\crates\app\app.ico")
  $source = $source.Replace('OutputBaseFilename=HaloContinued-{#ReleaseVersion}-windows-x64-setup', 'OutputBaseFilename=TestSetup')
  Assert-True (!$source.Contains('ProductMutex "Local\HaloBatteryNext"') -and !$source.Contains('StartupValue "HaloBatteryNext"') -and !$source.Contains('D31E62BF-458D-43EA-A5BB-6B69A1717F78')) 'Fixture identity isolation failed.'
  $scriptPath = Join-Path $root 'TestSetup.iss'
  $source | Set-Content -LiteralPath $scriptPath -Encoding utf8
  & $Iscc '/Qp' "/DPackageDir=$payload" "/DReleaseVersion=$version" "/DNumericVersion=$version" "/DDestinationDir=$root" $scriptPath
  Assert-True ($LASTEXITCODE -eq 0) 'Fixture compilation failed.'
  Assert-True ((Get-AuthenticodeSignature -LiteralPath $setup).Status -eq 'NotSigned') 'Installer is unexpectedly signed.'
  $initialSetup = $setup
  $initialPayload = $payload
  if ($PreviousPackageZip) {
    $initialPayload = Join-Path $root 'previous-payload'
    Expand-Archive -LiteralPath $PreviousPackageZip -DestinationPath $initialPayload
    $previousVersion = [Diagnostics.FileVersionInfo]::GetVersionInfo((Join-Path $initialPayload 'HaloContinued.exe')).ProductVersion
    Assert-True ($previousVersion -match '^\d+\.\d+\.\d+$' -and [version]$previousVersion -lt [version]$version) 'Previous package must contain an older version.'
    & $Iscc '/Qp' '/FPreviousSetup' "/DPackageDir=$initialPayload" "/DReleaseVersion=$previousVersion" "/DNumericVersion=$previousVersion" "/DDestinationDir=$root" $scriptPath
    Assert-True ($LASTEXITCODE -eq 0) 'Previous-version fixture compilation failed.'
    $initialSetup = Join-Path $root 'PreviousSetup.exe'
  }

  $sdkRoot = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits/10/bin'
  $mt = Get-ChildItem -Path "$sdkRoot/*/x64/mt.exe" -File | Sort-Object FullName -Descending | Select-Object -First 1
  Assert-True ([bool]$mt) 'Windows SDK mt.exe is required for manifest verification.'
  $manifestPath = Join-Path $root 'setup.manifest'
  & $mt.FullName '-nologo' "-inputresource:$setup;#1" "-out:$manifestPath"
  Assert-True ($LASTEXITCODE -eq 0) 'Cannot extract installer manifest.'
  $manifest = [xml](Get-Content -LiteralPath $manifestPath -Raw)
  Assert-True ($manifest.assembly.trustInfo.security.requestedPrivileges.requestedExecutionLevel.level -eq 'asInvoker') 'Installer requests elevation.'

  $mutex = [Threading.Mutex]::new($false, $mutexName)
  Invoke-Setup -ExpectFailure
  Assert-True (!(Test-Path -LiteralPath $install)) 'Blocked setup installed files.'
  $mutex.Dispose(); $mutex = $null

  Invoke-Setup '/TASKS=""' -Installer $initialSetup
  Assert-True (Test-Path -LiteralPath "$install/HaloContinued.exe") 'Application was not installed.'
  Assert-True (Test-Path -LiteralPath $startLink) 'Start menu shortcut is missing.'
  Assert-True (!(Test-Path -LiteralPath $desktopLink)) 'Desktop shortcut was not opt-in.'
  Assert-True (!(Read-Startup)) 'Startup was not opt-in.'
  Assert-True (Test-Path -LiteralPath "HKCU:\$uninstallKey") 'Per-user uninstall registration is missing.'
  Assert-True (!(Test-Path -LiteralPath "HKLM:\$uninstallKey")) 'Installer created machine-wide registration.'
  Assert-True ((Get-FileHash -LiteralPath "$install/HaloContinued.exe").Hash -eq (Get-FileHash -LiteralPath "$initialPayload/HaloContinued.exe").Hash) 'Installed executable differs from the initial portable build.'
  if ($PreviousPackageZip) {
    Assert-True ((Get-ItemPropertyValue -LiteralPath "HKCU:\$uninstallKey" -Name DisplayVersion) -eq $previousVersion) 'Previous version was not registered.'
  }
  & $mt.FullName '-nologo' "-inputresource:$uninstaller;#1" "-out:$manifestPath"
  Assert-True ($LASTEXITCODE -eq 0) 'Cannot extract uninstaller manifest.'
  $manifest = [xml](Get-Content -LiteralPath $manifestPath -Raw)
  Assert-True ($manifest.assembly.trustInfo.security.requestedPrivileges.requestedExecutionLevel.level -eq 'asInvoker') 'Uninstaller requests elevation.'
  Assert-True ((Get-AuthenticodeSignature -LiteralPath $uninstaller).Status -eq 'NotSigned') 'Uninstaller is unexpectedly signed.'
  $shell = New-Object -ComObject WScript.Shell
  try { Assert-True ($shell.CreateShortcut($startLink).TargetPath -eq "$install\HaloContinued.exe") 'Shortcut points to the wrong app.' }
  finally { [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell) | Out-Null }

  # Simulate user data that must survive both upgrades and uninstall. No real
  # AppData folder is touched; the installer has no user-data deletion entries.
  [IO.Directory]::CreateDirectory("$install/HaloBatteryNext-data") | Out-Null
  'keep settings' | Set-Content -LiteralPath "$install/HaloBatteryNext-data/config.json"
  'keep history' | Set-Content -LiteralPath "$root/history.sqlite"
  New-Item -ItemType File -Path "$install/portable.txt" | Out-Null
  $command = '"' + $install + '\HaloContinued.exe" --background'
  Invoke-Setup '/TASKS="desktopicon,startup"'
  Assert-True ((Get-FileHash -LiteralPath "$install/HaloContinued.exe").Hash -eq (Get-FileHash -LiteralPath "$payload/HaloContinued.exe").Hash) 'Upgrade did not replace the executable with the current portable build.'
  Assert-True ([Diagnostics.FileVersionInfo]::GetVersionInfo("$install/HaloContinued.exe").ProductVersion -eq $version) 'Upgraded executable version is incorrect.'
  Assert-True ((Get-ItemPropertyValue -LiteralPath "HKCU:\$uninstallKey" -Name DisplayVersion) -eq $version) 'Upgrade did not update Installed apps version.'
  Assert-True (!(Test-Path -LiteralPath "$install/unins001.exe")) 'Upgrade created a second uninstaller.'
  Assert-True ((Get-Content -LiteralPath "$install/HaloBatteryNext-data/config.json" -Raw).Trim() -eq 'keep settings') 'Upgrade changed user settings.'
  Assert-True ((Get-Content -LiteralPath "$root/history.sqlite" -Raw).Trim() -eq 'keep history') 'Upgrade changed user history.'
  if ($PreviousPackageZip) { Write-Output "PASS: $previousVersion -> $version upgrade replaced the app, updated Installed apps and preserved settings/history." }
  Assert-True ((Read-Startup) -eq $command) 'Startup command or quoting is incorrect.'
  Assert-True (Test-Path -LiteralPath $desktopLink) 'Optional desktop shortcut is missing.'
  Remove-ItemProperty -LiteralPath $runKey -Name $startupName
  Invoke-Setup
  Assert-True (!(Read-Startup)) 'Upgrade re-enabled startup disabled in the app.'
  Invoke-Setup '/TASKS="startup"'
  Invoke-Setup
  Assert-True ((Read-Startup) -eq $command) 'Upgrade lost enabled startup.'
  Invoke-Setup '/TASKS=""'
  Assert-True (!(Read-Startup)) 'Explicitly disabling startup did not remove the owned entry.'

  $otherCommand = '"' + $root + '\portable\HaloContinued.exe" --background'
  Set-ItemProperty -LiteralPath $runKey -Name $startupName -Value $otherCommand
  Invoke-Setup
  Assert-True ((Read-Startup) -eq $otherCommand) 'Upgrade changed another copy''s startup entry.'
  $mutex = [Threading.Mutex]::new($false, $mutexName)
  Invoke-Uninstall -ExpectFailure
  Assert-True (Test-Path -LiteralPath "$install/HaloContinued.exe") 'Blocked uninstall removed the app.'
  $mutex.Dispose(); $mutex = $null
  Invoke-Uninstall
  Assert-True ((Read-Startup) -eq $otherCommand) 'Uninstall removed another copy''s startup entry.'
  Assert-True (!(Test-Path -LiteralPath "$install/HaloContinued.exe")) 'Uninstall left the executable.'
  Assert-True (!(Test-Path -LiteralPath "HKCU:\$uninstallKey")) 'Uninstall registration remains.'
  Assert-True (!(Test-Path -LiteralPath $startLink) -and !(Test-Path -LiteralPath $desktopLink)) 'Uninstall left shortcuts.'
  Assert-True ((Get-Content -LiteralPath "$install/HaloBatteryNext-data/config.json" -Raw).Trim() -eq 'keep settings') 'User settings were changed.'
  Assert-True ((Get-Content -LiteralPath "$root/history.sqlite" -Raw).Trim() -eq 'keep history') 'User history was changed.'
  Assert-True (Test-Path -LiteralPath "$install/portable.txt") 'Uninstall removed the portable marker.'
  Invoke-Setup '/TASKS="startup"'
  Invoke-Uninstall
  Assert-True (!(Read-Startup)) 'Uninstall left its own startup entry.'

  Write-Output 'PASS: clean packaging, unsigned asInvoker installer/uninstaller, mutex guards, per-user install, identical payload, optional shortcuts/startup, upgrade preferences, startup ownership and data-preserving uninstall.'
  Write-Output "Isolated test logs: $root"
} finally {
  if ($mutex) { $mutex.Dispose() }
  if (Test-Path -LiteralPath $uninstaller) { Invoke-Uninstall }
  Remove-ItemProperty -LiteralPath $runKey -Name $startupName -ErrorAction SilentlyContinue
  foreach ($link in @($startLink, $desktopLink)) {
    if (Test-Path -LiteralPath $link) { Remove-Item -LiteralPath $link -Force }
  }
}
