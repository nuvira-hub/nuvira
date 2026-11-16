// Support queue (NuviraHub Pro, feature "pro.support_queue"): on this computer
// the "Ask for support" card, for the technicians the "Queue" section, refreshed
// every few seconds while it is open. Uses $, invoke, t, msg, formatId and
// showSection from main.js; render() calls renderSupport() on every new state.

let supportState = null;
let queueTimer = null;

function supportAvailable(state) {
  return (state?.features ?? []).includes("pro.support_queue");
}

function supportText(s) {
  if (s.taken_by) return t("support.taken", { name: s.taken_by });
  if (s.invited_by) return t("support.invited", { name: s.invited_by });
  return s.position ? t("support.position", { n: s.position }) : t("support.sent");
}

function renderSupport(state) {
  supportState = state;
  renderNote(state);
  renderInvite(state);
  const available = supportAvailable(state);
  // An invitation's card shows the request itself.
  $("sq-card").classList.toggle("hidden", !(available && state.controllable) || Boolean(state.invite));
  const open = Boolean(state.support);
  $("sq-form").classList.toggle("hidden", open);
  $("sq-open").classList.toggle("hidden", !open);
  if (open) $("sq-status").textContent = supportText(state.support);
  // A refused request (e.g. an expired invitation), when the invitation card does not show it.
  if (state.support_error && !state.invite) {
    $("sq-error").textContent = msg(state.support_error);
    $("sq-error").classList.remove("hidden");
  }
  const technician = available && Boolean(state.account);
  $("nav-queue").classList.toggle("hidden", !technician);
  $("invites").classList.toggle("hidden", !(technician && (state.features ?? []).includes("pro.invite")));
  if (!technician && section === "queue") showSection("connect");
  watchQueue(technician);
}

// An invitation received: on this server it goes by itself; on another one the
// person agrees to move first.
let shownInvite = null;

function renderInvite(state) {
  const inv = state.invite;
  $("invite-card").classList.toggle("hidden", !inv);
  if (!inv) {
    shownInvite = null;
    return;
  }
  // A new invitation brings the connection section, where its card is.
  if (shownInvite !== inv.code) {
    shownInvite = inv.code;
    if (section !== "connect") showSection("connect");
  }
  let text;
  if (inv.other_server && inv.server_locked) text = t("invite.locked", { server: inv.server });
  else if (inv.other_server) text = t("invite.other_server", { server: inv.server, current: state.server || t("invite.public_server") });
  else if (state.support) text = supportText(state.support);
  else if (state.support_error) text = "";
  else text = t("invite.waiting");
  $("invite-text").textContent = text;
  $("invite-text").classList.toggle("hidden", !text);
  $("invite-accept").classList.toggle("hidden", !(inv.other_server && !inv.server_locked));
  // The request it made, while waiting: withdrawn here (the support card is hidden).
  // An invitation for another server made no request: a request still waiting is an
  // earlier one, not this card's to cancel; "Close" drops the invitation.
  const waiting = Boolean(!inv.other_server && state.support && !state.support.taken_by);
  $("invite-cancel").classList.toggle("hidden", !waiting);
  $("invite-dismiss").classList.toggle("hidden", waiting);
  $("invite-error").classList.toggle("hidden", !state.support_error);
  $("invite-error").textContent = state.support_error ? msg(state.support_error) : "";
}

$("invite-accept").addEventListener("click", async () => {
  $("invite-accept").disabled = true;
  try {
    await invoke("accept_invite");
  } catch (err) {
    $("invite-error").textContent = msg(String(err));
    $("invite-error").classList.remove("hidden");
  }
  $("invite-accept").disabled = false;
});
$("invite-dismiss").addEventListener("click", () => invoke("dismiss_invite"));
$("invite-cancel").addEventListener("click", async () => {
  await invoke("cancel_support");
  await invoke("dismiss_invite");
});

// The card stays collapsed to its title unless opened (remembered on this computer).
function setSupportCollapsed(collapsed) {
  $("sq-card").classList.toggle("collapsed", collapsed);
  $("sq-toggle").setAttribute("aria-expanded", String(!collapsed));
}
try {
  setSupportCollapsed(localStorage.getItem("sq-expanded") !== "1");
} catch {
  setSupportCollapsed(true);
}
$("sq-toggle").addEventListener("click", () => {
  const collapsed = !$("sq-card").classList.contains("collapsed");
  setSupportCollapsed(collapsed);
  try {
    localStorage.setItem("sq-expanded", collapsed ? "0" : "1");
  } catch {}
  if (!collapsed) $("sq-message").focus();
});

$("sq-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  try {
    const invite = $("sq-invite").value.trim();
    await invoke("request_support", { message: $("sq-message").value.trim(), invite: invite || null });
    $("sq-message").value = "";
    $("sq-invite").value = "";
    $("sq-error").classList.add("hidden");
  } catch (err) {
    $("sq-error").textContent = msg(String(err));
    $("sq-error").classList.remove("hidden");
  }
});

$("sq-cancel").addEventListener("click", () => invoke("cancel_support"));

function waited(since) {
  const minutes = Math.max(0, Math.round((Date.now() / 1000 - since) / 60));
  return minutes < 1 ? t("queue.just_now") : t("queue.minutes", { n: minutes });
}

async function refreshQueue() {
  if (section !== "queue") return;
  let requests;
  try {
    requests = await invoke("support_queue");
    $("queue-error").classList.add("hidden");
  } catch (err) {
    $("queue-error").textContent = msg(String(err));
    $("queue-error").classList.remove("hidden");
    return;
  }
  checkForMe(requests);
  // The invitations' uses change with the requests.
  const count = requests.length;
  if (count !== lastQueueCount) refreshInvites();
  lastQueueCount = count;
  const list = $("queue-list");
  list.replaceChildren();
  $("queue-empty").classList.toggle("hidden", requests.length > 0);
  for (const r of requests) {
    const row = document.createElement("div");
    row.className = "entry queue-item";
    const text = document.createElement("div");
    text.className = "queue-text";
    const name = document.createElement("strong");
    name.textContent = r.name || formatId(r.device_id);
    const meta = document.createElement("span");
    meta.className = "hint";
    meta.textContent = `${formatId(r.device_id)} · ${r.taken_by ? t("queue.taken_by", { name: r.taken_by }) : waited(r.since)}`;
    text.append(name, meta);
    // An invitation's request: for its technician alone.
    if (r.for_account) {
      const forWho = document.createElement("span");
      const mine = r.for_account === supportState?.account?.username;
      forWho.className = mine ? "for-me" : "hint";
      forWho.textContent = mine ? t("queue.for_you") : t("queue.for", { name: r.for_account });
      text.append(forWho);
    }
    if (r.message) {
      const message = document.createElement("span");
      message.className = "queue-message";
      message.textContent = r.message;
      text.append(message);
    }
    row.append(text);
    if (!r.taken_by && (!r.for_account || r.for_account === supportState?.account?.username)) {
      const take = document.createElement("button");
      take.className = "primary";
      take.textContent = t("queue.take");
      take.addEventListener("click", async () => {
        take.disabled = true;
        try {
          await invoke("support_take", { id: r.id });
        } catch (err) {
          $("queue-error").textContent = msg(String(err));
          $("queue-error").classList.remove("hidden");
        }
        refreshQueue();
      });
      row.append(take);
    }
    list.append(row);
  }
}

// Requests made for this technician (invitations): the queue's icon gets a dot and
// the window asks for attention, also while another section is open.
let watchTimer = null;
let lastQueueCount = -1;
const seenForMe = new Set();

function watchQueue(technician) {
  if (!technician) {
    clearInterval(watchTimer);
    watchTimer = null;
    return;
  }
  if (watchTimer) return;
  checkForMe();
  watchTimer = setInterval(checkForMe, 15000);
}

async function checkForMe(requests) {
  const me = supportState?.account?.username;
  if (!me) return;
  if (!requests) {
    try {
      requests = await invoke("support_queue");
    } catch {
      return;
    }
  }
  let fresh = false;
  for (const r of requests) {
    if (r.for_account === me && !r.taken_by && !seenForMe.has(r.id)) {
      seenForMe.add(r.id);
      fresh = true;
    }
  }
  const waiting = requests.some((r) => r.for_account === me && !r.taken_by);
  $("queue-new").classList.toggle("hidden", !waiting || section === "queue");
  if (fresh) invoke("request_attention").catch(() => {});
}

$("nav-queue").addEventListener("click", () => {
  refreshQueue();
  refreshInvites();
  clearInterval(queueTimer);
  queueTimer = setInterval(() => (section === "queue" ? refreshQueue() : clearInterval(queueTimer)), 3000);
});

// Note at the end of a session (session log, NuviraHub Pro).
let noteFor = null;

function renderNote(state) {
  const request = state.note_request;
  $("note").classList.toggle("hidden", !request);
  if (!request || noteFor === request.device_id + request.name) return;
  noteFor = request.device_id + request.name;
  $("note-hint").textContent = t("note.hint", { name: request.name || formatId(request.device_id) });
  $("note-text").value = "";
  $("note-error").classList.add("hidden");
  $("note-text").focus();
}

async function sendNote(note) {
  const request = supportState?.note_request;
  if (!request) return;
  try {
    await invoke("session_note", { deviceId: request.device_id, note });
    noteFor = null;
  } catch (err) {
    $("note-error").textContent = msg(String(err));
    $("note-error").classList.remove("hidden");
  }
}

$("note-form").addEventListener("submit", (e) => {
  e.preventDefault();
  sendNote($("note-text").value.trim());
});
$("note-skip").addEventListener("click", () => sendNote(""));

// Invitation links of this technician (NuviraHub Pro, feature "pro.invite").
function untilText(seconds) {
  return new Date(seconds * 1000).toLocaleString(undefined, { dateStyle: "short", timeStyle: "short" });
}

async function copyText(text, button) {
  try {
    await navigator.clipboard.writeText(text);
    const label = button.textContent;
    button.textContent = t("invite.copied");
    setTimeout(() => (button.textContent = label), 1500);
  } catch {}
}

async function refreshInvites() {
  if ($("invites").classList.contains("hidden")) return;
  let list;
  try {
    list = await invoke("invites_list");
    $("invite-list-error").classList.add("hidden");
  } catch (err) {
    $("invite-list-error").textContent = msg(String(err));
    $("invite-list-error").classList.remove("hidden");
    return;
  }
  const now = Date.now() / 1000;
  $("invite-list").replaceChildren(
    ...list.map((inv) => {
      const ended = inv.expires <= now || inv.uses >= inv.max_uses;
      const row = document.createElement("div");
      row.className = "entry invite-item" + (ended ? " ended" : "");
      const text = document.createElement("div");
      text.className = "queue-text";
      const title = document.createElement("strong");
      title.className = "mono";
      title.textContent = inv.code;
      const meta = document.createElement("span");
      meta.className = "hint";
      const parts = [ended ? t("invite.ended") : t("invite.until", { date: untilText(inv.expires) }), t("invite.used", { n: inv.uses, max: inv.max_uses })];
      if (inv.account !== supportState?.account?.username) parts.push(inv.account);
      meta.textContent = parts.join(" · ");
      text.append(title, meta);
      if (inv.note) {
        const note = document.createElement("span");
        note.className = "queue-message";
        note.textContent = inv.note;
        text.append(note);
      }
      row.append(text);
      const actions = document.createElement("div");
      actions.className = "actions";
      if (!ended) {
        const copy = document.createElement("button");
        copy.textContent = t("invite.copy");
        copy.addEventListener("click", () => copyText(inv.url, copy));
        actions.append(copy);
      }
      const del = document.createElement("button");
      del.textContent = t("invite.delete");
      del.addEventListener("click", async () => {
        try {
          await invoke("invite_delete", { code: inv.code });
        } catch (err) {
          $("invite-list-error").textContent = msg(String(err));
          $("invite-list-error").classList.remove("hidden");
        }
        refreshInvites();
      });
      actions.append(del);
      row.append(actions);
      return row;
    }),
  );
}

$("invite-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  try {
    const inv = await invoke("invite_create", {
      hours: Number($("invite-hours").value),
      uses: Number($("invite-uses").value),
      note: $("invite-note").value.trim(),
    });
    $("invite-note").value = "";
    $("invite-made-url").value = inv.url;
    $("invite-made-code").textContent = t("invite.code", { code: inv.code });
    $("invite-made").classList.remove("hidden");
    $("invite-list-error").classList.add("hidden");
    copyText(inv.url, $("invite-made-copy"));
  } catch (err) {
    $("invite-list-error").textContent = msg(String(err));
    $("invite-list-error").classList.remove("hidden");
  }
  refreshInvites();
});
$("invite-made-copy").addEventListener("click", () => copyText($("invite-made-url").value, $("invite-made-copy")));
