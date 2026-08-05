# installed by karvex
# managed by karvex; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# KARVEX_INTEGRATION_ID=droid
# KARVEX_INTEGRATION_VERSION=2

param([string]$Action = "")

if ($Action -ne "session") { exit 0 }
if ($env:KARVEX_ENV -ne "1") { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:KARVEX_PANE_ID)) { exit 0 }

$inputText = [Console]::In.ReadToEnd()
try {
    $payload = if ([string]::IsNullOrWhiteSpace($inputText)) { $null } else { $inputText | ConvertFrom-Json }
} catch {
    $payload = $null
}

if ($null -eq $payload -or [string]::IsNullOrWhiteSpace($payload.session_id)) { exit 0 }

$seq = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
try {
    & kvx pane report-agent-session $env:KARVEX_PANE_ID --source karvex:droid --agent droid --agent-session-id $payload.session_id --seq $seq 2>$null | Out-Null
} catch {
}
