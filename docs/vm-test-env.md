# Two-machine verification environment (Hyper-V VMs)

TailKVM is a software KVM: one PC's mouse/keyboard drives another over the
tailnet. End-to-end verification (hooks, `SendInput` injection, IME focus,
cursor confine, edge crossing) **requires two real desktops** — it cannot be
done on a single machine, because the controller's capture hooks and the
receiver's input injection would fight over the same OS input system.

Two Hyper-V Windows 11 VMs give you those two independent desktops on one host.
The protocol/session/clipboard layers are already covered locally without VMs
(`cargo test -p tailkvm-net` + the `fake_receiver` example, see
[single-machine-testing.md](single-machine-testing.md)); this runbook is for the
parts that genuinely need two machines.

## What is scripted vs. manual

| Step | How |
| --- | --- |
| Internal switch, 2 Gen2 VMs, TPM/Secure Boot, ISO mount | **scripted** — `tools/hyperv/provision-tailkvm-vms.ps1` |
| Windows 11 install / OOBE | manual (interactive) |
| Networking (static IPs or Tailscale) | manual |
| Install TailKVM v0.1.7 | manual |
| Run the #24 checklist | manual |

The interactive steps (Windows licensing, OOBE, Tailscale login) cannot be
automated from the host and are intentionally left to the operator.

## Prerequisites

- Hyper-V enabled on the host (this machine already has a hypervisor present).
  If `New-VM` is missing: enable the feature from an elevated shell and reboot —
  `Enable-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V-All -All`.
- A **Windows 11 x64 ISO** (e.g. from Microsoft's download page).
- ~16 GB free RAM (2 × 6 GB VMs) and ~130 GB disk (2 × 64 GB dynamic VHDX).
- An **elevated** PowerShell.

## 1. Create the VMs

```powershell
cd V:\src\tailkvm\tools\hyperv
.\provision-tailkvm-vms.ps1 -IsoPath D:\iso\Win11_x64.iso
```

This creates switch `TailKVM-Lab` and VMs `TailKVM-A` (controller) and
`TailKVM-B` (receiver). Re-running is safe: existing VMs/switch are skipped.

## 2. Install Windows on each VM

```powershell
Start-VM TailKVM-A; vmconnect localhost TailKVM-A
Start-VM TailKVM-B; vmconnect localhost TailKVM-B
```

Complete OOBE on both. For an offline local account during OOBE you can use
`Shift+F10` → `OOBE\BYPASSNRO` (or "I don't have internet" where offered).

> **CRITICAL — do not use Enhanced Session Mode.** Hyper-V's Enhanced Session
> is an RDP connection into the VM, and RDP changes input handling: low-level
> keyboard/mouse hooks, `SendInput`, the secure desktop, and foreground/IME
> focus all behave differently than at a real console. TailKVM is *about* those
> exact mechanics, so a passing/failing result under Enhanced Session is
> meaningless. Use the **Basic Session** (View menu → uncheck "Enhanced
> Session", or the toolbar toggle) so you are at the raw VM console.

## 3. Network the two VMs

Pick one path. **Tailscale is the faithful path** (it is what real users run,
and its IPs fall in `100.64.0.0/10`, which the app's "Install firewall rule"
button allows out of the box).

### Path A — Tailscale (recommended)

1. Switch each VM's network adapter to an **External** switch (or give the
   internal switch host-shared internet) so the VMs can reach the internet:
   `Connect-VMNetworkAdapter -VMName TailKVM-A -SwitchName <external-switch>`.
2. Install Tailscale in both VMs and sign in to the **same tailnet**.
3. Note each VM's `100.x.y.z` address (`tailscale ip -4`).

### Path B — internal switch only (no internet, simplest isolation)

The VMs already share the internal `TailKVM-Lab` switch. Give each a static IP
in the same subnet, e.g. on A `192.168.234.1/24` and B `192.168.234.2/24`
(Settings → Network → adapter properties, or `New-NetIPAddress`).

> The app's bundled firewall helper scopes inbound `47110` to the Tailscale
> CGNAT range (`100.64.0.0/10`). On the internal-switch subnet that rule will
> not match, so on the **receiver** VM add a rule for the lab subnet:
> ```powershell
> New-NetFirewallRule -DisplayName 'TailKVM lab' -Direction Inbound `
>   -Protocol TCP -LocalPort 47110 -RemoteAddress 192.168.234.0/24 -Action Allow
> ```

## 4. Install TailKVM v0.1.7 on both VMs

Download `TailKVM_0.1.7_x64-setup.exe` from the
[Releases](https://github.com/Panda17TK/TailKVM/releases) page into each VM and
install. (To test an unreleased build instead, run `npm run tauri build` on the
host and copy the installer from `target/release/bundle/`.)

## 5. Run the #24 verification checklist

Roles: **TailKVM-B = receiver**, **TailKVM-A = controller**.

1. On B: **Start receiver**. (Path B: confirm the lab firewall rule above.)
2. On A: enter B's IP (tailnet `100.x.y.z` or lab `192.168.234.2`) → **Connect**.
3. On A: position the peer tile, then **Start KVM**.
4. Work through the acceptance checklist in
   [issue #24](https://github.com/Panda17TK/TailKVM/issues/24): seamless edge
   crossing, Japanese IME (P0–P2), clipboard text **and image** (#9),
   stuck-key/modifier safety, and `Ctrl+Alt+Pause` failsafe.

### Controller-only smoke test (one VM)

To sanity-check the controller's capture→forward path without the second VM
doing injection, run the headless receiver on B (or even on the host) and watch
the wire output:

```powershell
cargo run -p tailkvm-net --example fake_receiver -- 47110
```

Connect the controller to that host and confirm crossing produces
`MouseSetPosition` / `KeyboardText` / `ClipboardImage` lines. This verifies the
controller half only; real injection still needs the two-VM setup above.

## Teardown

```powershell
Stop-VM TailKVM-A,TailKVM-B -TurnOff -Force
Remove-VM TailKVM-A,TailKVM-B -Force
Remove-VMSwitch TailKVM-Lab -Force
Remove-Item -Recurse -Force V:\hyperv\tailkvm
```
