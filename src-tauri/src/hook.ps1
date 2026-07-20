# catty shell integration (PowerShell)
# Reports the current working directory to the terminal via OSC 7 on every prompt.
# Compatible with both Windows PowerShell 5.1 and PowerShell 7+ (avoids `e).

$__catty_OriginalPrompt = try {
    (Get-Command prompt -ErrorAction Stop).ScriptBlock
} catch {
    $null
}

function global:prompt {
    $cwd = $PWD.Path -replace '\\', '/'
    $osc = [char]27
    $bel = [char]7
    [Console]::Write("$osc]7;file://$env:COMPUTERNAME/$cwd$bel")
    if ($__catty_OriginalPrompt) {
        & $__catty_OriginalPrompt
    } else {
        "PS $($executionContext.SessionState.Path.CurrentLocation)$('>' * ($nestedPromptLevel + 1)) "
    }
}
