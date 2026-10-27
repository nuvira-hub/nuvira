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

function showMessage(id, text) {
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
  $("login-form").classList.toggle("hidden", Boolean(account));
  $("address-book-list").classList.toggle("hidden", !account);
  $("address-book-tools").classList.toggle("hidden", !account);
  $("nav-admin").classList.toggle("hidden", !account?.admin);
  $("account-out").classList.toggle("hidden", Boolean(account));
  $("account-in").classList.toggle("hidden", !account);
  if (account) $("account-name").textContent = t("settings.signed_in", { name: account.username });
  if (state.address_book_error) showMessage("address-book-message", state.address_book_error);
  $("connect-hint").textContent = t(account ? "connect.hint_address_book" : "connect.hint");
  $("remote-id").placeholder = t(account ? "connect.id_placeholder_address_book" : "connect.id_placeholder");
  renderEntries();
  if (!editingAccount) $("account-save").textContent = t("admin.add");
  if (!editingBook) $("book-save").textContent = t("admin.create_book");
  if (account?.admin && !adminLoaded) loadAccounts();
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
  const text = [entry.name, entry.company, entry.contact, entry.email, entry.phone, entry.notes];
  return text.some((v) => v.toLowerCase().includes(query)) || (/^\d+$/.test(digits) && entry.device_id.includes(digits));
}

function renderEntries() {
  const books = addressBook?.books ?? [];
  const query = $("address-book-search").value.trim().toLowerCase();
  const children = [];
  for (const book of books) {
    const entries = book.entries.filter((e) => matches(e, query));
    if (query && entries.length === 0) continue;
    const label = book.personal ? t("address_book.personal", { name: bookName(book) }) : t("address_book.shared", { name: book.name });
    children.push(el("p", "book-title", `${label} (${book.entries.length})`));
    if (entries.length === 0) children.push(el("p", "empty", t("address_book.empty_book")));
    for (const entry of entries) children.push(entryRow(entry));
  }
  if (books.length && children.length === 0) children.push(el("p", "empty", t("address_book.not_found")));
  $("address-book-list").replaceChildren(...children);
}

// The personal address book is named in the interface language.
function bookName(book) {
  return book.personal ? t("address_book.personal_book") : book.name;
}

function entryRow(entry) {
  const row = el("div", "entry");
  const initial = (entry.name || entry.company || "#").trim().charAt(0).toUpperCase();
  const who = el("div", "who");
  who.append(el("strong", "", entryTitle(entry)), el("span", "", entryDetails(entry)));
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
  const connect = el("button", "primary", t("address_book.connect"));
  connect.addEventListener("click", () => connectEntry(entry));
  row.addEventListener("dblclick", () => connectEntry(entry));
  row.append(avatar, who, edit, connect);
  return row;
}

async function connectEntry(entry) {
  if (!entry.password) {
    // Without a saved password it is asked for in the Connection section.
    choose(entry);
    showSection("connect");
    $("remote-password").focus();
    showMessage("connect-message", t("connect.no_saved_password"));
    return;
  }
  try {
    await call("connect_entry", { id: entry.id }, "address-book-message");
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
  if (!addressBook?.account || query.length < 2) {
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
  const books = addressBook?.books ?? [];
  $("entry-book").replaceChildren(
    ...books.map((b) => {
      const option = el("option", "", b.personal ? t("address_book.personal_option", { name: bookName(b) }) : b.name);
      option.value = b.id;
      return option;
    }),
  );
  $("entry-title").textContent = t(entry ? "entry.edit" : "entry.new");
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
  try {
    await call("address_book_login", { username: $("login-user").value, password: $("login-password").value }, "address-book-message");
    $("login-password").value = "";
  } catch {
    // Message already shown.
  }
});

$("address-book-logout").addEventListener("click", () => invoke("address_book_logout"));
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

// --- administration ---

async function loadAccounts() {
  adminLoaded = true;
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
      if (account.admin) name.append(el("span", "badge", t("admin.badge_admin")));
      if (account.id === myId) name.append(el("span", "badge", t("admin.badge_you")));
      const edit = el("button", "", t("admin.edit"));
      edit.addEventListener("click", () => editAccount(account));
      row.append(name, edit);
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

  const shared = (addressBook?.books ?? []).filter((b) => !b.personal);
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
    row.append(edit, remove);
    return row;
  });
  if (books.length === 0) books.push(el("p", "empty", t("admin.no_books")));
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
    admin: $("account-admin").checked,
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
  renderMembers();
  $("book-name").focus();
}

function resetBookForm() {
  editingBook = null;
  delete $("book-members").dataset.touched;
  $("book-form").reset();
  $("book-save").textContent = t("admin.create_book");
  $("book-cancel").classList.add("hidden");
  renderMembers();
}

$("book-cancel").addEventListener("click", resetBookForm);
$("book-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const memberIds = [...$("book-members").querySelectorAll("input:checked")].map((b) => Number(b.value));
  try {
    await call("address_book_save_book", { id: editingBook?.id ?? 0, name: $("book-name").value, memberIds }, "admin-message");
    resetBookForm();
  } catch {
    // Message already shown.
  }
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") closeEntry();
});
