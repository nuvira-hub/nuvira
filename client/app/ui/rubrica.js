// Rubrica sul server: accesso con l'account, schede dei dispositivi e, per gli
// amministratori, gestione di account e rubriche condivise. Usa $, invoke e
// formatId di main.js; render() chiama renderRubrica() a ogni stato nuovo.

let rubrica = null; // ultimo stato ricevuto (account, books, rubrica_error)
let editingEntry = null; // scheda nella finestra di modifica (null = nuova)
let adminAccounts = [];
let editingAccount = null;
let editingBook = null;

function showMessage(id, text) {
  $(id).textContent = text ?? "";
  $(id).classList.toggle("hidden", !text);
}

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

// Pulsante che chiede un secondo clic entro 3 secondi prima di agire.
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

// --- elenco ---

function renderRubrica(state) {
  rubrica = state;
  const logged = Boolean(state.account);
  $("login-form").classList.toggle("hidden", logged);
  $("rubrica-body").classList.toggle("hidden", !logged);
  $("rubrica-tools").classList.toggle("hidden", !logged);
  $("rubrica-admin").classList.toggle("hidden", !state.account?.admin);
  if (logged) $("rubrica-user").textContent = state.account.username;
  if (state.rubrica_error) showMessage("rubrica-message", state.rubrica_error);
  renderEntries();
  if (!$("admin-dialog").classList.contains("hidden")) renderAdmin();
}

function matches(entry, query) {
  if (!query) return true;
  const digits = query.replace(/\s/g, "");
  const text = [entry.name, entry.company, entry.contact, entry.email, entry.phone, entry.notes];
  return text.some((v) => v.toLowerCase().includes(query)) || (/^\d+$/.test(digits) && entry.device_id.includes(digits));
}

function renderEntries() {
  const books = rubrica?.books ?? [];
  const query = $("rubrica-search").value.trim().toLowerCase();
  const children = [];
  for (const book of books) {
    const entries = book.entries.filter((e) => matches(e, query));
    if (query && entries.length === 0) continue;
    children.push(el("p", "book-title", `${book.name} (${book.entries.length})`));
    if (entries.length === 0) children.push(el("p", "empty", "Nessuna scheda: aggiungine una con «Nuova»."));
    for (const entry of entries) children.push(entryRow(entry));
  }
  if (books.length && children.length === 0) children.push(el("p", "empty", "Nessuna scheda trovata."));
  $("rubrica-list").replaceChildren(...children);
}

function entryRow(entry) {
  const row = el("div", "entry");
  const who = el("div", "who");
  who.append(el("strong", "", entry.name || formatId(entry.device_id)));
  const details = [entry.company, formatId(entry.device_id), entry.contact].filter(Boolean).join(" · ");
  who.append(el("span", "", details));
  row.title = entry.notes || "";
  const connect = el("button", "primary", "Collega");
  connect.title = entry.password ? "Collega con la password salvata" : "Nessuna password salvata: verrà chiesta la conferma o la password";
  connect.addEventListener("click", () => connectEntry(entry));
  const edit = el("button", "ghost", "Modifica");
  edit.addEventListener("click", () => openEntry(entry));
  row.addEventListener("dblclick", () => connectEntry(entry));
  row.append(who, connect, edit);
  return row;
}

async function connectEntry(entry) {
  if (!entry.password) {
    // Senza password salvata la si chiede nel modulo di collegamento.
    $("remote-id").value = formatId(entry.device_id);
    $("remote-password").focus();
    showMessage("rubrica-message", "Nessuna password salvata per questa scheda: inseriscila e premi Collega.");
    return;
  }
  try {
    await call("connect_entry", { id: entry.id }, "rubrica-message");
  } catch {
    // Messaggio già mostrato.
  }
}

// --- scheda ---

function openEntry(entry) {
  editingEntry = entry;
  const books = rubrica?.books ?? [];
  $("entry-book").replaceChildren(
    ...books.map((b) => {
      const option = el("option", "", b.personal ? `${b.name} (solo tua)` : b.name);
      option.value = b.id;
      return option;
    }),
  );
  $("entry-title").textContent = entry ? "Modifica scheda" : "Nuova scheda";
  $("entry-book").value = entry ? entry.book_id : (books.find((b) => b.personal) ?? books[0])?.id;
  $("entry-device").value = entry ? formatId(entry.device_id) : "";
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
  try {
    await call("rubrica_save_entry", { entry }, "entry-message");
    closeEntry();
  } catch {
    // Messaggio già mostrato.
  }
});

$("entry-delete").addEventListener("click", (e) =>
  confirmClick(e.target, "Conferma eliminazione", async () => {
    try {
      await call("rubrica_delete_entry", { id: editingEntry.id }, "entry-message");
      closeEntry();
    } catch {
      // Messaggio già mostrato.
    }
  }),
);
$("entry-cancel").addEventListener("click", closeEntry);

// --- accesso ---

$("login-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  try {
    await call("rubrica_login", { username: $("login-user").value, password: $("login-password").value }, "rubrica-message");
    $("login-password").value = "";
  } catch {
    // Messaggio già mostrato.
  }
});

$("rubrica-logout").addEventListener("click", () => invoke("rubrica_logout"));
$("rubrica-reload").addEventListener("click", () => call("rubrica_refresh", {}, "rubrica-message").catch(() => {}));
$("rubrica-new").addEventListener("click", () => openEntry(null));
$("rubrica-search").addEventListener("input", renderEntries);

// --- cambio password ---

$("rubrica-password").addEventListener("click", () => {
  for (const id of ["pw-old", "pw-new", "pw-confirm"]) $(id).value = "";
  showMessage("password-message", null);
  $("password-dialog").classList.remove("hidden");
  $("pw-old").focus();
});
$("password-cancel").addEventListener("click", () => $("password-dialog").classList.add("hidden"));
$("password-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  if ($("pw-new").value !== $("pw-confirm").value) {
    showMessage("password-message", "Le due password nuove non coincidono");
    return;
  }
  try {
    await call("rubrica_change_password", { old: $("pw-old").value, new: $("pw-new").value }, "password-message");
    $("password-dialog").classList.add("hidden");
  } catch {
    // Messaggio già mostrato.
  }
});

// --- gestione (amministratori) ---

$("rubrica-admin").addEventListener("click", async () => {
  showMessage("admin-message", null);
  resetAccountForm();
  resetBookForm();
  $("admin-dialog").classList.remove("hidden");
  await loadAccounts();
});
$("admin-close").addEventListener("click", () => $("admin-dialog").classList.add("hidden"));

async function loadAccounts() {
  try {
    adminAccounts = await call("rubrica_accounts", {}, "admin-message");
  } catch {
    adminAccounts = [];
  }
  renderAdmin();
}

function renderAdmin() {
  const myId = rubrica?.account?.id;
  $("admin-accounts").replaceChildren(
    ...adminAccounts.map((account) => {
      const row = el("div", "row");
      const name = el("span", "", account.username);
      if (account.admin) name.append(el("span", "badge", "amministratore"));
      const edit = el("button", "ghost", "Modifica");
      edit.addEventListener("click", () => editAccount(account));
      row.append(name, edit);
      if (account.id !== myId) {
        const remove = el("button", "danger", "Elimina");
        remove.addEventListener("click", () =>
          confirmClick(remove, "Conferma", async () => {
            await call("rubrica_delete_account", { id: account.id }, "admin-message").catch(() => {});
            await loadAccounts();
          }),
        );
        row.append(remove);
      }
      return row;
    }),
  );

  const shared = (rubrica?.books ?? []).filter((b) => !b.personal);
  const books = shared.map((book) => {
    const row = el("div", "row");
    const members = book.member_ids.length;
    row.append(el("span", "", `${book.name} · ${members === 1 ? "1 account" : `${members} account`} · ${book.entries.length} schede`));
    const edit = el("button", "ghost", "Modifica");
    edit.addEventListener("click", () => editBook(book));
    const remove = el("button", "danger", "Elimina");
    remove.addEventListener("click", () =>
      confirmClick(remove, "Conferma", () => call("rubrica_delete_book", { id: book.id }, "admin-message").catch(() => {})),
    );
    row.append(edit, remove);
    return row;
  });
  if (books.length === 0) books.push(el("p", "empty", "Nessuna rubrica condivisa. Gli amministratori le vedono tutte."));
  $("admin-books").replaceChildren(...books);
  renderMembers();
}

function renderMembers() {
  const selected = new Set(editingBook?.member_ids ?? []);
  const boxes = adminAccounts.map((account) => {
    const label = el("label", "check");
    const box = document.createElement("input");
    box.type = "checkbox";
    box.value = account.id;
    box.checked = selected.has(account.id);
    label.append(box, ` ${account.username}`);
    return label;
  });
  $("book-members").replaceChildren(...boxes);
}

function editAccount(account) {
  editingAccount = account;
  $("account-user").value = account.username;
  $("account-password").value = "";
  $("account-admin").checked = account.admin;
  $("account-save").textContent = "Salva";
  $("account-cancel").classList.remove("hidden");
  $("account-user").focus();
}

function resetAccountForm() {
  editingAccount = null;
  $("account-form").reset();
  $("account-save").textContent = "Aggiungi";
  $("account-cancel").classList.add("hidden");
}

$("account-cancel").addEventListener("click", resetAccountForm);
$("account-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const args = {
    id: editingAccount?.id ?? 0,
    username: $("account-user").value,
    password: $("account-password").value,
    admin: $("account-admin").checked,
  };
  try {
    await call("rubrica_save_account", args, "admin-message");
    resetAccountForm();
    await loadAccounts();
  } catch {
    // Messaggio già mostrato.
  }
});

function editBook(book) {
  editingBook = book;
  $("book-name").value = book.name;
  $("book-save").textContent = "Salva rubrica";
  $("book-cancel").classList.remove("hidden");
  renderMembers();
  $("book-name").focus();
}

function resetBookForm() {
  editingBook = null;
  $("book-form").reset();
  $("book-save").textContent = "Crea rubrica";
  $("book-cancel").classList.add("hidden");
  renderMembers();
}

$("book-cancel").addEventListener("click", resetBookForm);
$("book-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const memberIds = [...$("book-members").querySelectorAll("input:checked")].map((b) => Number(b.value));
  try {
    await call("rubrica_save_book", { id: editingBook?.id ?? 0, name: $("book-name").value, memberIds }, "admin-message");
    resetBookForm();
  } catch {
    // Messaggio già mostrato.
  }
});

document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape") return;
  for (const id of ["entry-dialog", "password-dialog", "admin-dialog"]) $(id).classList.add("hidden");
});
