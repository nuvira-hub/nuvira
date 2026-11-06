// Support queue (NuviraHub Pro, feature "pro.support_queue"): on this computer
// the "Ask for support" card, for the technicians the "Queue" section, refreshed
// every few seconds while it is open. Uses $, invoke, t, msg, formatId and
// showSection from main.js; render() calls renderSupport() on every new state.

let supportState = null;
let queueTimer = null;

function supportAvailable(state) {
  return (state?.features ?? []).includes("pro.support_queue");
}

function renderSupport(state) {
  supportState = state;
  const available = supportAvailable(state);
  $("sq-card").classList.toggle("hidden", !(available && state.controllable));
  const open = Boolean(state.support);
  $("sq-form").classList.toggle("hidden", open);
  $("sq-open").classList.toggle("hidden", !open);
  if (open) {
    const s = state.support;
    $("sq-status").textContent = s.taken_by
      ? t("support.taken", { name: s.taken_by })
      : s.position
        ? t("support.position", { n: s.position })
        : t("support.sent");
  }
  const technician = available && Boolean(state.account);
  $("nav-queue").classList.toggle("hidden", !technician);
  if (!technician && section === "queue") showSection("connect");
}

$("sq-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  try {
    await invoke("request_support", { message: $("sq-message").value.trim() });
    $("sq-message").value = "";
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
    if (r.message) {
      const message = document.createElement("span");
      message.className = "queue-message";
      message.textContent = r.message;
      text.append(message);
    }
    row.append(text);
    if (!r.taken_by) {
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

$("nav-queue").addEventListener("click", () => {
  refreshQueue();
  clearInterval(queueTimer);
  queueTimer = setInterval(() => (section === "queue" ? refreshQueue() : clearInterval(queueTimer)), 3000);
});
