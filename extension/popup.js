// Shows the bridge's last reading and whether the app received it.

const NAMES = { GXU_FIVE_HOURLY: "Current (5 h)", GXU_WEEKLY: "Weekly" };

const out = document.getElementById("out");
const button = document.getElementById("refresh");

function row(label, value, cls) {
  const dt = document.createElement("dt");
  dt.textContent = label;
  const dd = document.createElement("dd");
  dd.textContent = value;
  if (cls) {
    dd.className = cls;
  }
  out.append(dt, dd);
}

function time(iso) {
  const d = new Date(iso);
  return Number.isNaN(d.getTime())
    ? "?"
    : d.toLocaleString(undefined, { weekday: "short", hour: "2-digit", minute: "2-digit" });
}

async function render() {
  const { last } = await chrome.storage.local.get("last");
  out.replaceChildren();
  if (!last) {
    row("Status", "no reading yet");
    return;
  }
  if (last.error) {
    row("Error", last.error, "error");
  }
  for (const p of last.pools ?? []) {
    const name = NAMES[p.poolType];
    if (name && typeof p.percentageUsed === "number") {
      row(name, `${Math.round(p.percentageUsed * 100)}% used, resets ${time(p.windowRefreshesAt)}`);
    }
  }
  row("Read at", time(last.fetchedAt));
  row(
    "Menu-bar app",
    last.delivered ? "received it" : "not reached - is Gemini Usage running?",
    last.delivered ? "" : "error",
  );
}

button.addEventListener("click", async () => {
  button.disabled = true;
  await chrome.runtime.sendMessage("poll");
  await render();
  button.disabled = false;
});

render();
