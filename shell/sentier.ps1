# sentier shell integration for PowerShell.
# Add to your $PROFILE:   . path\to\sentier.ps1
# Then run `s`. Quit with Q to land the shell in sentier's current directory
# (q quits without changing directory).

function s {
    $f = New-TemporaryFile
    try {
        sentier --cwd-file $f @args
        $d = Get-Content -LiteralPath $f -Raw
        if ($d -and $d -ne $PWD.Path) { Set-Location -LiteralPath $d }
    } finally {
        Remove-Item -LiteralPath $f -ErrorAction SilentlyContinue
    }
}
