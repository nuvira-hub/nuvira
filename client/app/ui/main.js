// Interfaccia di NexoDesk: riceve lo stato dal processo Rust (evento "state")
// e invia i comandi con invoke.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

const STATUS_TEXT = {
  setup: "Imposta il server NexoDesk (Impostazioni)",
  connecting: "Collegamento al server…",
  online: "Pronto: raggiungibile con questo ID",
  offline: "Non collegato al server",
};

let current = null;
let countdownTimer = null;

function formatId(id) {
  return id ? `${id.slice(0, 3)} ${id.slice(3, 6)} ${id.slice(6)}` : "— — —";
}

function render(state) {
  current = state;
  $("status-dot").className = `dot ${state.status}`;
  let text = STATUS_TEXT[state.status] ?? state.status;
  if (state.status === "online" && state.websocket) text += " (via WebSocket)";
  if (state.status === "offline" && state.error) text += `: ${state.error}`;
  // Con il servizio, finché il suo agent non risponde si mostra il motivo.
  if (state.service && state.status === "connecting" && state.error) text = state.error;
  $("status-text").textContent = text;
  $("service-note").classList.toggle("hidden", !state.service);
  $("password-label").textContent = state.service ? "Password temporanea" : "Password";
  $("device-id").textContent = formatId(state.device_id);
  $("password").textContent = state.password;
  if (document.activeElement !== $("server")) $("server").value = state.server;
  if (state.status === "setup") $("settings").open = true;

  const session = state.session;
  $("session").classList.toggle("hidden", !session);
  if (session) {
    const who = session.viewer_name || formatId(session.viewer_id);
    $("session-text").textContent = `${who} sta controllando questo computer (${session.direct ? "connessione diretta" : "attraverso il server"})`;
  }

  const msg = state.viewer_message;
  $("connect-message").classList.toggle("hidden", !msg);
  $("connect-message").textContent = msg ?? "";

  renderChat(state);
  renderIncoming(state.incoming);
}

function renderChat(state) {
  const visible = Boolean(state.session) || state.chat.length > 0;
  $("chat").classList.toggle("hidden", !visible);
  $("chat-form").classList.toggle("hidden", !state.session);
  const log = $("chat-log");
  if (log.childElementCount === state.chat.length) return;
  log.replaceChildren(
    ...state.chat.map((line) => {
      const p = document.createElement("p");
      p.className = line.mine ? "mine" : "theirs";
      p.textContent = line.text;
      return p;
    }),
  );
  log.scrollTop = log.scrollHeight;
  // Messaggio in arrivo: si può rispondere subito, senza cliccare nel campo.
  const last = state.chat[state.chat.length - 1];
  if (state.session && last && !last.mine && document.activeElement?.tagName !== "INPUT") {
    $("chat-text").focus();
  }
}

function renderIncoming(incoming) {
  $("incoming").classList.toggle("hidden", !incoming);
  clearInterval(countdownTimer);
  if (!incoming) return;
  $("incoming-name").textContent = incoming.viewer_name || "Un dispositivo";
  $("incoming-id").textContent = `(ID ${formatId(incoming.viewer_id)})`;
  const tick = () => {
    const left = Math.max(0, Math.round((incoming.deadline_ms - Date.now()) / 1000));
    $("incoming-countdown").textContent = `Rifiutata automaticamente tra ${left} s`;
  };
  tick();
  countdownTimer = setInterval(tick, 500);
  $("accept").focus();
}

async function respond(accept) {
  const incoming = current?.incoming;
  if (!incoming) return;
  $("incoming").classList.add("hidden");
  await invoke("respond", { sessionId: incoming.session_id, accept });
}

async function copy(text, button) {
  try {
    await navigator.clipboard.writeText(text);
    const old = button.textContent;
    button.textContent = "Copiato";
    setTimeout(() => (button.textContent = old), 1200);
  } catch {
    // Appunti non disponibili: niente da fare.
  }
}

$("accept").addEventListener("click", () => respond(true));
$("reject").addEventListener("click", () => respond(false));
$("end-session").addEventListener("click", () => invoke("end_session"));
$("new-password").addEventListener("click", () => invoke("new_password"));
$("copy-id").addEventListener("click", (e) => current?.device_id && copy(current.device_id, e.target));
$("copy-password").addEventListener("click", (e) => current && copy(current.password, e.target));
$("source").addEventListener("click", (e) => {
  e.preventDefault();
  invoke("open_source");
});

$("server-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  try {
    await invoke("set_server", { server: $("server").value });
  } catch (err) {
    $("status-text").textContent = String(err);
  }
});

$("connect-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const message = $("connect-message");
  message.classList.add("hidden");
  try {
    await invoke("connect", { deviceId: $("remote-id").value, password: $("remote-password").value });
    $("remote-password").value = "";
  } catch (err) {
    message.textContent = String(err);
    message.classList.remove("hidden");
  }
});

$("chat-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const text = $("chat-text").value;
  if (!text.trim()) return;
  $("chat-text").value = "";
  await invoke("send_chat", { text });
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && current?.incoming) respond(false);
});

listen("state", (event) => render(event.payload));
invoke("get_state").then(render);
