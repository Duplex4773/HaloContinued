#requires -Version 7.0
param(
  [ValidatePattern('^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$')]
  [string]$Version = '1.0.10',
  [string]$Executable,
  [string]$Iscc
)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (!$Iscc) {
  $compiler = Get-Command ISCC.exe -ErrorAction SilentlyContinue
  if ($compiler) { $Iscc = $compiler.Source }
  else {
    $candidates = @(
      "$env:LOCALAPPDATA/Programs/Inno Setup 7/ISCC.exe",
      "$env:LOCALAPPDATA/Programs/Inno Setup 6/ISCC.exe",
      "${env:ProgramFiles}/Inno Setup 7/ISCC.exe",
      "${env:ProgramFiles(x86)}/Inno Setup 6/ISCC.exe"
    )
    $Iscc = $candidates | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
  }
}
if (!$Iscc) { throw 'Install Inno Setup 6.3 or newer from https://jrsoftware.org/isinfo.php, or supply -Iscc with the path to ISCC.exe.' }
$Iscc = (Resolve-Path -LiteralPath $Iscc).Path
# Generate both formats from the same fresh, privacy-checked payload.
& (Join-Path $PSScriptRoot 'package-rust.ps1') -Version $Version -Executable $Executable | Out-Host
$dist = (Resolve-Path -LiteralPath (Join-Path $repo 'dist')).Path
$zip = Join-Path $dist "HaloContinued-$Version-windows-x64.zip"
$stage = Join-Path $dist ('.installer-' + [guid]::NewGuid().ToString('N'))
try {
  Expand-Archive -LiteralPath $zip -DestinationPath $stage
  $numericVersion = ($Version -split '[-+]')[0]
  & $Iscc '/Qp' "/DPackageDir=$stage" "/DReleaseVersion=$Version" "/DNumericVersion=$numericVersion" "/DDestinationDir=$dist" (Join-Path $repo 'installer/HaloContinued.iss')
  if ($LASTEXITCODE -ne 0) { throw "Inno Setup compilation failed ($LASTEXITCODE)." }
  $setup = Join-Path $dist "HaloContinued-$Version-windows-x64-setup.exe"
  if ((Get-AuthenticodeSignature -LiteralPath $setup).Status -ne 'NotSigned') {
    throw 'Expected an unsigned installer.'
  }
  $standalone = Join-Path $dist "HaloContinued-$Version-windows-x64.exe"
  Copy-Item -LiteralPath (Join-Path $stage 'HaloContinued.exe') -Destination $standalone -Force
  $checksums = foreach ($artifact in @($setup, $zip, $standalone)) {
    '{0}  {1}' -f (Get-FileHash -LiteralPath $artifact -Algorithm SHA256).Hash, [IO.Path]::GetFileName($artifact)
  }
  $checksums | Set-Content -LiteralPath (Join-Path $dist "HaloContinued-$Version-SHA256SUMS.txt") -Encoding ascii
  Get-Item -LiteralPath $setup, $zip, $standalone | Select-Object FullName,Length
} finally {
  $resolved = [IO.Path]::GetFullPath($stage)
  if (!$resolved.StartsWith($dist + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase) -or
      ![IO.Path]::GetFileName($resolved).StartsWith('.installer-')) {
    throw 'Refusing to remove an unexpected installer staging path.'
  }
  if (Test-Path -LiteralPath $resolved) { Remove-Item -LiteralPath $resolved -Recurse -Force }
}
