<#
.SYNOPSIS
    Installs process-monitor on Windows as a scheduled task that starts at boot and keeps running.

.DESCRIPTION
    Registers a Task Scheduler task named "process-monitor" that runs process-monitor.exe as the SYSTEM
    account when Windows starts, whether or not anyone is logged in, and restarts it if it stops.
    The program reads monitor.toml from its own folder (scheduled tasks start in System32, and
    process-monitor falls back to the config next to the .exe).

    Put process-monitor.exe and monitor.toml in one folder (for example C:\process-monitor), test them with
    `process-monitor.exe check` and `process-monitor.exe test-notify`, then run this script from an
    administrator PowerShell window.

    Secrets written as ${NAME} in monitor.toml must be machine-wide environment variables, because the
    task runs as SYSTEM:
        [Environment]::SetEnvironmentVariable("SMTP_PASSWORD", "...", "Machine")
    Restart the task after changing them (Stop-ScheduledTask / Start-ScheduledTask process-monitor).

.PARAMETER Folder
    Folder containing process-monitor.exe and monitor.toml. Default: the folder this script is in, or
    its parent if the script is in a "contrib" subfolder.

.PARAMETER Uninstall
    Stop and remove the task instead of installing it.

.EXAMPLE
    .\install-windows.ps1 -Folder C:\process-monitor

.EXAMPLE
    .\install-windows.ps1 -Uninstall
#>
[CmdletBinding()]
param(
    [string]$Folder,
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'
$TaskName = 'process-monitor'

# Task Scheduler changes for SYSTEM tasks need an elevated (administrator) session.
$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this script from an administrator PowerShell window.'
}

if ($Uninstall) {
    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        Write-Host "Removed scheduled task '$TaskName'."
    } else {
        Write-Host "No scheduled task named '$TaskName'."
    }
    return
}

if (-not $Folder) {
    $Folder = $PSScriptRoot
    if ((Split-Path $Folder -Leaf) -eq 'contrib') { $Folder = Split-Path $Folder -Parent }
}
$Folder = (Resolve-Path $Folder).Path
$exe = Join-Path $Folder 'process-monitor.exe'
$config = Join-Path $Folder 'monitor.toml'
if (-not (Test-Path $exe)) { throw "process-monitor.exe not found in $Folder" }
if (-not (Test-Path $config)) { throw "monitor.toml not found in $Folder (copy monitor.example.toml and edit it)" }

# Fail now, with the program's own error message, rather than in a background task nobody sees.
& $exe --config $config validate | Out-Null
if ($LASTEXITCODE -ne 0) { throw "monitor.toml is not valid; run: `"$exe`" --config `"$config`" validate" }

$action = New-ScheduledTaskAction -Execute $exe -Argument "--config `"$config`"" -WorkingDirectory $Folder
$trigger = New-ScheduledTaskTrigger -AtStartup
$runAs = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
# No time limit, restart every minute if it exits, and don't stop on battery power (laptops).
$settings = New-ScheduledTaskSettingsSet `
    -ExecutionTimeLimit ([TimeSpan]::Zero) `
    -RestartCount 9999 -RestartInterval (New-TimeSpan -Minutes 1) `
    -StartWhenAvailable -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -MultipleInstances IgnoreNew

Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $runAs `
    -Settings $settings -Description 'Monitors websites and servers and sends alerts (process-monitor).' -Force | Out-Null
Start-ScheduledTask -TaskName $TaskName

Write-Host "Installed and started scheduled task '$TaskName' running $exe."
Write-Host "Check it with:  Get-ScheduledTaskInfo $TaskName   (outage log: see outage_log in monitor.toml)"
