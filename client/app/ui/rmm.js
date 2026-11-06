// Light RMM (NuviraHub Pro, feature "pro.inventory"): the inventory and health
// of an address book computer, and "Wake up" while it is off. Uses $, invoke, t,
// msg and formatId from main.js; address-book.js opens it.

function gb(bytes) {
  return `${(bytes / 1073741824).toFixed(bytes < 10737418240 ? 1 : 0)} GB`;
}

function inventoryRow(table, label, value) {
  if (!value) return;
  const tr = document.createElement("tr");
  const th = document.createElement("th");
  th.textContent = label;
  const td = document.createElement("td");
  td.textContent = value;
  tr.append(th, td);
  table.append(tr);
}

async function openInventory(entry) {
  $("inventory-title").textContent = entry.name || formatId(entry.device_id);
  $("inventory-health").textContent = "";
  $("inventory-table").replaceChildren();
  $("inventory-message").classList.add("hidden");
  $("inventory-wake").classList.add("hidden");
  $("inventory").classList.remove("hidden");
  $("inventory-wake").onclick = async () => {
    try {
      await invoke("wake_device", { deviceId: entry.device_id });
      $("inventory-message").textContent = t("rmm.wake_sent");
      $("inventory-message").classList.add("ok");
    } catch (err) {
      $("inventory-message").textContent = msg(String(err));
      $("inventory-message").classList.remove("ok");
    }
    $("inventory-message").classList.remove("hidden");
  };
  let d;
  try {
    d = await invoke("device_inventory", { deviceId: entry.device_id });
  } catch (err) {
    $("inventory-message").textContent = msg(String(err));
    $("inventory-message").classList.remove("hidden");
    return;
  }
  $("inventory-wake").classList.toggle("hidden", d.online);
  const reasons = d.health_reasons.map((r) => msg(r)).join(" · ");
  $("inventory-health").className = `inventory-health ${d.health}`;
  $("inventory-health").textContent = t(`rmm.health_${d.health}`) + (reasons ? ` — ${reasons}` : "");
  const table = $("inventory-table");
  if (!d.updated) {
    inventoryRow(table, "", t("rmm.no_inventory"));
    return;
  }
  inventoryRow(table, t("rmm.os"), d.os);
  inventoryRow(table, t("rmm.user"), d.user);
  inventoryRow(table, t("rmm.cpu"), d.cores ? `${d.cpu} · ${d.cores} core` : d.cpu);
  if (d.memory_total) {
    inventoryRow(table, t("rmm.memory"), t("rmm.free_of", { free: gb(d.memory_available), total: gb(d.memory_total) }));
  }
  if (d.uptime_s) {
    inventoryRow(table, t("rmm.uptime"), t("rmm.uptime_value", { d: Math.floor(d.uptime_s / 86400), h: Math.floor((d.uptime_s % 86400) / 3600) }));
  }
  for (const [mount, total, available] of d.disks) {
    inventoryRow(table, `${t("rmm.disk")} ${mount}`, t("rmm.free_of", { free: gb(available), total: gb(total) }));
  }
  for (const [name, mac, addresses] of d.interfaces) {
    inventoryRow(table, `${t("rmm.network")} ${name}`, [mac, ...addresses].filter(Boolean).join(" · "));
  }
  inventoryRow(table, "NuviraHub", d.app_version);
  inventoryRow(table, t("rmm.updated"), new Date(d.updated * 1000).toLocaleString(document.documentElement.lang));
}

$("inventory-close").addEventListener("click", () => $("inventory").classList.add("hidden"));
