// NuviraHub user interface: receives the state from the Rust process ("state"
// event) and sends commands with invoke. Sections are picked from the rail on the
// left; address book and administration live in address-book.js. Texts come from the
// nuvira-i18n strings: t() for keys, msg() for "@key" messages from other programs.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

let strings = {};
let current = null;
let section = "connect";
let countdownTimer = null;
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

// Customised program (NuviraHub Pro installer): name, logo, colour, support,
// locked settings, quick support (only this computer's ID and password).
function applyBranding(b) {
  if (!b) return;
  if (b.name) document.title = b.name;
  if (b.logo) document.querySelector(".rail-logo").src = b.logo;
  if (b.color) {
    document.documentElement.style.setProperty("--accent", b.color);
    document.documentElement.style.setProperty("--accent-soft", `${b.color}1f`);
  }
  const contact = b.support_text || b.support_phone || b.support_email || b.support_url;
  $("support-box").classList.toggle("hidden", !contact);
  $("support-text").textContent = b.support_text;
  $("support-phone").textContent = b.support_phone;
  $("support-email").textContent = b.support_email;
  if (b.support_url) {
    $("support-url").textContent = b.support_url.replace(/^https:\/\//, "");
    $("support-url").classList.remove("hidden");
    $("support-url").addEventListener("click", () => invoke("open_support").catch(() => {}));
  }
  if (b.kind === "quick") {
    document.body.classList.add("quick");
    for (const name of ["address-book", "admin", "settings"]) {
      document.querySelector(`.nav[data-section="${name}"]`)?.classList.add("hidden", "locked");
    }
  }
  if (b.lock_server) {
    $("server").disabled = true;
    $("server-form").querySelector("button").disabled = true;
  }
  if (b.lock_language) $("language").disabled = true;
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

$("privacy-allowed").addEventListener("change", async (e) => {
  try {
    await invoke("set_privacy_allowed", { allowed: e.target.checked });
  } catch (err) {
    console.error(err);
    e.target.checked = !e.target.checked;
  }
});

$("terminal-allowed").addEventListener("change", async (e) => {
  try {
    await invoke("set_terminal_allowed", { allowed: e.target.checked });
  } catch (err) {
    console.error(err);
    e.target.checked = !e.target.checked;
  }
});

$("tunnel-allowed").addEventListener("change", async (e) => {
  try {
    await invoke("set_tunnel_allowed", { allowed: e.target.checked });
  } catch (err) {
    console.error(err);
    e.target.checked = !e.target.checked;
  }
});

$("print-allowed").addEventListener("change", async (e) => {
  try {
    await invoke("set_print_allowed", { allowed: e.target.checked });
  } catch (err) {
    console.error(err);
    e.target.checked = !e.target.checked;
  }
});

$("recording-allowed").addEventListener("change", async (e) => {
  try {
    await invoke("set_recording_allowed", { allowed: e.target.checked });
  } catch (err) {
    console.error(err);
    e.target.checked = !e.target.checked;
  }
});

$("record-sessions").addEventListener("change", async (e) => {
  try {
    await invoke("set_record_sessions", { on: e.target.checked });
  } catch (err) {
    console.error(err);
    e.target.checked = !e.target.checked;
  }
});

$("record-change").addEventListener("click", () => invoke("choose_record_dir").catch(console.error));
$("record-open").addEventListener("click", () => invoke("open_record_dir").catch(console.error));

$("print-ask").addEventListener("click", async () => {
  try {
    await invoke("print_ask_again");
  } catch (err) {
    console.error(err);
  }
});

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
  // The session window may have changed the printing choice.
  if (name === "settings") invoke("get_state").then(render).catch(console.error);
  if (name === "chat") {
    chatSeen = current?.chat.length ?? 0;
    $("chat-unread").classList.add("hidden");
    if (current?.session) $("chat-text").focus();
  }
}

for (const nav of document.querySelectorAll(".nav")) {
  nav.addEventListener("click", () => showSection(nav.dataset.section));
}

// Why the last attempt to start the stopped service failed: kept while the service
// stays stopped, or the next state would hide it.
let repairError = null;

function statusText(state) {
  // With the service, the reason is shown until its agent answers.
  if (state.service && state.status !== "online" && state.error) return msg(state.error);
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
  // Which server: the public one (address not shown) or the one set.
  $("server-line").textContent = state.server.trim()
    ? t("status.server_own", { server: state.server.trim() })
    : t("status.server_public");
  // The public server's terms of use, while it is the one in use.
  for (const node of document.querySelectorAll(".public-only")) node.classList.toggle("hidden", Boolean(state.server.trim()));
  $("service-note").classList.toggle("hidden", !state.service);
  // With the server fixed there is nothing to save, so no note about the service.
  $("server-service").classList.toggle("hidden", !state.service || state.server_managed);
  // Linux and macOS: a user service, installed again without an administrator.
  const serverKey = state.platform === "windows" ? "settings.server_service" : "settings.server_service_unix";
  $("server-service").dataset.i18n = serverKey;
  $("server-service").textContent = t(serverKey);
  $("install-off").classList.toggle("hidden", state.service);
  // Linux for now: only controls other computers.
  $("this-pc").classList.toggle("hidden", !state.controllable);
  $("viewer-only").classList.toggle("hidden", state.controllable);
  $("unattended").classList.toggle("hidden", !state.unattended);
  $("this-pc-settings").classList.toggle("hidden", !state.controllable);
  // Privacy mode exists only on Windows for now.
  $("privacy-settings").classList.toggle("hidden", state.platform !== "windows");
  $("privacy-allowed").checked = state.privacy_allowed;
  $("terminal-allowed").checked = state.terminal_allowed;
  $("tunnel-allowed").checked = state.tunnel_allowed;
  $("print-allowed").checked = state.print_allowed;
  $("recording-allowed").checked = state.recording_allowed || state.recording_required;
  $("recording-allowed").disabled = state.recording_required;
  $("recording-required").classList.toggle("hidden", !state.recording_required);
  $("record-sessions").checked = state.record_sessions;
  $("record-dir").textContent = state.record_dir;
  $("print-auto").classList.toggle("hidden", !state.print_auto);
  $("print-auto-text").textContent = state.print_auto ? t("settings.print_auto", { printer: state.print_auto }) : "";
  $("update-bar").classList.toggle("hidden", !state.update && !state.installed && !state.service_stopped && !state.update_blocked);
  if (!$("update-button").disabled) {
    if (!state.service_stopped) repairError = null;
    if (state.service_stopped) {
      $("update-text").textContent = repairError
        ?? (state.service_old ? t("service.stopped_old", { version: state.service_old }) : t("service.stopped"));
      $("update-button").textContent = t(state.service_old ? "service.update_start" : "service.start");
    } else if (state.installed) {
      $("update-text").textContent = t("update.installed", { version: state.installed });
      $("update-button").textContent = t("update.restart");
    } else if (state.update) {
      $("update-text").textContent = t("update.available", { version: state.update });
      $("update-button").textContent = t("update.button");
    } else if (state.update_blocked) {
      $("update-text").textContent = t("update.blocked", { version: state.update_blocked });
    }
    // Only a notice: nothing the app could do about it.
    $("update-button").classList.toggle(
      "hidden",
      Boolean(state.update_blocked) && !state.service_stopped && !state.installed && !state.update,
    );
  }
  $("install-on").classList.toggle("hidden", !state.service);
  // Linux and macOS: a user service (systemd, launchd), removed from here;
  // Windows: from the system settings.
  const linux = state.platform === "linux";
  const mac = state.platform === "macos";
  $("install-on").textContent = t(
    state.service_managed ? "settings.installed_managed" : linux || mac ? "settings.installed_linux" : "settings.installed",
  );
  $("unattended-hint").textContent = t(
    linux ? "settings.unattended_hint_linux" : mac ? "settings.unattended_hint_mac" : "settings.unattended_hint",
  );
  $("uninstall-row").classList.toggle("hidden", !((linux || mac) && state.service && !state.service_managed));
  // Installed by the organisation: nothing to install from here either.
  if (state.service_managed) $("install-off").classList.add("hidden");
  $("password-label").textContent = t(state.service ? "connect.temp_password" : "connect.password");
  $("device-id").textContent = formatId(state.device_id);
  $("password").textContent = state.password || "—";
  if (document.activeElement !== $("server")) $("server").value = state.server;
  // Set by the organisation's policies (Group Policy, Intune).
  $("server-managed").classList.toggle("hidden", !state.server_managed);
  const managedKey = state.platform === "windows" ? "settings.server_managed" : "settings.server_managed_unix";
  $("server-managed").dataset.i18n = managedKey;
  $("server-managed").textContent = t(managedKey);
  if (state.server_managed) {
    $("server").disabled = true;
    $("server-form").querySelector("button").disabled = true;
  }

  const session = state.session;
  $("session-strip").classList.toggle("hidden", !session);
  if (session) {
    const who = session.viewer_name || formatId(session.viewer_id);
    const path = t(session.direct ? "session.direct" : "session.relay");
    $("session-text").textContent = t(session.terminal ? "session.strip_terminal" : "session.strip", { who, path });
  }
  $("session-rec").classList.toggle("hidden", !session?.recording);
  renderSwitches(session);

  const message = state.viewer_message;
  $("connect-message").classList.toggle("hidden", !message);
  // Not an error: the other side is being asked.
  $("connect-message").classList.toggle("ok", message === "@app.waiting_confirmation");
  $("connect-message").textContent = msg(message) ?? "";

  renderChat(state);
  renderIncoming(state.incoming);
  // Defined in address-book.js, loaded after this file.
  if (typeof renderAddressBook === "function") renderAddressBook(state);
  if (typeof renderSupport === "function") renderSupport(state);
}

// What the session may do, one switch each: off for this session only.
function renderSwitches(session) {
  const offered = session?.offered ?? [];
  const box = $("session-switches");
  box.classList.toggle("hidden", offered.length === 0);
  const key = JSON.stringify([offered, session?.off ?? []]);
  if (box.dataset.key === key) return;
  box.dataset.key = key;
  const buttons = offered.map((name) => {
    const on = !session.off.includes(name);
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = t(`perm.${name}`);
    button.setAttribute("aria-pressed", String(on));
    button.title = t(on ? "session.switch_off" : "session.switch_on");
    button.addEventListener("click", () => invoke("switch_permission", { name, on: !on }));
    return button;
  });
  box.replaceChildren(box.firstElementChild, ...buttons);
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
  $("incoming-support").classList.toggle("hidden", !incoming.support);
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
for (const link of document.querySelectorAll("a.terms")) {
  link.addEventListener("click", (e) => {
    e.preventDefault();
    invoke("open_terms");
  });
}
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
    // A note when the server was saved but the sign-in screen stayed on the old one.
    const note = await invoke("set_server", { server: $("server").value });
    if (note) {
      $("server-message").textContent = note;
      $("server-message").classList.remove("hidden");
    }
  } catch (err) {
    $("server-message").textContent = msg(String(err));
    $("server-message").classList.remove("hidden");
  }
});

$("update-button").addEventListener("click", async () => {
  $("update-button").disabled = true;
  if (current.service_stopped) {
    $("update-text").textContent = t("service.starting");
    try {
      await invoke("repair_service");
      repairError = null;
    } catch (err) {
      repairError = msg(String(err));
      $("update-text").textContent = repairError;
    }
    $("update-button").disabled = false;
    // The link with the service notices it running within 2 s and hides the bar.
    return;
  }
  try {
    if (current.installed) {
      await invoke("restart_app");
      return;
    }
    $("update-text").textContent = t("update.updating");
    await invoke("apply_update");
  } catch (err) {
    $("update-text").textContent = msg(String(err));
    $("update-button").disabled = false;
  }
});

$("uninstall-button").addEventListener("click", async () => {
  const message = $("install-message");
  $("uninstall-button").disabled = true;
  try {
    message.textContent = msg(await invoke("uninstall_service"));
    message.classList.add("ok");
  } catch (err) {
    message.textContent = msg(String(err));
    message.classList.remove("ok");
  } finally {
    message.classList.remove("hidden");
    $("uninstall-button").disabled = false;
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
  // macOS: its prompt for the printer warns that Apple cannot verify the script and
  // shows no text of ours, so the app says what it is for.
  show(t(current?.platform === "macos" ? "settings.installing_mac" : "settings.installing"), true);
  try {
    show(await invoke("install_service", { password }), true);
    $("install-form").reset();
  } catch (err) {
    show(String(err), false);
  } finally {
    $("install-button").disabled = false;
  }
});

// Connect (desktop) or Terminal only.
async function connectNow(terminal) {
  const message = $("connect-message");
  message.classList.add("hidden");
  const password = $("remote-password").value;
  // Entry picked from the suggestions (address-book.js): without a typed password, the saved one is used.
  const entry = typeof chosenEntry === "function" ? chosenEntry() : null;
  try {
    if (entry && !password) {
      await invoke("connect_entry", { id: entry.id, terminal });
    } else {
      await invoke("connect", { deviceId: $("remote-id").value, password, terminal });
    }
    $("remote-password").value = "";
  } catch (err) {
    message.textContent = msg(String(err));
    message.classList.remove("hidden");
  }
}

$("connect-form").addEventListener("submit", (e) => {
  e.preventDefault();
  connectNow(false);
});
$("terminal-button").addEventListener("click", () => connectNow(true));

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
  invoke("key_protection")
    .then((p) => {
      if (!p) return;
      $("key-protection").textContent = t(`about.key.${p.replace("-", "_")}`);
      $("key-protection").classList.remove("hidden");
    })
    .catch(() => {});
  applyBranding(await invoke("get_branding"));
  listen("state", (event) => render(event.payload));
  render(await invoke("get_state"));
})();
