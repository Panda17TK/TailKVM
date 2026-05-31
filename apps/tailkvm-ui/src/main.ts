import "./styles.css";
import { invoke } from "@tauri-apps/api/core";

const app = document.querySelector<HTMLDivElement>("#app")!;

app.innerHTML = `
  <main class="shell">
    <section class="hero">
      <div>
        <p class="eyebrow">Windows 11 + Tailscale Software KVM</p>
        <h1>TailKVM</h1>
        <p class="lead">
          Task 1: Rust workspace + Tauri v2 tray resident app.
        </p>
      </div>
      <div class="status-pill">TRAY READY</div>
    </section>

    <section class="grid">
      <article class="card">
        <h2>Runtime</h2>
        <p id="runtime-status">Not checked yet.</p>
        <button id="check-status">Check Rust backend</button>
      </article>

      <article class="card">
        <h2>Next Tasks</h2>
        <ul>
          <li>Task 2: tailscale status --json peer list</li>
          <li>Task 3: Windows monitor topology</li>
          <li>Task 4: TCP session over Tailscale</li>
          <li>Task 5: mouse move forwarding</li>
        </ul>
      </article>

      <article class="card full">
        <h2>Tray behavior</h2>
        <p>
          Closing the window hides TailKVM to the Windows task tray.
          Use the tray icon to reopen it, pause input forwarding, or quit.
        </p>
      </article>
    </section>
  </main>
`;

document
  .querySelector<HTMLButtonElement>("#check-status")!
  .addEventListener("click", async () => {
    const status = await invoke<string>("get_app_status");
    document.querySelector<HTMLParagraphElement>("#runtime-status")!.textContent = status;
  });
