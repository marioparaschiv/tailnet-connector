# Removes what install.ps1 set up: the scheduled task, the running connector, the installed
# files and the TAILNET_URL / TAILNET_LOG user variables. Does nothing that isn't there.
#
#   powershell -ExecutionPolicy Bypass -File uninstall.ps1

$ErrorActionPreference = 'Stop'

$TaskName = 'tailnet-connector'
$InstallDir = Join-Path $env:LOCALAPPDATA 'tailnet-connector'
$InstalledExe = Join-Path $InstallDir 'tailnet-connector.exe'

if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
    Stop-ScheduledTask -TaskName $TaskName
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
}

# Stopping the task doesn't wait for the process, and the exe can't be replaced while it runs.
Get-Process -Name 'tailnet-connector' -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -eq $InstalledExe } |
    ForEach-Object { $_.Kill(); $_.WaitForExit() }

if (Test-Path -LiteralPath $InstallDir) {
    Remove-Item -LiteralPath $InstallDir -Recurse -Force
}

[Environment]::SetEnvironmentVariable('TAILNET_URL', $null, 'User')
[Environment]::SetEnvironmentVariable('TAILNET_LOG', $null, 'User')

Write-Host "Uninstalled $TaskName"
