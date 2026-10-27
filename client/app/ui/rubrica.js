// Rubrica sul server: accesso con l'account, schede dei computer, suggerimenti
// nel campo "Controlla un computer" e, per gli amministratori, gestione di
// account e rubriche condivise. Usa $, invoke, formatId e showSection di main.js;
// render() chiama renderRubrica() a ogni stato nuovo.

let rubrica = null; // ultimo stato ricevuto (account, books, rubrica_error)
let editingEntry = null; // scheda nella finestra di modifica (null = nuova)
let suggested = null; // scheda scelta dai suggerimenti del collegamento
let adminAccounts = [];
let adminLoaded = false;
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

function allEntries() {
  return (rubrica?.books ?? []).flatMap((b) => b.entries);
}

function entryTitle(entry) {
  return entry.name || formatId(entry.device_id);
}

function entryDetails(entry) {
  return [entry.company, formatId(entry.device_id), entry.contact].filter(Boolean).join(" · ");
}

// --- stato ---

function renderRubrica(state) {
  rubrica = state;
  const account = state.account;
  $("login-form").classList.toggle("hidden", Boolean(account));
  $("rubrica-list").classList.toggle("hidden", !account);
  $("rubrica-tools").classList.toggle("hidden", !account);
  $("nav-admin").classList.toggle("hidden", !account?.admin);
  $("account-out").classList.toggle("hidden", Boolean(account));
  $("account-in").classList.toggle("hidden", !account);
  if (account) $("account-name").textContent = account.username;
  if (state.rubrica_error) showMessage("rubrica-message", state.rubrica_error);
  $("connect-hint").textContent = account
    ? "Scrivi l'ID oppure il nome, l'azienda o il referente: i suggerimenti arrivano dalla rubrica."
    : "Scrivi l'ID del computer da controllare.";
  $("remote-id").placeholder = account ? "ID o nome dalla rubrica" : "ID";
  renderEntries();
  if (account?.admin && !adminLoaded) loadAccounts();
  if (!account) {
    adminLoaded = false;
    adminAccounts = [];
  }
  renderAdmin();
}

// --- elenco della rubrica ---

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
    const label = book.personal ? `${book.name} · solo tua` : `${book.name} · condivisa`;
    children.push(el("p", "book-title", `${label} (${book.entries.length})`));
    if (entries.length === 0) children.push(el("p", "empty", "Nessuna scheda: aggiungine una con «Nuova scheda»."));
    for (const entry of entries) children.push(entryRow(entry));
  }
  if (books.length && children.length === 0) children.push(el("p", "empty", "Nessuna scheda trovata."));
  $("rubrica-list").replaceChildren(...children);
}

function entryRow(entry) {
  const row = el("div", "entry");
  const initial = (entry.name || entry.company || "#").trim().charAt(0).toUpperCase();
  const who = el("div", "who");
  who.append(el("strong", "", entryTitle(entry)), el("span", "", entryDetails(entry)));
  row.title = entry.notes || "";
  const edit = el("button", "edit", "Modifica");
  edit.addEventListener("click", () => openEntry(entry));
  const connect = el("button", "primary", "Collega");
  connect.addEventListener("click", () => connectEntry(entry));
  row.addEventListener("dblclick", () => connectEntry(entry));
  row.append(el("span", "avatar", initial), who, edit, connect);
  return row;
}

async function connectEntry(entry) {
  if (!entry.password) {
    // Senza password salvata la si chiede nella sezione Connessione.
    choose(entry);
    showSection("connect");
    $("remote-password").focus();
    showMessage("connect-message", "Nessuna password salvata per questa scheda: inseriscila e premi Collega.");
    return;
  }
  try {
    await call("connect_entry", { id: entry.id }, "rubrica-message");
  } catch {
    // Messaggio già mostrato.
  }
}

// --- suggerimenti nel campo "Controlla un computer" ---

// Scheda scelta, se l'ID nel campo è ancora il suo (usata da main.js al Collega).
function chosenEntry() {
  const digits = $("remote-id").value.replace(/\D/g, "");
  return suggested && suggested.device_id === digits ? suggested : null;
}

function choose(entry) {
  suggested = entry;
  $("remote-id").value = formatId(entry.device_id);
  $("remote-password").value = "";
  $("remote-password").placeholder = entry.password ? "Password salvata in rubrica" : "Password";
  hideSuggestions();
}

function hideSuggestions() {
  $("suggestions").classList.add("hidden");
}

function renderSuggestions() {
  const query = $("remote-id").value.trim().toLowerCase();
  if (!chosenEntry()) $("remote-password").placeholder = "Password";
  if (!rubrica?.account || query.length < 2) {
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
      // mousedown: arriva prima che il campo perda il fuoco e chiuda l'elenco.
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

// --- accesso e account ---

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
$("goto-login").addEventListener("click", () => {
  showSection("rubrica");
  $("login-user").focus();
});
$("rubrica-reload").addEventListener("click", () => call("rubrica_refresh", {}, "rubrica-message").catch(() => {}));
$("rubrica-new").addEventListener("click", () => openEntry(null));
$("rubrica-search").addEventListener("input", renderEntries);

$("password-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  if ($("pw-new").value !== $("pw-confirm").value) {
    showMessage("password-message", "Le due password nuove non coincidono");
    return;
  }
  try {
    await call("rubrica_change_password", { old: $("pw-old").value, new: $("pw-new").value }, "password-message");
    $("password-form").reset();
    showMessage("password-message", null);
  } catch {
    // Messaggio già mostrato.
  }
});

// --- amministrazione ---

async function loadAccounts() {
  adminLoaded = true;
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
      if (account.id === myId) name.append(el("span", "badge", "tu"));
      const edit = el("button", "", "Modifica");
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
    const edit = el("button", "", "Modifica");
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
  const checked = new Set(
    [...$("book-members").querySelectorAll("input:checked")].map((b) => Number(b.value)),
  );
  const selected = editingBook && !$("book-members").dataset.touched ? new Set(editingBook.member_ids) : checked;
  const boxes = adminAccounts.map((account) => {
    const label = el("label", "check");
    const box = document.createElement("input");
    box.type = "checkbox";
    box.value = account.id;
    box.checked = selected.has(account.id);
    box.addEventListener("change", () => ($("book-members").dataset.touched = "1"));
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
  delete $("book-members").dataset.touched;
  $("book-name").value = book.name;
  $("book-save").textContent = "Salva rubrica";
  $("book-cancel").classList.remove("hidden");
  renderMembers();
  $("book-name").focus();
}

function resetBookForm() {
  editingBook = null;
  delete $("book-members").dataset.touched;
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
  if (e.key === "Escape") closeEntry();
});
