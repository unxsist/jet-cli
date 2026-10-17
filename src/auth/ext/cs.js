// Drives JET's hosted login page: fills the email once Turnstile has produced a token, then
// fills the emailed code that the user types in the terminal. The user handles any Turnstile
// challenge themselves; we only wait for it.
(() => {
  // Local start page served by jet-cli: tell the extension where jet-cli listens, then go on to
  // the real login page.
  if (location.hostname === '127.0.0.1') {
    const m = location.pathname.match(/^\/([\w-]+)\/start$/);
    if (m) {
      chrome.runtime
        .sendMessage({ type: 'init', base: `${location.origin}/${m[1]}` })
        .then(() => location.replace(document.body.dataset.authUrl));
    }
    return;
  }

  const send = (kind, detail) => chrome.runtime.sendMessage({ type: 'event', kind, detail });
  const rpc = (type) => chrome.runtime.sendMessage({ type });
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const started = Date.now();

  // React tracks input values itself; go through the native setter and fire input events so
  // its onChange sees the new value.
  const setValue = (input, value) => {
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
    setter.call(input, value);
    input.dispatchEvent(new Event('input', { bubbles: true }));
    input.dispatchEvent(new Event('change', { bubbles: true }));
  };

  // Fires the form's submit event, which is what the page's React handler listens to.
  const submit = (form) => form.requestSubmit();

  // The cookie banner covers the Turnstile widget; decline non-essential cookies.
  const cookieButton = /^(alleen noodzakelijk|only necessary|necessary only|accept necessary|nur notwendige|nur erforderliche)/i;
  const dismissCookies = () => {
    for (const b of document.querySelectorAll('button')) {
      if (cookieButton.test(b.textContent.trim())) b.click();
    }
  };

  const visibleErrors = () => {
    const nodes = document.querySelectorAll('[role=alert], [aria-live=assertive], [data-qa*=error], [class*=error]');
    const texts = [...nodes].map((n) => n.textContent.trim()).filter(Boolean);
    return [...new Set(texts)].join(' / ');
  };

  const pageText = () => (document.querySelector('main') || document.body).innerText.replace(/\s+/g, ' ').trim().slice(0, 300);

  const turnstileToken = () => document.querySelector('input[name=cf-turnstile-response]')?.value;

  let cfg = null;
  let lastPath = null;
  let emailSubmitted = false;
  let emailSubmittedAt = 0;
  let reportedWaiting = false;
  let reportedEmailError = false;
  let askedOtp = false;
  let otpSeq = 0;
  let otpSubmittedAt = 0;

  async function tick() {
    if (!cfg) cfg = await rpc('config');
    if (!cfg) return;
    dismissCookies();

    if (location.pathname !== lastPath) {
      lastPath = location.pathname;
      send('page', lastPath);
    }

    const email = document.querySelector('input[name=email]');
    if (email && cfg.email) {
      if (!emailSubmitted) {
        if (email.value !== cfg.email) setValue(email, cfg.email);
        if (turnstileToken()) {
          emailSubmitted = true;
          emailSubmittedAt = Date.now();
          await sleep(150);
          submit(email.form);
          send('email_submitted');
        } else if (!reportedWaiting && Date.now() - started > 2500) {
          reportedWaiting = true;
          send('turnstile_wait');
        }
      } else if (!reportedEmailError && Date.now() - emailSubmittedAt > 5000) {
        // Still on the email form well after submitting: something went wrong.
        reportedEmailError = true;
        send('email_error', visibleErrors() || pageText() || 'the login page did not accept the email');
      }
    }

    const otp = document.querySelector('input[name=otpCode]');
    if (otp) {
      if (!askedOtp) {
        askedOtp = true;
        send('otp_needed');
      }
      // Only report a rejected code when the page shows an error; a correct code just takes a
      // moment to redirect.
      const error = otpSubmittedAt && Date.now() - otpSubmittedAt > 1500 ? visibleErrors() : '';
      if (error) {
        otpSubmittedAt = 0;
        send('otp_error', error);
      }
      const r = await rpc('otp');
      if (r && r.seq > otpSeq) {
        otpSeq = r.seq;
        setValue(otp, r.code);
        await sleep(150);
        submit(otp.form);
        otpSubmittedAt = Date.now();
      }
    }
  }

  (async () => {
    for (;;) {
      try {
        await tick();
      } catch (e) {
        send('log', String(e));
      }
      await sleep(400);
    }
  })();
})();
