// The full application markup. Kept verbatim from the original monolith so every
// DOM id, class and user-facing string is preserved exactly. `mountApp` installs
// it into #app; all feature wiring runs after this.

export const APP_HTML = `
  <main class="shell">
    <section class="hero">
      <div>
        <p class="eyebrow">Windows 11 + Tailscale Software KVM</p>
        <h1>TailKVM</h1>
        <p class="lead">
          複数の Windows PC でマウス・キーボード・クリップボードを Tailscale 経由で共有します。
        </p>
      </div>
      <div class="hud">
        <div class="hud-cell">
          <span class="hud-k">SELF NODE</span>
          <span class="hud-v mono" id="hud-self">—</span>
        </div>
        <div class="hud-cell">
          <span class="hud-k">LINK</span>
          <span class="hud-v" id="hud-link"><i class="hud-lamp"></i>OFFLINE</span>
        </div>
        <div class="hud-cell">
          <span class="hud-k">PEERS</span>
          <span class="hud-v mono" id="hud-peers">0</span>
        </div>
        <div class="status-pill">TRAY READY</div>
      </div>
    </section>

    <section class="card full quick-start">
      <h2>クイックスタート / Quick start</h2>
      <details class="qs-desc">
        <summary>使い方 / How to use</summary>
        <p class="qs-help">
          <b>操作する側</b>：① 相手PCの Tailscale IP を入れて接続 → ② 相手の位置をドラッグで指定 →
          ③「KVM操作を開始」。マウスを指定した<b>画面端まで動かすと相手PCを操作</b>でき、端で戻ると自分に戻ります。<br />
          <b>操作される側</b>：このPCを操作させるなら「受信を開始」を押して待ち受けます。
        </p>
      </details>

      <p class="qs-help">このPCの Tailscale IP（相手側で入力する値）: <b id="qs-self-ip">取得中...</b></p>

      <div class="qs-row" data-step="RX">
        <span class="qs-inline-label">このPCを操作される側にする：</span>
        <button id="qs-receiver">受信を開始 / Start receiver</button>
        <span id="qs-receiver-state" class="qs-state"></span>
      </div>

      <div class="qs-row" data-step="01">
        <input id="qs-host" type="text" placeholder="100.x.y.z (相手PCの Tailscale IP)" />
        <button id="qs-connect">接続 / Connect</button>
        <span id="qs-conn" class="qs-state">未接続</span>
      </div>

      <div class="qs-row qs-monitors-row" data-step="02">
        <strong>相手PC の位置 ／ このPCのモニター構成</strong>
        <div id="qs-monitors" class="qs-monitors">読込中...</div>
      </div>

      <div class="qs-kvm" data-step="03">
        <div class="qs-inline-label qs-kvm-hint">
          上のモニタ地図で<b>相手PCタイルをドラッグ</b>して位置を決め、「KVM操作を開始」。
        </div>
        <div class="qs-kvm-controls">
          <button id="qs-kvm-start">KVM操作を開始</button>
          <button id="qs-kvm-stop">停止 / Stop</button>
          <button id="qs-emergency" title="全転送停止＋カーソル解放＋被制御セッション切断（トレイの Emergency reset と同じ）">緊急リセット</button>
          <label class="qs-speed">
            操作速度
            <input id="qs-kvm-gain" type="range" min="0.5" max="4" step="0.1" value="1.8" />
            <span id="qs-kvm-gain-val">1.8×</span>
          </label>
          <span id="qs-status" class="qs-state"></span>
        </div>
      </div>

      <details class="qs-checklist-details">
        <summary>接続できない時のチェック（「connection refused」等）</summary>
        <ul>
          <li>① <b>相手PC（操作される側）でも TailKVM を起動</b>し「受信を開始」している。</li>
          <li>② 相手PCで <b>Install firewall rule</b> を一度実行（47110 の受信許可）。詳細設定にあります。</li>
          <li>③ 入れる IP は<b>相手PCの Tailscale IP</b>（このPCのIPではない）。</li>
        </ul>
      </details>

      <div class="qs-toggles">
        <button id="qs-toggle-status" class="qs-advanced-toggle" type="button">
          状態（Runtime / Tailscale / Keyboard / モニタ / Peers）を表示 ▼
        </button>
        <button id="qs-toggle-advanced" class="qs-advanced-toggle" type="button">
          詳細設定（テスト/ルータ/Raw入力/クリップボード）を表示 ▼
        </button>
      </div>
    </section>

    <section class="grid">
      <article class="card status-card">
        <h2>Runtime</h2>
        <p id="runtime-status">Not checked yet.</p>
        <button id="check-status">Check Rust backend</button>
      </article>

      <article class="card status-card">
        <h2>Tailscale</h2>
        <p id="tailscale-summary">Not loaded yet.</p>
        <button id="refresh-tailscale">Refresh peers</button>
      </article>

      <article class="card status-card">
        <h2>Keyboard Layout</h2>
        <p id="keyboard-layout-summary">Not checked yet.</p>
        <button id="refresh-keyboard-layout">Check keyboard layout</button>
      </article>

      <article class="card full advanced">
        <h2>TCP Session（詳細 / Advanced）</h2>
        <p id="tcp-summary">Not started yet.</p>

        <div class="tcp-controls">
          <label>
            Peer Tailscale IP
            <input id="tcp-host" type="text" placeholder="100.x.y.z" />
          </label>

          <label>
            Port
            <input id="tcp-port" type="number" value="47110" min="1" max="65535" />
          </label>

          <button id="start-receiver">Start receiver</button>
          <button id="connect-peer">Connect peer</button>
          <button id="disconnect-peer">Disconnect</button>
          <button id="discover-peers">Discover peers</button>
          <button id="refresh-tcp">Refresh TCP state</button>

          <label class="checkbox-label">
            <input id="accept-incoming" type="checkbox" checked />
            Accept incoming connections
          </label>

          <label class="checkbox-label">
            <input id="tailnet-only" type="checkbox" />
            Bind to Tailscale IP only (not 0.0.0.0)
          </label>

          <label>
            Pairing token (optional)
            <input id="auth-token" type="password" placeholder="shared secret (blank = off)" autocomplete="off" />
          </label>

          <div id="discovered-peers" class="tcp-state empty">No discovery yet.</div>
          <div id="lock-state" class="tcp-state empty">Local input: unknown</div>

          <label>
            Screen name (multi)
            <input id="screen-name" type="text" placeholder="peer-pc" />
          </label>
          <label>
            Screen host
            <input id="screen-host" type="text" placeholder="100.x.y.z" />
          </label>
          <button id="connect-screen">Connect screen</button>
          <button id="disconnect-screen">Disconnect screen</button>
          <button id="list-screens">List screens</button>
          <div id="screen-list" class="tcp-state empty">No screens.</div>

          <label>
            Local screen name
            <input id="router-local-name" type="text" value="local" />
          </label>
          <button id="start-router">Start router (right-chain)</button>
          <button id="stop-router">Stop router</button>

          <label>
            Saved layout (JSON)
            <textarea id="layout-json" rows="6" spellcheck="false"
              placeholder='{"screens":[{"name":"local","is_local":true},{"name":"bob","host":"100.x.y.z","width":1920,"height":1080}],"links":[{"from":"local","edge":"right","to":"bob"}],"auto_connect":false}'></textarea>
          </label>
          <button id="load-layout">Load layout</button>
          <button id="save-layout">Save layout</button>

          <div class="layout-editor">
            <h4>Visual layout (local on the left, screens chained right)</h4>
            <div id="le-row" class="le-row"></div>
            <label>
              Add screen name
              <input id="le-name" type="text" placeholder="peer-pc" />
            </label>
            <label>
              host
              <input id="le-host" type="text" placeholder="100.x.y.z" />
            </label>
            <button id="le-add">Add screen</button>
            <button id="le-apply">Apply (connect all + start router)</button>
            <button id="le-reconfigure">Reconfigure live</button>
            <button id="le-save">Save visual layout</button>
          </div>

          <div class="layout-editor">
            <h4>2D placement editor (drag screens; links inferred from adjacency)</h4>
            <div id="editor-2d" class="editor-2d"></div>
            <label>
              Add screen name
              <input id="e2-name" type="text" placeholder="peer-pc" />
            </label>
            <label>
              host
              <input id="e2-host" type="text" placeholder="100.x.y.z" />
            </label>
            <button id="e2-add">Add screen</button>
            <button id="e2-reset-local">Reset to local only</button>
            <button id="e2-clear">Clear</button>
            <button id="e2-save">Save</button>
            <button id="e2-apply">Apply live</button>
          </div>

          <label>
            Firewall remote
            <input id="firewall-remote" type="text" value="100.64.0.0/10" />
          </label>

          <button id="install-firewall">Install firewall rule</button>

          <label>
            Mouse dx
            <input id="mouse-dx" type="number" value="80" min="-1000" max="1000" />
          </label>

          <label>
            Mouse dy
            <input id="mouse-dy" type="number" value="0" min="-1000" max="1000" />
          </label>

          <button id="send-mouse-test">Test mouse move</button>
          <button id="send-left-click-test">Test left click</button>
          <button id="send-right-click-test">Test right click</button>
          <button id="send-middle-click-test">Test middle click</button>
          <button id="send-x1-click-test">Test X1 click</button>
          <button id="send-x2-click-test">Test X2 click</button>
          <button id="send-left-double-click-test">Test left double click</button>
                    <label>
            Mouse gain
            <input id="mouse-gain" type="number" value="1.00" min="0.10" max="4.00" step="0.10" />
          </label>

          <label>
            Capture interval ms
            <input id="capture-interval-ms" type="number" value="8" min="8" max="100" />
          </label>

          <label>
            Max delta
            <input id="max-delta" type="number" value="80" min="10" max="500" />

          </label>

                    <label class="checkbox-label">
            <input id="remote-mode" type="checkbox" checked />
            Remote mode
          </label>

          <label class="checkbox-label">
            <input id="use-raw-input" type="checkbox" />
            Raw Input mouse (PoC)
          </label>

          <label class="checkbox-label">
            <input id="seamless-mode" type="checkbox" />
            Seamless absolute mode (PoC)
          </label>

          <label>
            Edge dwell ms (0=instant)
            <input id="edge-dwell-ms" type="number" value="0" min="0" max="2000" />
          </label>

          <label>
            Dead corner px (0=off)
            <input id="dead-corner-px" type="number" value="0" min="0" max="1000" />
          </label>

          <label>
            Switch edge
            <select id="switch-edge">
              <option value="right" selected>right</option>
              <option value="left">left</option>
              <option value="top">top</option>
              <option value="bottom">bottom</option>
            </select>
          </label>

          <label>
            Edge margin px
            <input id="edge-margin" type="number" value="3" min="1" max="64" />
          </label>

          <button id="start-mouse-capture">Capture mouse</button>
          <button id="stop-mouse-capture">Stop capture</button>

          <label>
            Keyboard text
            <input id="keyboard-text" type="text" value="hello tailkvm" maxlength="200" />
          </label>

          <button id="send-keyboard-text">Send keyboard text</button>
          <button id="send-key-enter">Test Enter</button>
          <button id="send-key-backspace">Test Backspace</button>
          <button id="send-key-tab">Test Tab</button>
          <button id="send-key-escape">Test Escape</button>

          <button id="start-keyboard-hook-capture">Capture keyboard</button>
          <button id="stop-keyboard-hook-capture">Stop keyboard capture</button>

          <label class="checkbox-label">
            <input id="resolve-characters" type="checkbox" />
            Resolve characters (JIS/US bridge)
          </label>

          <button id="send-clipboard-text">Send clipboard to peer</button>
          <button id="send-clipboard-image">Send clipboard image to peer</button>

          <label class="checkbox-label">
            <input id="clipboard-sync" type="checkbox" />
            Auto clipboard sync (bidirectional)
          </label>

          <button id="start-raw-mouse-diagnostic">Raw Input diagnostic (PoC)</button>
          <button id="stop-raw-mouse-diagnostic">Stop Raw Input diagnostic</button>
        </div>

        <div id="tcp-state" class="tcp-state empty">Not loaded yet.</div>
      </article>

      <article class="card full advanced">
        <h2>日本語IME入力（詳細 / Advanced）</h2>
        <p id="ime-status">IME composition mode: off</p>
        <p>
          文字解決ONの状態で半角/全角キーを押すと composition mode に入ります。
          変換はローカルIMEで行い、確定文字のみ相手PCへ送信します。
        </p>

        <div class="layout-controls">
          <label>
            プリセット
            <select id="ime-preset">
              <option value="" selected>（選択して一括適用）</option>
              <option value="standard_japanese">標準（日本語優先）</option>
              <option value="preserve_current">現状維持</option>
              <option value="last_session">前回の状態</option>
            </select>
          </label>

          <label>
            候補ウィンドウ位置
            <select id="ime-candidate-position">
              <option value="remote_projected">リモートカーソル位置を投影（推奨）</option>
              <option value="lock_near">ロック位置の近傍</option>
              <option value="monitor_center">現在モニタ中央</option>
              <option value="fixed">固定座標</option>
              <option value="legacy_top_left">従来互換（左上）</option>
            </select>
          </label>

          <label>
            IME open policy
            <select id="ime-open-policy">
              <option value="force_japanese">force_japanese（推奨）</option>
              <option value="preserve_current">preserve_current</option>
              <option value="restore_last_tailkvm">restore_last_tailkvm</option>
              <option value="manual">manual</option>
            </select>
          </label>

          <label>
            Conversion mode policy
            <select id="ime-conversion-policy">
              <option value="native_default">native_default（推奨）</option>
              <option value="native_fullshape">native_fullshape（互換）</option>
              <option value="preserve">preserve</option>
              <option value="last_used">last_used</option>
            </select>
          </label>

          <label>
            フォーカス取得失敗時
            <select id="ime-focus-policy">
              <option value="retry">retry（推奨）</option>
              <option value="warn_continue">warn_continue</option>
              <option value="abort">abort</option>
            </select>
          </label>

          <label>
            固定座標 X（fixed 用）
            <input id="ime-fixed-x" type="number" value="0" step="1" />
          </label>

          <label>
            固定座標 Y（fixed 用）
            <input id="ime-fixed-y" type="number" value="0" step="1" />
          </label>

          <label>
            capture window サイズ(px)
            <select id="ime-window-size">
              <option value="1">1（既定）</option>
              <option value="2">2</option>
              <option value="8">8</option>
            </select>
          </label>

          <label>
            lock_near オフセット(px)
            <input id="ime-lock-offset" type="number" value="24" min="0" max="256" step="1" />
          </label>
        </div>
      </article>

      <article class="card full advanced">
        <h2>Display Layout Editor（詳細 / Advanced）</h2>
        <p id="layout-summary">
          Arrange the remote display like Windows display settings. This layout will be used for edge mapping.
        </p>

        <div class="layout-controls">
          <label>
            Target peer
            <select id="layout-peer">
              <option value="">Select peer...</option>
            </select>
          </label>

          <label>
            Remote width
            <input id="layout-remote-width" type="number" value="1920" min="640" max="10000" />
          </label>

          <label>
            Remote height
            <input id="layout-remote-height" type="number" value="1080" min="480" max="10000" />
          </label>

          <label>
            Canvas scale
            <input id="layout-scale" type="number" value="0.12" min="0.03" max="0.40" step="0.01" />
          </label>

          <button id="reset-layout">Reset layout</button>
          <button id="apply-layout">Use layout</button>
        </div>

        <div id="layout-canvas" class="layout-canvas empty">
          Load monitors and Tailscale peers first.
        </div>
      </article>

      <article class="card full status-card">
        <h2>Monitor Topology</h2>
        <p id="monitor-summary">Not loaded yet.</p>
        <button id="refresh-monitors">Refresh monitors</button>
        <div id="monitor-list" class="monitor-list empty">Not loaded yet.</div>
      </article>

      <article class="card full status-card">
        <h2>This machine</h2>
        <div id="self-node" class="empty">Not loaded yet.</div>
      </article>

      <article class="card full status-card">
        <h2>Peers</h2>
        <div id="peer-list" class="peer-list empty">Not loaded yet.</div>
      </article>
    </section>
  </main>
`;

export function mountApp(): void {
  const app = document.querySelector<HTMLDivElement>("#app")!;
  app.innerHTML = APP_HTML;
}
