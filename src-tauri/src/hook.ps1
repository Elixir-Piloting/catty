# catty shell integration (PowerShell)
# Reports the current working directory to the terminal via OSC 7.
# Compatible with both Windows PowerShell 5.1 and PowerShell 7+ (avoids `e).
#
# CRITICAL: PSReadLine calls prompt() on EVERY keystroke redraw (not just
# when a new prompt appears).  Writing OSC 7 directly to the console from
# inside prompt injects bytes that PSReadLine doesn't know about, silently
# desyncing its internal cursor-position bookkeeping — this is the root cause
# of the recurring cursor-jump bug.
#
# Instead, OSC 7 is emitted:
#   1. Once at startup (for the initial prompt).
#   2. From PSReadLine's AcceptLine key handler, AFTER the command is
#      submitted and PSReadLine's redraw cycle is complete.
#
# This removes the side-effect from prompt entirely, while still providing
# accurate per-command cwd tracking on the Rust/Osc7Scanner side.

$__catty_esc = [char]27
$__catty_bel = [char]7

# Emit OSC 7 for the initial prompt (startup).
[Console]::Write("$__catty_esc]7;file://$env:COMPUTERNAME/$($PWD.Path -replace '\\', '/')$__catty_bel")

$__catty_OriginalPrompt = try {
    (Get-Command prompt -ErrorAction Stop).ScriptBlock
} catch {
    $null
}

function global:prompt {
    # Pure function — returns a string, no side effects.
    if ($__catty_OriginalPrompt) {
        & $__catty_OriginalPrompt
    } else {
        "PS $($executionContext.SessionState.Path.CurrentLocation)$('>' * ($nestedPromptLevel + 1)) "
    }
}

# Emit OSC 7 after each command submission, not on every keystroke redraw.
if (Get-Command Set-PSReadLineKeyHandler -ErrorAction SilentlyContinue) {
    Set-PSReadLineKeyHandler -Chord Enter -ScriptBlock {
        param($key, $arg)
        [Microsoft.PowerShell.PSConsoleReadLine]::AcceptLine($key, $arg)
        [Console]::Write("$__catty_esc]7;file://$env:COMPUTERNAME/$($PWD.Path -replace '\\', '/')$__catty_bel")
    }
}