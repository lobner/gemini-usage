// Gemini Usage bridge: once a minute, reads the Gemini app's usage windows
// with the browser's own Google login, and hands the numbers to the Gemini
// Usage menu-bar app on 127.0.0.1.
//
// It sends only the windows (pool type, fraction used, reset time) and the
// plan tier. No cookie, token or page content leaves the browser.

import {
  ASSET_BASE,
  ORIGIN,
  findLoadingBundle,
  findQueryId,
  parseUsageResponse,
  queryUrl,
} from "./gemini.js";

// Keep in step with WEB_PORT in the app's src/web.rs.
const APP_URL = "http://127.0.0.1:47614/gemini-web-usage";
const ALARM = "poll";
const POLL_MINUTES = 1;
// Look for a new query id at least this often, even when the old one works.
const DISCOVER_MS = 6 * 60 * 60 * 1000;

chrome.runtime.onInstalled.addListener(start);
chrome.runtime.onStartup.addListener(start);
chrome.alarms.onAlarm.addListener((a) => {
  if (a.name === ALARM) {
    poll();
  }
});
chrome.runtime.onMessage.addListener((msg, _sender, reply) => {
  if (msg === "poll") {
    poll().then(() => reply(true));
    return true; // reply later
  }
  return false;
});

function start() {
  chrome.alarms.create(ALARM, { periodInMinutes: POLL_MINUTES, delayInMinutes: 0 });
  poll();
}

let running = null;

/** Runs one poll, or joins the one that is already running. */
function poll() {
  running ??= pollOnce().finally(() => {
    running = null;
  });
  return running;
}

async function pollOnce() {
  const at = new Date().toISOString();
  let report;
  try {
    const usage = await readUsage();
    report = { fetchedAt: at, tier: usage.tier, pools: usage.pools };
  } catch (e) {
    report = { fetchedAt: at, error: String(e?.message ?? e) };
  }
  const delivered = await deliver(report);
  await chrome.storage.local.set({ last: { ...report, delivered } });
}

/** Reads the usage windows, and finds a new query id once if the old one fails. */
async function readUsage() {
  let found = await queryId(false);
  try {
    return await runQuery(found.id);
  } catch (e) {
    if (e.signedOut) {
      throw e;
    }
    const fresh = await queryId(true);
    if (fresh.id === found.id) {
      throw e;
    }
    found = fresh;
    return await runQuery(found.id);
  }
}

async function runQuery(id) {
  const resp = await fetch(queryUrl(id), {
    credentials: "include",
    headers: { Accept: "application/json" },
  });
  // Signed out, Google answers 401/403 or sends the request to its sign-in
  // page.
  if (resp.status === 401 || resp.status === 403 || new URL(resp.url).host !== new URL(ORIGIN).host) {
    const e = new Error(`not signed in to gemini.google.com (HTTP ${resp.status})`);
    e.signedOut = true;
    throw e;
  }
  if (!resp.ok) {
    throw new Error(`usage query HTTP ${resp.status}`);
  }
  return parseUsageResponse(await resp.text());
}

/** Returns {id, bundle}: the cached query id, or a newly found one. */
async function queryId(force) {
  const { query } = await chrome.storage.local.get("query");
  if (!force && query?.id && Date.now() - query.foundAt < DISCOVER_MS) {
    return query;
  }
  const html = await text(`${ORIGIN}/app`);
  const bundle = findLoadingBundle(html);
  if (!bundle) {
    throw new Error("cannot find the usage page bundle on gemini.google.com");
  }
  if (!force && query?.bundle === bundle && query.id) {
    const same = { ...query, foundAt: Date.now() };
    await chrome.storage.local.set({ query: same });
    return same;
  }
  const id = findQueryId(await text(ASSET_BASE + bundle));
  if (!id) {
    throw new Error(`cannot find UsagePageQuery in ${bundle}`);
  }
  const found = { id, bundle, foundAt: Date.now() };
  await chrome.storage.local.set({ query: found });
  return found;
}

async function text(url) {
  const resp = await fetch(url, { credentials: "include" });
  if (!resp.ok) {
    throw new Error(`${url} HTTP ${resp.status}`);
  }
  return resp.text();
}

/** Posts the report to the app. Returns false when the app is not running. */
async function deliver(report) {
  try {
    const resp = await fetch(APP_URL, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(report),
    });
    return resp.ok;
  } catch {
    return false;
  }
}
