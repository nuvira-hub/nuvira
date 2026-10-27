// NexoDesk user interface: receives the state from the Rust process ("state"
// event) and sends commands with invoke. Sections are picked from the rail on the
// left; address book and administration live in address-book.js. Texts come from the
// nexodesk-i18n strings: t() for keys, msg() for "@key" messages from other programs.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

let strings = {};
let current = null;
let section = "connect";
let countdownTimer = null;
let firstRender = true;
let chatSeen = 0;

function t(key, vars = {}) {
  let text = strings[key] ?? key;
  for (const [name, value] of Object.entries(vars)) text = text.replaceAll(`{${name}}`, value);
  return text;
}

// A message from another program: "@key" is translated, anything else is shown as is.
function msg(text) {
  return typeof text === "string" && text.startsWith("@") && strings[text.slice(1)] ? t(text.slice(1)) : text;
}

function applyStrings() {
  for (const node of document.querySelectorAll("[data-i18n]")) node.textContent = t(node.dataset.i18n);
  for (const node of document.querySelectorAll("[data-i18n-placeholder]")) node.placeholder = t(node.dataset.i18nPlaceholder);
  for (const node of document.querySelectorAll("[data-i18n-title]")) {
    node.title = t(node.dataset.i18nTitle);
    node.setAttribute("aria-label", node.title);
  }
  for (const node of document.querySelectorAll("[data-i18n-label]")) node.setAttribute("aria-label", t(node.dataset.i18nLabel));
}

function useStrings(info) {
  strings = info.strings;
  document.documentElement.lang = info.lang;
  const select = $("language");
  const auto = new Option(t("settings.language_auto"), "auto");
  select.replaceChildren(auto, ...info.languages.map(([code, name]) => new Option(name, code)));
  select.value = info.preference;
  applyStrings();
  if (current) render(current);
}

$("language").addEventListener("change", async (e) => {
  useStrings(await invoke("set_language", { language: e.target.value }));
});

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

function statusText(state) {
  // With the service, the reason is shown until its agent answers.
  if (state.service && state.status === "connecting" && state.error) return msg(state.error);
  switch (state.status) {
    case "online":
      return t(state.websocket ? "status.online_ws" : "status.online");
    case "offline":
      return state.error ? t("status.offline_reason", { reason: msg(state.error) }) : t("status.offline");
    default:
      return t(`status.${state.status}`);
  }
}

function render(state) {
  current = state;
  const text = statusText(state);
  $("status-dot").className = `dot ${state.status}`;
  $("rail-status").title = text;
  $("status-text").textContent = text;
  $("service-note").classList.toggle("hidden", !state.service);
  $("install-off").classList.toggle("hidden", state.service);
  $("update-bar").classList.toggle("hidden", !state.update);
  if (state.update && !$("update-button").disabled) $("update-text").textContent = t("update.available", { version: state.update });
  $("install-on").classList.toggle("hidden", !state.service);
  $("password-label").textContent = t(state.service ? "connect.temp_password" : "connect.password");
  $("device-id").textContent = formatId(state.device_id);
  $("password").textContent = state.password || "—";
  if (document.activeElement !== $("server")) $("server").value = state.server;
  if (firstRender && state.status === "setup") showSection("settings");
  firstRender = false;

  const session = state.session;
  $("session-strip").classList.toggle("hidden", !session);
  if (session) {
    const who = session.viewer_name || formatId(session.viewer_id);
    const path = t(session.direct ? "session.direct" : "session.relay");
    $("session-text").textContent = t("session.strip", { who, path });
  }

  const message = state.viewer_message;
  $("connect-message").classList.toggle("hidden", !message);
  // Not an error: the other side is being asked.
  $("connect-message").classList.toggle("ok", message === "@app.waiting_confirmation");
  $("connect-message").textContent = msg(message) ?? "";

  renderChat(state);
  renderIncoming(state.incoming);
  // Defined in address-book.js, loaded after this file.
  if (typeof renderAddressBook === "function") renderAddressBook(state);
}

function renderChat(state) {
  const available = Boolean(state.session) || state.chat.length > 0;
  $("nav-chat").classList.toggle("hidden", !available);
  if (!available && section === "chat") showSection("connect");
  $("chat-form").classList.toggle("hidden", !state.session);
  // New session: the chat starts over.
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
  // Incoming message during a session: the chat opens, ready for an answer.
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
  $("incoming-name").textContent = incoming.viewer_name || t("incoming.unknown");
  $("incoming-id").textContent = t("incoming.id", { id: formatId(incoming.viewer_id) });
  const tick = () => {
    const left = Math.max(0, Math.round((incoming.deadline_ms - Date.now()) / 1000));
    $("incoming-countdown").textContent = t("incoming.countdown", { s: left });
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
    const title = button.title;
    button.title = t("connect.copied");
    setTimeout(() => {
      button.classList.remove("done");
      button.title = title;
    }, 1200);
  } catch {
    // Clipboard not available: nothing to do.
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

$("server-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  $("server-message").classList.add("hidden");
  try {
    await invoke("set_server", { server: $("server").value });
  } catch (err) {
    $("server-message").textContent = msg(String(err));
    $("server-message").classList.remove("hidden");
  }
});

$("update-button").addEventListener("click", async () => {
  $("update-button").disabled = true;
  $("update-text").textContent = t("update.updating");
  try {
    await invoke("apply_update");
  } catch (err) {
    $("update-text").textContent = msg(String(err));
    $("update-button").disabled = false;
  }
});

$("install-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const message = $("install-message");
  const show = (text, ok) => {
    message.textContent = msg(text);
    message.classList.toggle("ok", ok);
    message.classList.remove("hidden");
  };
  const password = $("install-pw").value;
  if (password !== $("install-pw2").value) return show(t("settings.pw_mismatch"), false);
  if (password && password.length < 8) return show(t("settings.install_pw_short"), false);
  $("install-button").disabled = true;
  show(t("settings.installing"), true);
  try {
    show(await invoke("install_service", { password }), true);
    $("install-form").reset();
  } catch (err) {
    show(String(err), false);
  } finally {
    $("install-button").disabled = false;
  }
});

$("connect-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const message = $("connect-message");
  message.classList.add("hidden");
  const password = $("remote-password").value;
  // Entry picked from the suggestions (address-book.js): without a typed password, the saved one is used.
  const entry = typeof chosenEntry === "function" ? chosenEntry() : null;
  try {
    if (entry && !password) {
      await invoke("connect_entry", { id: entry.id });
    } else {
      await invoke("connect", { deviceId: $("remote-id").value, password });
    }
    $("remote-password").value = "";
  } catch (err) {
    message.textContent = msg(String(err));
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

(async () => {
  useStrings(await invoke("i18n"));
  window.__TAURI__.app
    ?.getVersion()
    .then((v) => {
      $("app-version").textContent = t("about.version", { v });
    })
    .catch(() => {});
  listen("state", (event) => render(event.payload));
  render(await invoke("get_state"));
})();
