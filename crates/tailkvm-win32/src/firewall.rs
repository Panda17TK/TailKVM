use base64::{engine::general_purpose, Engine as _};
use std::{
    env,
    ptr::{null, null_mut},
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub fn install_firewall_rule(port: u16, remote_address: Option<String>) -> Result<String, String> {
    let exe_path =
        env::current_exe().map_err(|e| format!("failed to resolve current exe path: {e}"))?;

    let exe_path = exe_path
        .to_str()
        .ok_or_else(|| "current exe path is not valid UTF-8".to_string())?
        .to_string();

    let remote_address = remote_address
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "100.64.0.0/10".to_string());

    let rule_name = format!("TailKVM TCP {port}");

    let script = format!(
        r#"
$ErrorActionPreference = 'Stop'

$ruleName = '{rule_name}'
$program = '{program}'
$remoteAddress = '{remote_address}'
$port = {port}

Get-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue |
  Remove-NetFirewallRule

New-NetFirewallRule `
  -DisplayName $ruleName `
  -Direction Inbound `
  -Action Allow `
  -Protocol TCP `
  -LocalPort $port `
  -RemoteAddress $remoteAddress `
  -Program $program `
  -Profile Any | Out-Null

Write-Host ''
Write-Host 'TailKVM firewall rule installed.'
Write-Host "Rule: $ruleName"
Write-Host "Port: $port"
Write-Host "RemoteAddress: $remoteAddress"
Write-Host "Program: $program"
Write-Host ''
Read-Host 'Press Enter to close'
"#,
        rule_name = escape_ps_single_quote(&rule_name),
        program = escape_ps_single_quote(&exe_path),
        remote_address = escape_ps_single_quote(&remote_address),
        port = port
    );

    let encoded = encode_powershell_command(&script);
    let params = format!(
        "-NoProfile -ExecutionPolicy Bypass -EncodedCommand {}",
        encoded
    );

    let operation = to_wide("runas");
    let file = to_wide("powershell.exe");
    let params = to_wide(&params);

    let result = unsafe {
        ShellExecuteW(
            null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            null(),
            SW_SHOWNORMAL,
        )
    };

    let code = result as isize;

    if code <= 32 {
        Err(format!(
            "failed to start elevated PowerShell. ShellExecuteW code={code}"
        ))
    } else {
        Ok(format!(
            "Started elevated firewall installer. Port={port}, RemoteAddress={remote_address}"
        ))
    }
}

fn escape_ps_single_quote(value: &str) -> String {
    value.replace('\'', "''")
}

fn encode_powershell_command(script: &str) -> String {
    let mut bytes = Vec::with_capacity(script.len() * 2);

    for unit in script.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }

    general_purpose::STANDARD.encode(bytes)
}

fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
