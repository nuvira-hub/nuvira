// Address book on the server: sign-in, computer entries, suggestions in the
// "Control a computer" field and, for administrators, accounts and shared
// address books. Uses $, invoke, t, msg, formatId and showSection from main.js;
// render() calls renderAddressBook() on every new state.

let addressBook = null; // last state received (account, books, address_book_error)
let editingEntry = null; // entry in the edit dialog (null = new)
let suggested = null; // entry picked from the connection suggestions
let adminAccounts = [];
let adminLoaded = false;
let editingAccount = null;
let editingBook = null;
// Limits a session can have: field of the Policy and its label.
const LIMITS = [
  ["confirm_always", "admin.policy_confirm"],
  ["no_input", "admin.policy_input"],
  ["no_files", "admin.policy_files"],
  ["no_clipboard", "admin.policy_clipboard"],
  ["no_privacy", "admin.policy_privacy"],
  ["no_terminal", "admin.policy_terminal"],
  ["no_tunnel", "admin.policy_tunnel"],
  ["no_print", "admin.policy_print"],
  ["no_restart", "admin.policy_restart"],
  ["no_resolution", "admin.policy_resolution"],
  ["no_record", "admin.policy_no_record"],
  ["record_always", "admin.policy_record"],
];
// Whose permissions the panel shows: {kind: "account" | "member" | "anonymous", account, book}.
let limitsTarget = null;

function showMessage(id, text) {
  $(id).classList.remove("ok");
  $(id).textContent = msg(text) ?? "";
  $(id).classList.toggle("hidden", !text);
}

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

// Button that asks for a second click within 3 seconds before acting.
function confirmClick(button, label, action) {
  if (button.dataset.armed) {
    delete button.dataset.armed;
    action();
    return;
  }
  const original = button.textContent;
  button.dataset.armed = "1";
  button.textContent = label;
  setTimeout(() => {
    delete button.dataset.armed;
    button.textContent = original;
  }, 3000);
}

async function call(command, args, messageId) {
  try {
    showMessage(messageId, null);
    return await invoke(command, args);
  } catch (err) {
    showMessage(messageId, String(err));
    throw err;
  }
}

// What the signed-in account may do on a book ("" from servers before roles).
function canEditBook(book) {
  return book?.access !== "view";
}

function bookOf(entry) {
  return (addressBook?.books ?? []).find((b) => b.id === entry.book_id);
}

function managesBooks() {
  return (addressBook?.books ?? []).some((b) => b.access === "manage");
}

function allEntries() {
  return (addressBook?.books ?? []).flatMap((b) => b.entries);
}

function entryTitle(entry) {
  return entry.name || formatId(entry.device_id);
}

function entryDetails(entry) {
  return [entry.company, formatId(entry.device_id), entry.contact].filter(Boolean).join(" · ");
}

// --- state ---

function renderAddressBook(state) {
  addressBook = state;
  const account = state.account;
  // Accounts and shared books only with the server's address book module (Pro);
  // the book on this computer is always there.
  const pro = (state.features ?? []).includes("pro.address_book");
  // The rule "record every session" only with the recordings on the server.
  $("policy-record-row").classList.toggle("hidden", !(state.features ?? []).includes("pro.recording"));
  // Permissions per account and per session (Pro).
  $("limits-admin").classList.toggle("hidden", !limitsAvailable());
  // Single sign-on: "pro.sso=<provider name>".
  const sso = (state.features ?? []).find((f) => f.startsWith("pro.sso="))?.slice(8);
  $("login-sso").classList.toggle("hidden", !sso || Boolean(account));
  if (sso && !$("login-sso").disabled) $("login-sso").textContent = t("address_book.sso", { name: sso });
  $("login-form").classList.toggle("hidden", Boolean(account) || !pro);
  $("address-book-list").classList.remove("hidden");
  $("address-book-tools").classList.remove("hidden");
  // Administration: administrators, and who manages a shared book (only its books).
  $("nav-admin").classList.toggle("hidden", document.body.classList.contains("quick") || (!account?.admin && !managesBooks()));
  $("admin-only").classList.toggle("hidden", !account?.admin);
  $("account-title").classList.toggle("hidden", !pro && !account);
  $("account-out").classList.toggle("hidden", Boolean(account) || !pro);
  $("account-in").classList.toggle("hidden", !account);
  if (account) $("account-name").textContent = `${t("settings.signed_in", { name: account.username })} · ${t(`role.${account.role}`)}`;
  renderTwoFactor(account);
  showMessage("address-book-message", state.address_book_error ?? null);
  $("connect-hint").textContent = t("connect.hint_address_book");
  $("remote-id").placeholder = t("connect.id_placeholder_address_book");
  renderEntries();
  if (!editingAccount) $("account-save").textContent = t("admin.add");
  if (!editingBook) $("book-save").textContent = t("admin.create_book");
  if ((account?.admin || managesBooks()) && !adminLoaded) loadAccounts();
  if (!account) {
    adminLoaded = false;
    adminAccounts = [];
  }
  renderAdmin();
}

// --- address book list ---

function matches(entry, query) {
  if (!query) return true;
  const digits = query.replace(/\s/g, "");
  const text = [entry.name, entry.company, entry.contact, entry.email, entry.phone, entry.notes, entry.group, ...entry.tags];
  return text.some((v) => v.toLowerCase().includes(query)) || (/^\d+$/.test(digits) && entry.device_id.includes(digits));
}

function renderEntries() {
  const books = addressBook?.books ?? [];
  const query = $("address-book-search").value.trim().toLowerCase();
  const children = [];
  for (const book of books) {
    const entries = book.entries.filter((e) => matches(e, query));
    if (query && entries.length === 0) continue;
    const label = book.local
      ? bookName(book)
      : book.personal
        ? t("address_book.personal", { name: bookName(book) })
        : t("address_book.shared", { name: book.name });
    children.push(el("p", "book-title", `${label} (${book.entries.length})`));
    if (entries.length === 0) children.push(el("p", "empty", t("address_book.empty_book")));
    // The server sends them sorted by group: a heading where the group changes.
    let group = null;
    for (const entry of entries) {
      if (entry.group && entry.group !== group) children.push(el("p", "group-title", entry.group));
      group = entry.group;
      children.push(entryRow(entry));
    }
  }
  if (books.length && children.length === 0) children.push(el("p", "empty", t("address_book.not_found")));
  $("address-book-list").replaceChildren(...children);
}

// The personal and the local address book are named in the interface language.
function bookName(book) {
  if (book.local) return t("address_book.local");
  return book.personal ? t("address_book.personal_book") : book.name;
}

function entryRow(entry) {
  const row = el("div", "entry");
  const initial = (entry.name || entry.company || "#").trim().charAt(0).toUpperCase();
  const who = el("div", "who");
  who.append(el("strong", "", entryTitle(entry)), el("span", "", entryDetails(entry)));
  if (entry.tags.length) {
    const tags = el("span", "tags");
    tags.append(...entry.tags.map((tag) => el("span", "tag", tag)));
    who.append(tags);
  }
  row.title = entry.notes || "";
  const avatar = el("span", "avatar", initial);
  const presence = el("span", entry.online ? "presence online" : "presence");
  presence.title = entry.online
    ? t("address_book.online")
    : entry.last_seen
      ? t("address_book.last_seen", { when: new Date(entry.last_seen * 1000).toLocaleString(document.documentElement.lang) })
      : t("address_book.never_seen");
  avatar.append(presence);
  const edit = el("button", "edit", t("address_book.edit"));
  edit.addEventListener("click", () => openEntry(entry));
  // Connect only: no editing (the password stays hidden).
  if (!canEditBook(bookOf(entry))) edit.classList.add("hidden");
  const connect = el("button", "primary", t("address_book.connect"));
  connect.addEventListener("click", () => connectEntry(entry));
  row.addEventListener("dblclick", () => connectEntry(entry));
  row.append(avatar, who, edit);
  // Light RMM (NuviraHub Pro): inventory, health and wake-on-LAN (rmm.js).
  if ((addressBook?.features ?? []).includes("pro.inventory")) {
    const info = el("button", "", t("rmm.info"));
    info.addEventListener("click", () => openInventory(entry));
    row.append(info);
  }
  const terminal = el("button", "", t("address_book.terminal"));
  terminal.title = t("address_book.terminal_hint");
  terminal.addEventListener("click", () => connectEntry(entry, true));
  row.append(terminal, connect);
  return row;
}

async function connectEntry(entry, terminal = false) {
  if (!entry.password) {
    // Without a saved password it is asked for in the Connection section.
    choose(entry);
    showSection("connect");
    $("remote-password").focus();
    showMessage("connect-message", t("connect.no_saved_password"));
    return;
  }
  try {
    await call("connect_entry", { id: entry.id, terminal }, "address-book-message");
  } catch {
    // Message already shown.
  }
}

// --- suggestions in the "Control a computer" field ---

// Picked entry, if the field still holds its ID (used by main.js on Connect).
function chosenEntry() {
  const digits = $("remote-id").value.replace(/\D/g, "");
  return suggested && suggested.device_id === digits ? suggested : null;
}

function choose(entry) {
  suggested = entry;
  $("remote-id").value = formatId(entry.device_id);
  $("remote-password").value = "";
  $("remote-password").placeholder = t(entry.password ? "connect.saved_password" : "connect.password_placeholder");
  hideSuggestions();
}

function hideSuggestions() {
  $("suggestions").classList.add("hidden");
}

function renderSuggestions() {
  const query = $("remote-id").value.trim().toLowerCase();
  if (!chosenEntry()) $("remote-password").placeholder = t("connect.password_placeholder");
  if (query.length < 2) {
    hideSuggestions();
    return;
  }
  const found = allEntries()
    .filter((e) => matches(e, query))
    .slice(0, 6);
  if (found.length === 0 || (found.length === 1 && chosenEntry() === found[0])) {
    hideSuggestions();
    return;
  }
  $("suggestions").replaceChildren(
    ...found.map((entry, i) => {
      const item = el("div", i === 0 ? "suggestion selected" : "suggestion");
      item.setAttribute("role", "option");
      item.append(el("strong", "", entryTitle(entry)), el("span", "", entryDetails(entry)));
      // mousedown: it arrives before the field loses focus and closes the list.
      item.addEventListener("mousedown", (e) => {
        e.preventDefault();
        choose(entry);
        $("remote-password").focus();
      });
      item.entry = entry;
      return item;
    }),
  );
  $("suggestions").classList.remove("hidden");
}

$("remote-id").addEventListener("input", renderSuggestions);
$("remote-id").addEventListener("blur", hideSuggestions);
$("remote-id").addEventListener("keydown", (e) => {
  const items = [...$("suggestions").children];
  if ($("suggestions").classList.contains("hidden") || items.length === 0) return;
  const index = items.findIndex((i) => i.classList.contains("selected"));
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    e.preventDefault();
    const next = (index + (e.key === "ArrowDown" ? 1 : items.length - 1)) % items.length;
    items.forEach((item, i) => item.classList.toggle("selected", i === next));
  } else if (e.key === "Enter" || e.key === "Tab") {
    e.preventDefault();
    choose(items[Math.max(index, 0)].entry);
    $("remote-password").focus();
  } else if (e.key === "Escape") {
    hideSuggestions();
  }
});

// --- entry ---

function openEntry(entry) {
  editingEntry = entry;
  const books = (addressBook?.books ?? []).filter(canEditBook);
  $("entry-book").replaceChildren(
    ...books.map((b) => {
      const option = el("option", "", b.personal ? t("address_book.personal_option", { name: bookName(b) }) : bookName(b));
      option.value = b.id;
      return option;
    }),
  );
  $("entry-title").textContent = t(entry ? "entry.edit" : "entry.new");
  $("entry-book").value = entry ? entry.book_id : (books.find((b) => b.personal) ?? books.find((b) => b.local) ?? books[0])?.id;
  $("entry-device").value = entry ? formatId(entry.device_id) : "";
  $("entry-group").value = entry ? entry.group : "";
  $("entry-tags").value = entry ? entry.tags.join(", ") : "";
  // Groups already used, as suggestions.
  const groups = [...new Set(books.flatMap((b) => b.entries.map((e) => e.group)).filter(Boolean))].sort();
  $("entry-groups").replaceChildren(...groups.map((g) => new Option(g)));
  for (const field of ["name", "company", "contact", "phone", "email", "password", "notes"]) {
    $(`entry-${field}`).value = entry ? entry[field] : "";
  }
  $("entry-delete").classList.toggle("hidden", !entry);
  showMessage("entry-message", null);
  $("entry-dialog").classList.remove("hidden");
  (entry ? $("entry-name") : $("entry-device")).focus();
}

function closeEntry() {
  $("entry-dialog").classList.add("hidden");
  editingEntry = null;
}

$("entry-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const entry = {
    id: editingEntry?.id ?? 0,
    book_id: Number($("entry-book").value),
    device_id: $("entry-device").value,
  };
  for (const field of ["name", "company", "contact", "phone", "email", "password", "notes"]) {
    entry[field] = $(`entry-${field}`).value;
  }
  entry.group = $("entry-group").value;
  entry.tags = $("entry-tags").value.split(",").map((tag) => tag.trim()).filter(Boolean);
  try {
    await call("address_book_save_entry", { entry }, "entry-message");
    closeEntry();
  } catch {
    // Message already shown.
  }
});

$("entry-delete").addEventListener("click", (e) =>
  confirmClick(e.target, t("entry.confirm_delete"), async () => {
    try {
      await call("address_book_delete_entry", { id: editingEntry.id }, "entry-message");
      closeEntry();
    } catch {
      // Message already shown.
    }
  }),
);
$("entry-cancel").addEventListener("click", closeEntry);

// --- sign-in and account ---

$("login-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const otp = $("login-otp").value.trim();
  try {
    await call(
      "address_book_login",
      { username: $("login-user").value, password: $("login-password").value, otp: otp || null },
      "address-book-message",
    );
    $("login-password").value = "";
    $("login-otp").value = "";
    $("login-otp").classList.add("hidden");
  } catch (err) {
    if (String(err) === "@server.otp_required") {
      // Password right: the code of the authenticator app is needed.
      $("login-otp").classList.remove("hidden");
      $("login-otp").focus();
    }
  }
});

$("address-book-logout").addEventListener("click", () => invoke("address_book_logout"));

$("login-sso").addEventListener("click", async () => {
  const button = $("login-sso");
  const label = button.textContent;
  button.disabled = true;
  button.textContent = t("address_book.sso_waiting");
  try {
    await call("address_book_sso_login", {}, "address-book-message");
  } catch {
    // Message already shown.
  } finally {
    button.disabled = false;
    button.textContent = label;
  }
});
$("goto-login").addEventListener("click", () => {
  showSection("address-book");
  $("login-user").focus();
});
$("address-book-reload").addEventListener("click", () => call("address_book_refresh", {}, "address-book-message").catch(() => {}));
$("address-book-new").addEventListener("click", () => openEntry(null));
$("address-book-search").addEventListener("input", renderEntries);

$("password-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  if ($("pw-new").value !== $("pw-confirm").value) {
    showMessage("password-message", t("settings.pw_mismatch"));
    return;
  }
  try {
    await call("address_book_change_password", { old: $("pw-old").value, new: $("pw-new").value }, "password-message");
    $("password-form").reset();
    showMessage("password-message", null);
  } catch {
    // Message already shown.
  }
});

// --- two-factor authentication of the account ---

let tfShownFor = null;

function renderTwoFactor(account) {
  if (!account) {
    tfShownFor = null;
    return;
  }
  const available = (addressBook?.features ?? []).includes("pro.two_factor");
  $("tf-status").textContent = t(account.two_factor ? "tf.state_on" : "tf.state_off");
  // A new state from the app: start over (unless recovery codes are on screen).
  const key = `${account.id}:${account.two_factor}`;
  if (tfShownFor === key) return;
  tfShownFor = key;
  if ($("tf-codes").classList.contains("hidden")) {
    $("tf-setup").classList.add("hidden");
    showMessage("tf-message", null);
  }
  $("tf-start").classList.toggle("hidden", account.two_factor || !available);
  $("tf-disable-form").classList.toggle("hidden", !account.two_factor);
}

$("tf-start").addEventListener("click", async () => {
  let setup;
  try {
    setup = await call("address_book_two_factor_setup", {}, "tf-message");
  } catch {
    return;
  }
  const bytes = new Uint8Array(setup.qr_png);
  let binary = "";
  for (const b of bytes) binary += String.fromCharCode(b);
  $("tf-qr").src = `data:image/png;base64,${btoa(binary)}`;
  $("tf-secret").textContent = setup.secret.replace(/(.{4})/g, "$1 ").trim();
  $("tf-start").classList.add("hidden");
  $("tf-codes").classList.add("hidden");
  $("tf-setup").classList.remove("hidden");
  $("tf-code").value = "";
  $("tf-code").focus();
});

$("tf-enable-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  try {
    const codes = await call("address_book_two_factor_enable", { code: $("tf-code").value }, "tf-message");
    $("tf-setup").classList.add("hidden");
    $("tf-codes-list").replaceChildren(...codes.map((c) => el("li", "", c)));
    $("tf-codes").classList.remove("hidden");
  } catch {
    // Message already shown.
  }
});

$("tf-disable-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  try {
    await call("address_book_two_factor_disable", { password: $("tf-password").value, code: $("tf-disable-code").value }, "tf-message");
    $("tf-disable-form").reset();
    $("tf-codes").classList.add("hidden");
    showMessage("tf-message", t("tf.disabled"));
  } catch {
    // Message already shown.
  }
});

// --- administration ---

function showPolicy(policy) {
  $("policy-confirm").checked = policy.confirm_always;
  $("policy-files").checked = policy.no_files;
  $("policy-clipboard").checked = policy.no_clipboard;
  $("policy-privacy").checked = policy.no_privacy;
  $("policy-terminal").checked = policy.no_terminal;
  $("policy-tunnel").checked = policy.no_tunnel;
  $("policy-print").checked = policy.no_print;
  $("policy-input").checked = policy.no_input;
  $("policy-restart").checked = policy.no_restart;
  $("policy-resolution").checked = policy.no_resolution;
  $("policy-no-record").checked = policy.no_record;
  $("policy-record").checked = policy.record_always;
}

$("policy-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const policy = {
    confirm_always: $("policy-confirm").checked,
    no_files: $("policy-files").checked,
    no_clipboard: $("policy-clipboard").checked,
    no_privacy: $("policy-privacy").checked,
    no_terminal: $("policy-terminal").checked,
    no_tunnel: $("policy-tunnel").checked,
    no_print: $("policy-print").checked,
    no_input: $("policy-input").checked,
    no_restart: $("policy-restart").checked,
    no_resolution: $("policy-resolution").checked,
    no_record: $("policy-no-record").checked,
    record_always: $("policy-record").checked,
  };
  try {
    showPolicy(await call("address_book_save_policy", { policy }, "policy-message"));
    $("policy-message").textContent = t("admin.policy_saved");
    $("policy-message").classList.add("ok");
    $("policy-message").classList.remove("hidden");
  } catch {
    $("policy-message").classList.remove("ok");
  }
});

async function loadAccounts() {
  adminLoaded = true;
  if (addressBook?.account?.admin) call("address_book_policy", {}, "policy-message").then(showPolicy).catch(() => {});
  try {
    adminAccounts = await call("address_book_accounts", {}, "admin-message");
  } catch {
    adminAccounts = [];
  }
  renderAdmin();
}

function renderAdmin() {
  const myId = addressBook?.account?.id;
  $("admin-accounts").replaceChildren(
    ...adminAccounts.map((account) => {
      const row = el("div", "row");
      const name = el("span", "", account.username);
      name.append(el("span", "badge", t(`role.${account.role}`)));
      if (account.id === myId) name.append(el("span", "badge", t("admin.badge_you")));
      if (account.two_factor) name.append(el("span", "badge", t("admin.badge_two_factor")));
      if (hasLimits(account.limits)) name.append(el("span", "badge", t("limits.badge")));
      const edit = el("button", "", t("admin.edit"));
      edit.addEventListener("click", () => editAccount(account));
      row.append(name);
      if (limitsAvailable()) {
        const perms = el("button", "", t("limits.button"));
        perms.addEventListener("click", () =>
          openLimits({ kind: "account", account }, account.limits, t("limits.of_account", { name: account.username })),
        );
        row.append(perms);
      }
      if (account.two_factor) {
        const reset = el("button", "", t("admin.reset_two_factor"));
        reset.addEventListener("click", () =>
          confirmClick(reset, t("admin.confirm"), async () => {
            await call("address_book_two_factor_reset", { id: account.id }, "admin-message").catch(() => {});
            loadAccounts();
          }),
        );
        row.append(reset);
      }
      row.append(edit);
      if (account.id !== myId) {
        const remove = el("button", "danger", t("admin.delete"));
        remove.addEventListener("click", () =>
          confirmClick(remove, t("admin.confirm"), async () => {
            await call("address_book_delete_account", { id: account.id }, "admin-message").catch(() => {});
            await loadAccounts();
          }),
        );
        row.append(remove);
      }
      return row;
    }),
  );

  const admin = Boolean(addressBook?.account?.admin);
  const shared = (addressBook?.books ?? []).filter((b) => !b.personal && !b.local && (admin || b.access === "manage"));
  // Who manages a book edits it, but creates and deletes none.
  $("book-form").classList.toggle("hidden", !admin && !editingBook);
  const books = shared.map((book) => {
    const row = el("div", "row");
    const members = book.member_ids.length;
    const memberText = members === 1 ? t("admin.members_one") : t("admin.members_many", { n: members });
    const count = book.entries.length;
    const entryText = count === 1 ? t("admin.entries_one") : t("admin.entries_many", { n: count });
    row.append(el("span", "", t("admin.book_row", { name: book.name, members: memberText, entries: entryText })));
    const edit = el("button", "", t("admin.edit"));
    edit.addEventListener("click", () => editBook(book));
    const remove = el("button", "danger", t("admin.delete"));
    remove.addEventListener("click", () =>
      confirmClick(remove, t("admin.confirm"), () => call("address_book_delete_book", { id: book.id }, "admin-message").catch(() => {})),
    );
    row.append(edit);
    if (admin) row.append(remove);
    return row;
  });
  if (books.length === 0) books.push(el("p", "empty", t("admin.no_books")));
  $("admin-books").replaceChildren(...books);
  renderMembers();
}

// One level per account (administrators see every book anyway).
function renderMembers() {
  const chosen = new Map(
    [...$("book-members").querySelectorAll("select")].map((s) => [Number(s.dataset.account), s.value]),
  );
  const levels =
    editingBook && !$("book-members").dataset.touched
      ? new Map((editingBook.members ?? []).map((m) => [m.account_id, m.level]))
      : chosen;
  const rows = adminAccounts
    .filter((account) => account.role !== "admin")
    .map((account) => {
      const row = el("label", "member");
      const select = document.createElement("select");
      select.dataset.account = account.id;
      for (const level of ["", "view", "edit", "manage"]) {
        select.append(new Option(t(level ? `level.${level}` : "level.none"), level, false, (levels.get(account.id) ?? "") === level));
      }
      select.addEventListener("change", () => ($("book-members").dataset.touched = "1"));
      row.append(el("span", "", `${account.username} · ${t(`role.${account.role}`)}`), select);
      // Limits of a member already saved in this book (administrators).
      const member = editingBook?.members?.find((m) => m.account_id === account.id);
      if (member && limitsAvailable() && addressBook?.account?.admin) {
        const perms = el("button", hasLimits(member.limits) ? "limited" : "", t("limits.button"));
        perms.type = "button";
        perms.addEventListener("click", (e) => {
          e.preventDefault();
          const title = t("limits.of_member", { name: account.username, book: editingBook.name });
          openLimits({ kind: "member", account, book: editingBook }, member.limits, title);
        });
        row.append(perms);
      }
      return row;
    });
  $("book-members").replaceChildren(...rows);
}

function editAccount(account) {
  editingAccount = account;
  $("account-user").value = account.username;
  $("account-password").value = "";
  $("account-role").value = account.role;
  $("account-save").textContent = t("admin.save");
  $("account-cancel").classList.remove("hidden");
  $("account-user").focus();
}

function resetAccountForm() {
  editingAccount = null;
  $("account-form").reset();
  $("account-save").textContent = t("admin.add");
  $("account-cancel").classList.add("hidden");
}

$("account-cancel").addEventListener("click", resetAccountForm);
$("account-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const args = {
    id: editingAccount?.id ?? 0,
    username: $("account-user").value,
    password: $("account-password").value,
    role: $("account-role").value,
  };
  try {
    await call("address_book_save_account", args, "admin-message");
    resetAccountForm();
    await loadAccounts();
  } catch {
    // Message already shown.
  }
});

function editBook(book) {
  editingBook = book;
  delete $("book-members").dataset.touched;
  $("book-name").value = book.name;
  $("book-save").textContent = t("admin.save_book");
  $("book-cancel").classList.remove("hidden");
  $("book-form").classList.remove("hidden");
  renderMembers();
  $("book-name").focus();
}

function resetBookForm() {
  editingBook = null;
  delete $("book-members").dataset.touched;
  $("book-form").reset();
  $("book-save").textContent = t("admin.create_book");
  $("book-cancel").classList.add("hidden");
  $("book-form").classList.toggle("hidden", !addressBook?.account?.admin);
  renderMembers();
}

$("book-cancel").addEventListener("click", resetBookForm);
$("book-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const members = [...$("book-members").querySelectorAll("select")]
    .filter((s) => s.value)
    .map((s) => ({ account_id: Number(s.dataset.account), level: s.value }));
  try {
    await call("address_book_save_book", { id: editingBook?.id ?? 0, name: $("book-name").value, members }, "admin-message");
    resetBookForm();
  } catch {
    // Message already shown.
  }
});

// --- permissions of the sessions (Pro) ---

function limitsAvailable() {
  return Boolean(addressBook?.account?.admin) && (addressBook?.features ?? []).includes("pro.limits");
}

function hasLimits(limits) {
  return Boolean(limits) && LIMITS.some(([field]) => limits[field]);
}

function openLimits(target, limits, title) {
  limitsTarget = target;
  const recordings = (addressBook?.features ?? []).includes("pro.recording");
  $("limits-title").textContent = title;
  $("limits-checks").replaceChildren(
    ...LIMITS.filter(([field]) => field !== "record_always" || recordings).map(([field, label]) => {
      const row = el("label", "check");
      const box = document.createElement("input");
      box.type = "checkbox";
      box.dataset.field = field;
      box.checked = Boolean(limits?.[field]);
      row.append(box, " ", el("span", "", t(label)));
      return row;
    }),
  );
  showMessage("limits-message", null);
  $("limits-panel").classList.remove("hidden");
  $("limits-panel").scrollIntoView({ block: "nearest" });
}

function closeLimits() {
  limitsTarget = null;
  $("limits-panel").classList.add("hidden");
}

$("limits-cancel").addEventListener("click", closeLimits);
$("limits-anonymous").addEventListener("click", async () => {
  try {
    const limits = await call("address_book_anonymous_limits", {}, "policy-message");
    openLimits({ kind: "anonymous" }, limits, t("limits.of_anonymous"));
  } catch {
    // Message already shown.
  }
});
$("limits-panel").addEventListener("submit", async (e) => {
  e.preventDefault();
  const target = limitsTarget;
  if (!target) return;
  const limits = Object.fromEntries(LIMITS.map(([field]) => [field, false]));
  for (const box of $("limits-checks").querySelectorAll("input")) limits[box.dataset.field] = box.checked;
  try {
    if (target.kind === "anonymous") {
      await call("address_book_save_anonymous_limits", { limits }, "limits-message");
    } else if (target.kind === "account") {
      const a = target.account;
      await call("address_book_save_account", { id: a.id, username: a.username, password: "", role: a.role, limits }, "limits-message");
      await loadAccounts();
    } else {
      // The members stay as they are; only this one's limits change.
      const book = target.book;
      const members = book.members.map((m) => ({
        account_id: m.account_id,
        level: m.level,
        limits: m.account_id === target.account.id ? limits : null,
      }));
      await call("address_book_save_book", { id: book.id, name: book.name, members }, "limits-message");
      resetBookForm();
    }
    closeLimits();
    showMessage("admin-message", t("limits.saved"));
    $("admin-message").classList.add("ok");
  } catch {
    // Message already shown.
  }
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") closeEntry();
});
