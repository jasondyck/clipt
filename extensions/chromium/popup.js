const status = document.getElementById("status");
const buttons = [...document.querySelectorAll("button, input")];

function show(text, error) {
  status.textContent = text;
  status.className = error ? "error" : "";
}

async function capture(action) {
  buttons.forEach((b) => (b.disabled = true));
  show("Capturing…");
  const r = await chrome.runtime.sendMessage({ type: "capture", action });
  buttons.forEach((b) => (b.disabled = false));
  if (!r.ok) {
    show(r.error, true);
    return;
  }
  const note = r.scaled ? " (scaled down: the page is very tall)" : "";
  show((action === "save" ? `Saved to ${r.path}` : "Copied") + note);
  if (action === "copy" && !r.scaled) setTimeout(() => window.close(), 700);
}

document.getElementById("copy").addEventListener("click", () => capture("copy"));
document.getElementById("save").addEventListener("click", () => capture("save"));

chrome.commands.getAll((cmds) => {
  const c = cmds.find((c) => c.name === "copy-full-page");
  if (c && c.shortcut) document.getElementById("copy-key").textContent = c.shortcut;
});

const exact = document.getElementById("exact");
chrome.storage.local.get("exact").then((s) => (exact.checked = s.exact === true));
exact.addEventListener("change", () => chrome.storage.local.set({ exact: exact.checked }));
