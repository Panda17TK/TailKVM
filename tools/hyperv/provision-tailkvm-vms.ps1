<#
.SYNOPSIS
    Provision two Hyper-V Windows 11 VMs for TailKVM two-machine verification
    (issue #24). Creates an internal switch and two Gen2 VMs (TPM + Secure Boot)
    with a Windows 11 ISO mounted, ready for OOBE.

.DESCRIPTION
    Automates the parts that can be scripted: the internal virtual switch, the
    two VM shells, their disks, TPM/Secure Boot (required by Windows 11), and
    the ISO mount. Everything after this — Windows OOBE, networking, Tailscale,
    installing TailKVM, and running the verification checklist — is manual and
    documented in docs/vm-test-env.md.

    MUST be run from an ELEVATED PowerShell (Hyper-V cmdlets require admin).

.PARAMETER IsoPath
    Path to a Windows 11 x64 installation ISO.

.PARAMETER VmRoot
    Directory where VM disks are created. Default: V:\hyperv\tailkvm.

.PARAMETER MemoryGB
    Startup memory per VM in GB (default 6).

.PARAMETER DiskGB
    Dynamic VHDX max size per VM in GB (default 64).

.PARAMETER SwitchName
    Internal switch name (default "TailKVM-Lab"). An internal switch lets the
    two VMs reach each other without external networking; see the runbook for
    the Tailscale alternative.

.EXAMPLE
    # From an elevated PowerShell:
    .\provision-tailkvm-vms.ps1 -IsoPath D:\iso\Win11_x64.iso
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$IsoPath,
    [string]$VmRoot = 'V:\hyperv\tailkvm',
    [int]$MemoryGB = 6,
    [int]$DiskGB = 64,
    [string]$SwitchName = 'TailKVM-Lab'
)

$ErrorActionPreference = 'Stop'
$VmNames = @('TailKVM-A', 'TailKVM-B')

function Assert-Admin {
    $isAdmin = ([Security.Principal.WindowsPrincipal] `
        [Security.Principal.WindowsIdentity]::GetCurrent()
    ).IsInRole([Security.Principal.WindowsBuiltinRole]::Administrator)
    if (-not $isAdmin) {
        throw 'This script must be run from an elevated (Administrator) PowerShell.'
    }
}

function Assert-Prereqs {
    if (-not (Get-Command New-VM -ErrorAction SilentlyContinue)) {
        throw 'Hyper-V cmdlets not found. Enable the Hyper-V feature and reboot first.'
    }
    if (-not (Test-Path -LiteralPath $IsoPath)) {
        throw "Windows 11 ISO not found at: $IsoPath"
    }
}

function New-LabSwitch {
    if (Get-VMSwitch -Name $SwitchName -ErrorAction SilentlyContinue) {
        Write-Host "[=] Switch '$SwitchName' already exists."
        return
    }
    # Internal: host + both VMs can talk; no external NIC needed. For the
    # Tailscale path use an External switch instead (see the runbook).
    New-VMSwitch -Name $SwitchName -SwitchType Internal | Out-Null
    Write-Host "[+] Created internal switch '$SwitchName'."
}

function New-LabVm {
    param([string]$Name)

    if (Get-VM -Name $Name -ErrorAction SilentlyContinue) {
        Write-Host "[=] VM '$Name' already exists; skipping."
        return
    }

    $vmDir = Join-Path $VmRoot $Name
    New-Item -ItemType Directory -Force -Path $vmDir | Out-Null
    $vhdPath = Join-Path $vmDir "$Name.vhdx"

    New-VM -Name $Name -Generation 2 -MemoryStartupBytes ($MemoryGB * 1GB) `
        -NewVHDPath $vhdPath -NewVHDSizeBytes ($DiskGB * 1GB) `
        -SwitchName $SwitchName | Out-Null

    # Windows 11 needs >=2 vCPU, TPM 2.0 and Secure Boot.
    Set-VM -Name $Name -ProcessorCount 2 `
        -CheckpointType Disabled -AutomaticCheckpointsEnabled $false
    Set-VMMemory -Name $Name -DynamicMemoryEnabled $true `
        -MinimumBytes 2GB -StartupBytes ($MemoryGB * 1GB) -MaximumBytes ($MemoryGB * 1GB)

    # Key protector is required before the vTPM can be enabled.
    if (-not (Get-HgsGuardian -Name 'TailKVMLabGuardian' -ErrorAction SilentlyContinue)) {
        New-HgsGuardian -Name 'TailKVMLabGuardian' -GenerateCertificates | Out-Null
    }
    $guardian = Get-HgsGuardian -Name 'TailKVMLabGuardian'
    $kp = New-HgsKeyProtector -Owner $guardian -AllowUntrustedRoot
    Set-VMKeyProtector -VMName $Name -KeyProtector $kp.RawData
    Enable-VMTPM -VMName $Name

    # Mount the ISO and boot from it first.
    Add-VMDvdDrive -VMName $Name -Path $IsoPath
    $dvd = Get-VMDvdDrive -VMName $Name
    Set-VMFirmware -VMName $Name -FirstBootDevice $dvd `
        -EnableSecureBoot On -SecureBootTemplate 'MicrosoftWindows'

    Write-Host "[+] Created VM '$Name' (mem ${MemoryGB}GB, disk ${DiskGB}GB, ISO mounted)."
}

Assert-Admin
Assert-Prereqs
New-Item -ItemType Directory -Force -Path $VmRoot | Out-Null
New-LabSwitch
foreach ($name in $VmNames) { New-LabVm -Name $name }

Write-Host ''
Write-Host '=== Done. Next steps (see docs/vm-test-env.md) ==='
Write-Host '1. Start each VM and complete Windows 11 OOBE:'
Write-Host '     Start-VM TailKVM-A; vmconnect localhost TailKVM-A'
Write-Host '   IMPORTANT: use the BASIC VMConnect console, NOT Enhanced Session'
Write-Host '   Mode (RDP) - enhanced session changes input semantics and breaks'
Write-Host '   the low-level hooks / SendInput TailKVM relies on.'
Write-Host '2. Network the two VMs (internal switch + static IPs, or Tailscale).'
Write-Host '3. Install TailKVM v0.1.7 on both, then run the #24 checklist.'
