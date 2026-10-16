# Installs tailnet-connector for the current user as a scheduled task that starts hidden at logon.
# Re-running replaces the installed exe and task.
#
#   powershell -ExecutionPolicy Bypass -File install.ps1 [-Exe <path>] [-TailnetUrl <url>]
#
# The exe is built on Linux with `cargo build --release --target x86_64-pc-windows-gnu`. It has
# no console, so its output and that of the MCP servers it starts goes to the log in TAILNET_LOG.
param(
    [string]$Exe = (Join-Path $PSScriptRoot 'tailnet-connector.exe'),
    [string]$TailnetUrl = 'http://100.65.233.54:8790'
)

$ErrorActionPreference = 'Stop'

$TaskName = 'tailnet-connector'
$InstallDir = Join-Path $env:LOCALAPPDATA 'tailnet-connector'
$InstalledExe = Join-Path $InstallDir 'tailnet-connector.exe'
$Log = Join-Path $InstallDir 'connector.log'
$User = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name

if (-not (Test-Path -LiteralPath $Exe -PathType Leaf)) {
    throw "$Exe not found; pass -Exe <path to tailnet-connector.exe>"
}

& (Join-Path $PSScriptRoot 'uninstall.ps1')

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Copy-Item -LiteralPath $Exe -Destination $InstalledExe -Force

# The task scheduler starts tasks with the user's environment, so this is how the exe gets its config.
[Environment]::SetEnvironmentVariable('TAILNET_URL', $TailnetUrl, 'User')
[Environment]::SetEnvironmentVariable('TAILNET_LOG', $Log, 'User')

$action = New-ScheduledTaskAction -Execute $InstalledExe -WorkingDirectory $InstallDir
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $User
$principal = New-ScheduledTaskPrincipal -UserId $User -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet `
    -ExecutionTimeLimit ([TimeSpan]::Zero) `
    -RestartCount 999 `
    -RestartInterval (New-TimeSpan -Minutes 1) `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -StartWhenAvailable `
    -MultipleInstances IgnoreNew

Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings | Out-Null
Start-ScheduledTask -TaskName $TaskName

Write-Host "Installed $InstalledExe as scheduled task $TaskName, connecting to $TailnetUrl; log: $Log"
