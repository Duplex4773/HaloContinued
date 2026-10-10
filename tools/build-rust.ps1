param(
  [ValidateSet('baseline', 'avx2', 'avx512')]
  [string]$InstructionSet = 'baseline',
  [switch]$NativeBenchmark,
  [switch]$InsightsBenchmark
)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$previousFlags = $env:CARGO_ENCODED_RUSTFLAGS
$features = switch ($InstructionSet) {
  'baseline' { '+crt-static' }
  'avx2' { '+crt-static,+avx2' }
  'avx512' { '+crt-static,+avx2,+avx512f,+avx512bw,+avx512cd,+avx512dq,+avx512vl' }
}
$flags = @('-C', "target-feature=$features",
  "--remap-path-prefix=$repo=/workspace")
# Remove local build-account paths from panic messages and other embedded strings.
# Resolve these at build time; never record a machine's account or profile in source.
if ($env:USERPROFILE) {
  $flags += "--remap-path-prefix=$env:USERPROFILE=/build/home"
}
if ($env:CARGO_HOME) {
  $flags += "--remap-path-prefix=$env:CARGO_HOME=/build/cargo"
}
$env:CARGO_ENCODED_RUSTFLAGS = $flags -join [char]31
Push-Location $repo
try {
  cargo build --release --locked
  if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
  if ($NativeBenchmark) {
    cargo test -p halo-continued --release --locked --no-run
    if ($LASTEXITCODE -ne 0) { throw 'Native benchmark build failed.' }
  }
  if ($InsightsBenchmark) {
    cargo build -p hb-storage --example isa_benchmark --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'Insights benchmark build failed.' }
  }
} finally {
  Pop-Location
  $env:CARGO_ENCODED_RUSTFLAGS = $previousFlags
}
