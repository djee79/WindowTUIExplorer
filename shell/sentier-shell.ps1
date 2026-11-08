# sentier shell integration for PowerShell.
# Add to your $PROFILE:   . path\to\sentier-shell.ps1
# (Not named sentier.ps1: in the same folder as sentier.exe, PowerShell would run the
# script instead of the exe when you type `sentier`.)
# Then run `s`. Quit with Q to land the shell in sentier's current directory
# (q quits without changing directory).

function s {
    $f = New-TemporaryFile
    try {
        sentier.exe --cwd-file $f @args
        $d = Get-Content -LiteralPath $f -Raw
        if ($d -and $d -ne $PWD.Path) { Set-Location -LiteralPath $d }
    } finally {
        Remove-Item -LiteralPath $f -ErrorAction SilentlyContinue
    }
}
