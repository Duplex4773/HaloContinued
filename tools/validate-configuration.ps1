param([string]$Executable)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (!$Executable) { $Executable = Join-Path $repo 'target/x86_64-pc-windows-msvc/release/HaloContinued.exe' }
# These APIs are unnecessary for device configuration. This regression gate
# complements protocol tests; it does not certify any anti-cheat's decisions.
$forbidden = '\b(OpenProcess|ReadProcessMemory|WriteProcessMemory|VirtualAllocEx|CreateRemoteThread(?:Ex)?|SetWindowsHookEx[AW]?|SendInput|mouse_event|keybd_event|RegisterRawInputDevices|GetRawInputData|CreateService[AW]?|StartService[AW]?)\b'
$sourceViolations = & rg --line-number --ignore-case --glob '*.rs' $forbidden (Join-Path $repo 'crates')
if ($LASTEXITCODE -eq 0) { throw "Forbidden production API reference: $sourceViolations" }
if ($LASTEXITCODE -ne 1) { throw 'Production source scan failed.' }
$manifest = [xml](Get-Content -LiteralPath (Join-Path $repo 'crates/app/app.manifest') -Raw)
$level = $manifest.assembly.trustInfo.security.requestedPrivileges.requestedExecutionLevel
if ($level.level -ne 'asInvoker' -or $level.uiAccess -ne 'false') { throw 'Application must remain unprivileged.' }
$dumpbin = Get-ChildItem -Path "${env:ProgramFiles}/Microsoft Visual Studio/*/*/VC/Tools/MSVC/*/bin/Hostx64/x64/dumpbin.exe" -File | Sort-Object FullName -Descending | Select-Object -First 1
if (!$dumpbin) { throw 'Visual Studio dumpbin is required to inspect release imports.' }
$imports = & $dumpbin.FullName /imports $Executable
if ($LASTEXITCODE -ne 0) { throw 'Release import inspection failed.' }
if ($imports -match $forbidden) { throw 'Forbidden API in release imports.' }
Write-Output 'Production source/imports pass configuration API restrictions; manifest uses asInvoker.'
