# Fetches Windows Terminal's ConPTY (conpty.dll + OpenConsole.exe, MIT) into
# installer\conpty: pinned version, checked against the SHA-512 NuGet
# publishes for it, and both binaries must carry a valid Microsoft
# Authenticode signature. Used by the release and conpty-check workflows.
$ErrorActionPreference = 'Stop'
$env:CONPTY_VERSION = '1.25.260930003'
$env:CONPTY_SHA512 = 'LuqMk9r5oTzeRne35IOlUdSQ7DkjorOIbcnODGZ81MvmUpTr/1VBQz3fAdCNCwkfuR0Mluq+H7rS3Hes/A1fOw=='
$pkg = Join-Path $env:RUNNER_TEMP 'conpty.zip'
$url = "https://api.nuget.org/v3-flatcontainer/microsoft.windows.console.conpty/$env:CONPTY_VERSION/microsoft.windows.console.conpty.$env:CONPTY_VERSION.nupkg"
Invoke-WebRequest $url -OutFile $pkg
$sha = [Convert]::ToBase64String([Security.Cryptography.SHA512]::Create().ComputeHash([IO.File]::ReadAllBytes($pkg)))
if ($sha -ne $env:CONPTY_SHA512) { throw "ConPTY package SHA-512 mismatch: $sha" }
$dir = Join-Path $env:RUNNER_TEMP 'conpty-pkg'
Expand-Archive -Path $pkg -DestinationPath $dir
New-Item -ItemType Directory -Force installer\conpty | Out-Null
Copy-Item (Join-Path $dir 'runtimes\win-x64\native\conpty.dll') installer\conpty\
Copy-Item (Join-Path $dir 'build\native\runtimes\x64\OpenConsole.exe') installer\conpty\
foreach ($f in Get-ChildItem installer\conpty) {
  $sig = Get-AuthenticodeSignature $f.FullName
  if ($sig.Status -ne 'Valid' -or $sig.SignerCertificate.Subject -notmatch 'O=Microsoft Corporation') {
    throw "$($f.Name): signature $($sig.Status), signer $($sig.SignerCertificate.Subject)"
  }
  "$($f.Name): $($sig.Status), $($sig.SignerCertificate.Subject)"
}
