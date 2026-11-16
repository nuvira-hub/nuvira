// Rules of unattended access (settings): who may connect, the two-factor code, the
// profiles. Edited here and saved together ("Save the rules"); on Windows with the
// service that asks for an administrator's confirmation. Uses $, invoke, t and msg
// from main.js; main.js calls renderAccess(state).

// Permissions a profile can take away (the session's switches).
const ACCESS_PERMS = ["input", "clipboard", "files", "terminal", "tunnel", "print", "resolution", "audio"];

// The rules being edited: null until the agent tells them, then kept until saved.
let accessDraft = null;
let accessDirty = false;
// Counts the edits: a save clears "dirty" only if none came while it ran (the
// administrator's prompt on macOS and Linux can take a while).
let accessEdits = 0;
// A two-factor secret being set up: shown with its QR code until confirmed.
let accessTotpSetup = null;

function renderAccess(state) {
  // Agents older than these rules do not tell them: nothing to show.
  const show = Boolean(state.service && state.access) && !state.service_managed && !state.password_managed;
  $("access-rules").classList.toggle("hidden", !show);
  if (!show) return;
  $("access-error").classList.toggle("hidden", !state.access.error);
  $("access-error").textContent = state.access.error ? t("access.unreadable") : "";
  if (accessDirty && accessDraft) return;
  accessDraft = {
    allow: state.access.allow.map((a) => ({ id: a.id, name: a.name || "" })),
    allow_attended: state.access.allow_attended,
    two_factor: state.access.two_factor,
    totp: "keep",
    profiles: state.access.profiles.map((p) => ({ name: p.name, off: [...p.off], password: "" })),
  };
  drawAccess();
}

function accessChanged() {
  accessDirty = true;
  accessEdits += 1;
  drawAccess();
}

function formatAccessId(id) {
  const d = String(id).replace(/\D/g, "");
  return d.replace(/(\d{3})(?=\d)/g, "$1 ");
}

function drawAccess() {
  const d = accessDraft;
  if (!d) return;
  // Computers allowed.
  const list = $("access-allow");
  list.replaceChildren();
  for (const [i, a] of d.allow.entries()) {
    const row = document.createElement("div");
    row.className = "row";
    const label = document.createElement("span");
    label.textContent = a.name ? `${a.name} — ${formatAccessId(a.id)}` : formatAccessId(a.id);
    const remove = document.createElement("button");
    remove.type = "button";
    remove.textContent = t("access.remove");
    remove.onclick = () => {
      d.allow.splice(i, 1);
      accessChanged();
    };
    row.append(label, remove);
    list.append(row);
  }
  list.classList.toggle("hidden", d.allow.length === 0);
  $("access-allow-empty").classList.toggle("hidden", d.allow.length !== 0);
  $("access-allow-attended").checked = d.allow_attended;
  // Two-factor code.
  const on = d.totp === "keep" ? d.two_factor : d.totp !== "off";
  $("access-totp-state").textContent = t(on ? "access.totp_on" : "access.totp_off");
  $("access-totp-on").classList.toggle("hidden", on || Boolean(accessTotpSetup));
  $("access-totp-off").classList.toggle("hidden", !on);
  $("access-totp-setup").classList.toggle("hidden", !accessTotpSetup);
  // Profiles.
  const profiles = $("access-profiles");
  profiles.replaceChildren();
  for (const [i, p] of d.profiles.entries()) {
    const row = document.createElement("div");
    row.className = "row";
    const label = document.createElement("span");
    const off = p.off.map((n) => t(`perm.${n}`)).join(", ");
    label.textContent = off ? `${p.name} — ${t("access.profile_without", { what: off })}` : p.name;
    const remove = document.createElement("button");
    remove.type = "button";
    remove.textContent = t("access.remove");
    remove.onclick = () => {
      d.profiles.splice(i, 1);
      accessChanged();
    };
    row.append(label, remove);
    profiles.append(row);
  }
  profiles.classList.toggle("hidden", d.profiles.length === 0);
  $("access-save").disabled = !accessDirty;
}

function accessMessage(text, ok) {
  const m = $("access-message");
  m.textContent = msg(text);
  m.classList.toggle("ok", ok);
  m.classList.toggle("hidden", !text);
}

(function setUpAccess() {
  const box = $("access-profile-off");
  for (const name of ACCESS_PERMS) {
    const label = document.createElement("label");
    label.className = "check";
    const input = document.createElement("input");
    input.type = "checkbox";
    input.value = name;
    const span = document.createElement("span");
    // Translated by main.js once the strings are there.
    span.dataset.i18n = `perm.${name}`;
    span.textContent = t(`perm.${name}`);
    label.append(input, " ", span);
    box.append(label);
  }

  $("access-allow-form").addEventListener("submit", (e) => {
    e.preventDefault();
    const id = $("access-allow-id").value.replace(/\D/g, "");
    if (id.length < 6) return accessMessage(t("access.bad_id"), false);
    if (accessDraft.allow.some((a) => a.id.replace(/\D/g, "") === id)) return accessMessage(t("access.already"), false);
    accessDraft.allow.push({ id, name: $("access-allow-name").value.trim() });
    $("access-allow-form").reset();
    accessMessage("", true);
    accessChanged();
  });
  $("access-allow-attended").addEventListener("change", (e) => {
    accessDraft.allow_attended = e.target.checked;
    accessChanged();
  });

  $("access-totp-on").addEventListener("click", async () => {
    accessTotpSetup = await invoke("access_totp_new");
    $("access-totp-qr").innerHTML = accessTotpSetup.qr_svg;
    $("access-totp-secret").textContent = accessTotpSetup.secret.replace(/(.{4})/g, "$1 ").trim();
    $("access-totp-code").value = "";
    drawAccess();
    $("access-totp-code").focus();
  });
  $("access-totp-cancel").addEventListener("click", () => {
    accessTotpSetup = null;
    drawAccess();
  });
  $("access-totp-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const code = $("access-totp-code").value;
    if (!(await invoke("access_totp_check", { secret: accessTotpSetup.secret, code }))) {
      return accessMessage(t("access.totp_wrong"), false);
    }
    accessDraft.totp = { set: accessTotpSetup.secret };
    accessTotpSetup = null;
    accessMessage(t("access.totp_confirmed"), true);
    accessChanged();
  });
  $("access-totp-off").addEventListener("click", () => {
    accessDraft.totp = "off";
    accessChanged();
  });

  $("access-profile-form").addEventListener("submit", (e) => {
    e.preventDefault();
    const name = $("access-profile-name").value.trim();
    const password = $("access-profile-pw").value;
    if (!name) return;
    if (accessDraft.profiles.some((p) => p.name.toLowerCase() === name.toLowerCase())) {
      return accessMessage(t("access.profile_exists"), false);
    }
    if (password !== $("access-profile-pw2").value) return accessMessage(t("settings.pw_mismatch"), false);
    if (password.length < 8) return accessMessage(t("settings.install_pw_short"), false);
    const off = [...box.querySelectorAll("input:checked")].map((i) => i.value);
    accessDraft.profiles.push({ name, off, password });
    $("access-profile-form").reset();
    accessMessage("", true);
    accessChanged();
  });

  $("access-save").addEventListener("click", async () => {
    const d = accessDraft;
    const edit = {
      allow: d.allow,
      allow_attended: d.allow_attended,
      totp: d.totp,
      profiles: d.profiles.map((p) => ({ name: p.name, off: p.off, password: p.password })),
    };
    $("access-save").disabled = true;
    accessMessage(t("access.saving"), true);
    const edits = accessEdits;
    try {
      accessMessage(await invoke("access_save", { edit }), true);
      // Changes made while saving stay to be saved.
      if (accessEdits === edits) accessDirty = false;
      // The new state comes from the agent (AgentEvent::Access).
      for (const p of d.profiles) p.password = "";
      if (d.totp !== "keep") {
        d.two_factor = d.totp !== "off";
        d.totp = "keep";
      }
    } catch (err) {
      accessMessage(String(err), false);
    } finally {
      drawAccess();
    }
  });
})();
