// Relays between the login page (cs.js) and the jet-cli process listening on localhost.
// The files are static; the per-run address of jet-cli arrives via the local start page.

let cfg = null; // { base: "http://127.0.0.1:<port>/<nonce>" }

async function config() {
  if (!cfg) cfg = (await chrome.storage.session.get('jet')).jet ?? null;
  return cfg;
}

async function call(path, body) {
  const c = await config();
  if (!c) throw new Error('jet-cli address unknown');
  const init = body === undefined
    ? {}
    : { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) };
  const res = await fetch(c.base + path, init);
  return res.json();
}

chrome.runtime.onMessage.addListener((msg, _sender, reply) => {
  let p;
  if (msg.type === 'init') {
    cfg = { base: msg.base };
    p = chrome.storage.session.set({ jet: cfg }).then(() => call('/event', { kind: 'log', detail: 'extension ready' }));
  } else if (msg.type === 'event') p = call('/event', { kind: msg.kind, detail: msg.detail ?? null });
  else if (msg.type === 'config') p = call('/config');
  else if (msg.type === 'otp') p = call('/otp');
  else p = Promise.resolve(null);
  p.then(reply, () => reply(null));
  return true;
});

// The auth server finishes by redirecting to the site's /signin-oidc?code=...; hand that URL to
// jet-cli and show a local "done" page instead of letting the site consume it.
let handled = false;
chrome.webRequest.onBeforeRequest.addListener(
  (details) => {
    if (handled) return;
    handled = true;
    call('/callback', { url: details.url }).finally(async () => {
      const c = await config();
      if (c) chrome.tabs.update(details.tabId, { url: c.base + '/done' });
    });
  },
  { urls: ['https://*/signin-oidc*'], types: ['main_frame'] },
);

// Report the login API calls' status so jet-cli can explain failures.
chrome.webRequest.onCompleted.addListener(
  (d) => {
    call('/event', { kind: 'api', detail: `${new URL(d.url).pathname} ${d.statusCode}` }).catch(() => {});
  },
  { urls: ['https://*/applications/authenticationservice/*'] },
);
