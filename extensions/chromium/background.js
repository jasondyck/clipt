// clipt's browser extension: captures the whole page and hands the PNG to
// clipt, which the browser starts as a native messaging host.
//
// Two ways to capture. The default scrolls the page a screenful at a time,
// captures each with captureVisibleTab and stitches them: no browser UI
// shows. "Exact" renders the whole page in one go through the DevTools
// protocol (as DevTools' "Capture full size screenshot" does), which is
// truer to the page but makes the browser show its "started debugging this
// browser" bar for a few seconds.

const HOST = "io.github.jasondyck.clipt";

// Browsers render at most 16384 device pixels a side; a taller page is
// captured scaled down to fit rather than coming back blank.
const MAX_SIDE = 16384;

// Chrome takes messages of up to 64 MiB to a native host; leave room for
// the rest of the JSON.
const MAX_MESSAGE = 60 * 1024 * 1024;

// ---- exact: one render through the DevTools protocol ----------------------

async function captureExact(tabId) {
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
    for (let attempt = 0; attempt < 3; attempt++) {
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
    return { data, scaled: scale < 1 };
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

// ---- scroll and stitch ---------------------------------------------------

// captureVisibleTab is allowed twice a second.
const CAPTURE_GAP_MS = 550;

// A page that keeps growing as it's scrolled (infinite feeds) stops here.
const MAX_TILES = 80;

// These run in the page.

function pagePrepare() {
  const style = document.createElement("style");
  style.id = "__clipt_capture";
  // No smooth scrolling, and no scrollbars in the picture.
  style.textContent =
    "html{scroll-behavior:auto!important;scrollbar-width:none!important}" +
    "html::-webkit-scrollbar,body::-webkit-scrollbar{display:none!important}";
  document.documentElement.appendChild(style);
  window.__clipt = { x: window.scrollX, y: window.scrollY, hidden: [] };
  const se = document.scrollingElement || document.documentElement;
  return {
    width: window.innerWidth,
    viewport: window.innerHeight,
    height: Math.max(se.scrollHeight, document.body ? document.body.scrollHeight : 0, window.innerHeight),
  };
}

async function pageScrollTo(y) {
  window.scrollTo(0, y);
  // Let it paint, and lazy images that just came into view start loading.
  await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
  await new Promise((r) => setTimeout(r, 120));
  return window.scrollY;
}

// Fixed and sticky things (headers, cookie bars, chat buttons) would show
// in every screenful: after the first, hide them.
function pageHideFixed() {
  for (const el of document.querySelectorAll("body *")) {
    const pos = getComputedStyle(el).position;
    if ((pos === "fixed" || pos === "sticky") && el.style.visibility !== "hidden") {
      window.__clipt.hidden.push([el, el.style.getPropertyValue("visibility"), el.style.getPropertyPriority("visibility")]);
      el.style.setProperty("visibility", "hidden", "important");
    }
  }
}

function pageRestore() {
  const c = window.__clipt;
  if (!c) return;
  for (const [el, value, priority] of c.hidden) {
    if (value) el.style.setProperty("visibility", value, priority);
    else el.style.removeProperty("visibility");
  }
  document.getElementById("__clipt_capture")?.remove();
  window.scrollTo(c.x, c.y);
  delete window.__clipt;
}

async function captureStitched(tab) {
  const inPage = async (func, ...args) => {
    const [r] = await chrome.scripting.executeScript({ target: { tabId: tab.id }, func, args });
    return r.result;
  };
  let geo;
  try {
    geo = await inPage(pagePrepare);
  } catch (e) {
    throw new Error(`can't capture this page (${e.message}). Browser pages such as the New Tab page and the extension store can't be captured.`);
  }
  const tiles = [];
  try {
    let last = 0;
    for (let y = 0; tiles.length < MAX_TILES; y += geo.viewport) {
      const at = await inPage(pageScrollTo, Math.min(y, geo.height - geo.viewport));
      const wait = last + CAPTURE_GAP_MS - Date.now();
      if (wait > 0) await new Promise((r) => setTimeout(r, wait));
      const url = await chrome.tabs.captureVisibleTab(tab.windowId, { format: "png" });
      last = Date.now();
      tiles.push({ y: at, bmp: await createImageBitmap(await (await fetch(url)).blob()) });
      // Long pages take a while (two screenfuls a second): count them on the
      // toolbar icon.
      chrome.action.setBadgeBackgroundColor({ color: "#6b7280" });
      chrome.action.setBadgeText({ text: String(tiles.length) });
      if (tiles.length === 1) await inPage(pageHideFixed);
      // The bottom, or a page that won't scroll any further.
      if (at + geo.viewport >= geo.height || (tiles.length > 1 && at <= tiles[tiles.length - 2].y)) break;
    }
  } finally {
    chrome.action.setBadgeText({ text: "" });
    await inPage(pageRestore).catch(() => {});
  }
  try {
    // Device pixels per CSS pixel, from what the browser actually captured.
    const ratio = tiles[0].bmp.width / geo.width;
    const scale = Math.min(1, MAX_SIDE / (geo.width * ratio), MAX_SIDE / (geo.height * ratio));
    const k = ratio * scale;
    const canvas = new OffscreenCanvas(Math.round(geo.width * k), Math.round(geo.height * k));
    const ctx = canvas.getContext("2d");
    for (const t of tiles) {
      ctx.drawImage(t.bmp, 0, Math.round(t.y * k), t.bmp.width * scale, t.bmp.height * scale);
    }
    const png = await canvas.convertToBlob({ type: "image/png" });
    return { data: toBase64(new Uint8Array(await png.arrayBuffer())), scaled: scale < 1 };
  } finally {
    tiles.forEach((t) => t.bmp.close());
  }
}

function toBase64(bytes) {
  let s = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    s += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
  }
  return btoa(s);
}

// ---- sending to clipt ------------------------------------------------------

async function run(action, tab) {
  if (!tab) {
    [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  }
  const exact = (await chrome.storage.local.get("exact")).exact === true;
  const { data, scaled } = exact ? await captureExact(tab.id) : await captureStitched(tab);
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
  return { ...reply, scaled };
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
