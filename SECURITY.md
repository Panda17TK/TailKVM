# Security Policy

## Threat model

TailKVM forwards **real mouse and keyboard input** and **clipboard contents** between
two machines. A connected controller can fully operate the receiver. Treat a TailKVM
link with the same trust as handing someone your keyboard.

Design choices that scope the exposure:

- Transport runs over your **Tailscale** tailnet (TCP, default port `47110`) and the
  firewall helper scopes inbound access to the Tailscale CGNAT range
  (`100.64.0.0/10`). TailKVM does not open itself to the public internet.
- The receiver is **single-slot** ("newest-wins"): a new controller session replaces
  any previous one rather than allowing several simultaneous controllers.
- A local **Ctrl + Alt + Pause** failsafe immediately stops all capture.

## Recommendations for users

- Only connect machines **you own and trust**, on a tailnet **you control**.
- Keep Tailscale ACLs tight so only intended devices can reach port `47110`.
- Do not run TailKVM as a receiver on a shared/untrusted machine.

## Reporting a vulnerability

Please **do not** open a public issue for security problems.

Use GitHub's private vulnerability reporting:
**[Report a vulnerability](https://github.com/Panda17TK/TailKVM/security/advisories/new)**
(Security tab → "Report a vulnerability").

Include the affected version, your OS, and reproduction steps. We aim to acknowledge
reports promptly and will coordinate a fix and disclosure timeline with you.
