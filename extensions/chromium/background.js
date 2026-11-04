// clipt's browser extension: captures the whole page through the DevTools
// protocol (the same call as DevTools' "Capture full size screenshot") and
// hands the PNG to clipt, which the browser starts as a native messaging host.

const HOST = "io.github.jasondyck.clipt";

// Browsers render at most 16384 device pixels a side; a taller page is
// captured scaled down to fit rather than coming back blank.
const MAX_SIDE = 16384;

// Chrome takes messages of up to 64 MiB to a native host; leave room for
// the rest of the JSON.
const MAX_MESSAGE = 60 * 1024 * 1024;

async function capturePage(tabId) {
  const target = { tabId };
  try {
    await chrome.debugger.attach(target, "1.3");
  } catch (e) {
    throw new Error(`can't capture this page (${e.message}). Browser pages such as the New Tab page and the extension store can't be captured.`);
  }
  try {
    const send = (method, params) => chrome.debugger.sendCommand(target, method, params);
    const metrics = await send("Page.getLayoutMetrics");
    const size = metrics.cssContentSize || metrics.contentSize;
    const { result } = await send("Runtime.evaluate", { expression: "window.devicePixelRatio", returnByValue: true });
    const dpr = result.value || 1;
    const width = Math.ceil(size.width);
    const height = Math.ceil(size.height);
    const scale = Math.min(1, MAX_SIDE / (width * dpr), MAX_SIDE / (height * dpr));
    // Capturing beyond the viewport sometimes comes back as the visible part
    // repeated all the way down (a race in Chrome's compositor, more often on
    // the first capture after attaching); capture again when it does.
    const maxPeriod = Math.ceil(metrics.cssVisualViewport.clientHeight * dpr * scale) + 1;
    let data;
    let attempts = 0;
    while (attempts < 3) {
      attempts++;
      await send("Runtime.evaluate", {
        expression: "new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)))",
        awaitPromise: true,
      });
      ({ data } = await send("Page.captureScreenshot", {
        format: "png",
        captureBeyondViewport: true,
        clip: { x: 0, y: 0, width, height, scale },
      }));
      if (!(await looksTiled(data, maxPeriod))) break;
      await new Promise((r) => setTimeout(r, 200));
    }
    return { data, scaled: scale < 1, attempts };
  } finally {
    await chrome.debugger.detach(target).catch(() => {});
  }
}

// Whether the image starts with a band of most of a viewport's height
// repeated exactly below it: what a tiled capture looks like. The exact
// height isn't known in advance (the "is being debugged" bar can shrink the
// viewport after it's measured), so it's searched for, up to `maxPeriod`.
async function looksTiled(base64, maxPeriod) {
  const blob = await (await fetch(`data:image/png;base64,${base64}`)).blob();
  const bmp = await createImageBitmap(blob);
  try {
    const w = bmp.width;
    const band = Math.min(bmp.height, 2 * maxPeriod);
    const canvas = new OffscreenCanvas(w, band);
    const ctx = canvas.getContext("2d", { willReadFrequently: true });
    ctx.drawImage(bmp, 0, 0, w, band, 0, 0, w, band);
    const px = ctx.getImageData(0, 0, w, band).data;
    const row = w * 4;
    const same = (a, b) => {
      for (let i = 0; i < row; i++) if (px[a * row + i] !== px[b * row + i]) return false;
      return true;
    };
    // Flat areas repeat trivially: a tile has to hold more than one kind of
    // row, so the first row unlike the top one must fall inside it.
    let firstDiff = 1;
    while (firstDiff < band && same(0, firstDiff)) firstDiff++;
    // The bar takes a few dozen pixels, so the tile is most of the viewport.
    for (let p = Math.floor(maxPeriod / 2); p <= maxPeriod && 2 * p <= band; p++) {
      if (firstDiff >= p || !same(0, p)) continue;
      let all = true;
      for (let i = 0; i < 16 && all; i++) {
        const y = Math.floor(((i + 0.5) * p) / 16);
        all = same(y, y + p);
      }
      for (let y = 0; y < p && all; y++) all = same(y, y + p);
      if (all) return true;
    }
    return false;
  } finally {
    bmp.close();
  }
}

async function run(action, tab) {
  if (!tab) {
    [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  }
  const { data, scaled, attempts } = await capturePage(tab.id);
  if (data.length > MAX_MESSAGE) {
    throw new Error("this page is too large to hand to clipt in one piece");
  }
  let reply;
  try {
    reply = await chrome.runtime.sendNativeMessage(HOST, { action, png: data, title: tab.title || "", url: tab.url || "" });
  } catch (e) {
    if (/not found/i.test(e.message)) {
      throw new Error("clipt isn't set up for this browser yet: run `clipt browser-extension` once, then try again.");
    }
    throw new Error(`clipt didn't answer: ${e.message}`);
  }
  if (!reply || !reply.ok) {
    throw new Error(reply && reply.error ? reply.error : "clipt failed without saying why");
  }
  return { ...reply, scaled, attempts };
}

// From the popup.
chrome.runtime.onMessage.addListener((msg, _sender, respond) => {
  if (msg.type !== "capture") return false;
  run(msg.action)
    .then((r) => respond(r))
    .catch((e) => respond({ ok: false, error: e.message }));
  return true; // respond asynchronously
});

// From the keyboard shortcuts: the badge says how it went.
chrome.commands.onCommand.addListener(async (command, tab) => {
  const action = command === "save-full-page" ? "save" : "copy";
  try {
    await run(action, tab);
    flash("✓", "#16a34a");
  } catch (e) {
    console.error("clipt:", e.message);
    flash("!", "#dc2626");
  }
});

function flash(text, color) {
  chrome.action.setBadgeBackgroundColor({ color });
  chrome.action.setBadgeText({ text });
  setTimeout(() => chrome.action.setBadgeText({ text: "" }), 2500);
}
