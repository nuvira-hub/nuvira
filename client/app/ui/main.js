// Interfaccia di NexoDesk: riceve lo stato dal processo Rust (evento "state")
// e invia i comandi con invoke. Le sezioni si scelgono dalla barra a sinistra;
// rubrica e amministrazione sono in rubrica.js.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

const STATUS_TEXT = {
  setup: "Imposta il server NexoDesk nelle Impostazioni",
  connecting: "Collegamento al server…",
  online: "Pronto: raggiungibile con questo ID",
  offline: "Non collegato al server",
};

let current = null;
let section = "connect";
let countdownTimer = null;
let firstRender = true;
let chatSeen = 0;

function formatId(id) {
  return id ? `${id.slice(0, 3)} ${id.slice(3, 6)} ${id.slice(6)}` : "— — —";
}

function showSection(name) {
  section = name;
  for (const page of document.querySelectorAll(".page")) page.classList.toggle("hidden", page.id !== `sec-${name}`);
  for (const nav of document.querySelectorAll(".nav")) nav.classList.toggle("active", nav.dataset.section === name);
  if (name === "chat") {
    chatSeen = current?.chat.length ?? 0;
    $("chat-unread").classList.add("hidden");
    if (current?.session) $("chat-text").focus();
  }
}

for (const nav of document.querySelectorAll(".nav")) {
  nav.addEventListener("click", () => showSection(nav.dataset.section));
}

function render(state) {
  current = state;
  let text = STATUS_TEXT[state.status] ?? state.status;
  if (state.status === "online" && state.websocket) text += " (via WebSocket)";
  if (state.status === "offline" && state.error) text += `: ${state.error}`;
  // Con il servizio, finché il suo agent non risponde si mostra il motivo.
  if (state.service && state.status === "connecting" && state.error) text = state.error;
  $("status-dot").className = `dot ${state.status}`;
  $("rail-status").title = text;
  $("status-text").textContent = text;
  $("service-note").classList.toggle("hidden", !state.service);
  $("password-label").textContent = state.service ? "Password temporanea" : "Password";
  $("device-id").textContent = formatId(state.device_id);
  $("password").textContent = state.password || "—";
  if (document.activeElement !== $("server")) $("server").value = state.server;
  if (firstRender && state.status === "setup") showSection("settings");
  firstRender = false;

  const session = state.session;
  $("session-strip").classList.toggle("hidden", !session);
  if (session) {
    const who = session.viewer_name || formatId(session.viewer_id);
    $("session-text").textContent = `${who} sta controllando questo computer · ${session.direct ? "connessione diretta" : "attraverso il server"}`;
  }

  const msg = state.viewer_message;
  $("connect-message").classList.toggle("hidden", !msg);
  $("connect-message").textContent = msg ?? "";

  renderChat(state);
  renderIncoming(state.incoming);
  // Definita in rubrica.js, caricato dopo questo file.
  if (typeof renderRubrica === "function") renderRubrica(state);
}

function renderChat(state) {
  const available = Boolean(state.session) || state.chat.length > 0;
  $("nav-chat").classList.toggle("hidden", !available);
  if (!available && section === "chat") showSection("connect");
  $("chat-form").classList.toggle("hidden", !state.session);
  // Sessione nuova: la chat ricomincia da capo.
  if (state.chat.length < chatSeen) chatSeen = 0;
  const log = $("chat-log");
  if (log.childElementCount === state.chat.length) return;
  log.replaceChildren(
    ...state.chat.map((line) => {
      const p = document.createElement("p");
      p.className = line.system ? "system" : line.mine ? "mine" : "theirs";
      p.textContent = line.text;
      return p;
    }),
  );
  log.scrollTop = log.scrollHeight;
  // Messaggio in arrivo durante la sessione: si apre la chat, pronta per rispondere.
  const last = state.chat[state.chat.length - 1];
  if (last && !last.mine && state.chat.length > chatSeen) {
    if (state.session && !last.system && document.activeElement?.tagName !== "INPUT") {
      showSection("chat");
    } else if (section !== "chat") {
      $("chat-unread").classList.remove("hidden");
    }
  }
  if (section === "chat") chatSeen = state.chat.length;
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
    button.classList.add("done");
    button.title = "Copiato";
    setTimeout(() => button.classList.remove("done"), 1200);
  } catch {
    // Appunti non disponibili: niente da fare.
  }
}

$("accept").addEventListener("click", () => respond(true));
$("reject").addEventListener("click", () => respond(false));
$("end-session").addEventListener("click", () => invoke("end_session"));
$("strip-chat").addEventListener("click", () => showSection("chat"));
$("new-password").addEventListener("click", () => invoke("new_password"));
$("copy-id").addEventListener("click", (e) => current?.device_id && copy(current.device_id, e.currentTarget));
$("copy-password").addEventListener("click", (e) => current?.password && copy(current.password, e.currentTarget));
for (const id of ["source", "source-button"]) {
  $(id).addEventListener("click", (e) => {
    e.preventDefault();
    invoke("open_source");
  });
}
window.__TAURI__.app
  ?.getVersion()
  .then((v) => ($("app-version").textContent = `Versione ${v}`))
  .catch(() => {});

$("server-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  $("server-message").classList.add("hidden");
  try {
    await invoke("set_server", { server: $("server").value });
  } catch (err) {
    $("server-message").textContent = String(err);
    $("server-message").classList.remove("hidden");
  }
});

$("connect-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const message = $("connect-message");
  message.classList.add("hidden");
  const password = $("remote-password").value;
  // Scheda scelta dai suggerimenti (rubrica.js): senza password scritta si usa quella salvata.
  const entry = typeof chosenEntry === "function" ? chosenEntry() : null;
  try {
    if (entry && !password) {
      await invoke("connect_entry", { id: entry.id });
    } else {
      await invoke("connect", { deviceId: $("remote-id").value, password });
    }
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
