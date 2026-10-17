// Runs in JET's hosted login page inside the jet-webview window. Fills in the email once the
// security check has produced a token, and the code the user types in the terminal. The user
// handles any security challenge themselves; we only wait for it.
// jet prepends `const JET = { email, authHost };`.
(() => {
  if (window.top !== window || location.hostname !== JET.authHost) return;

  const send = (kind, detail) => window.ipc.postMessage(JSON.stringify({ kind, detail: detail ?? null }));
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const started = Date.now();

  // jet delivers codes with: window.__jetCode = { seq, code }
  window.__jetCode = window.__jetCode || { seq: 0, code: '' };

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

  // The cookie banner covers the security check widget; decline non-essential cookies.
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

  const turnstileToken = () => document.querySelector('input[name=cf-turnstile-response]')?.value;

  let lastPath = null;
  let emailSubmitted = false;
  let emailSubmittedAt = 0;
  let reportedWaiting = false;
  let reportedEmailError = false;
  let askedOtp = false;
  let otpSeq = 0;
  let otpSubmittedAt = 0;

  async function tick() {
    if (!document.body) return;
    dismissCookies();

    if (location.pathname !== lastPath) {
      lastPath = location.pathname;
      send('page', location.href);
    }

    const email = document.querySelector('input[name=email]');
    if (email && JET.email) {
      if (!emailSubmitted) {
        if (email.value !== JET.email) setValue(email, JET.email);
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
      } else if (!reportedEmailError && Date.now() - emailSubmittedAt > 6000) {
        // Still on the email form well after submitting: something went wrong.
        reportedEmailError = true;
        send('email_error', visibleErrors() || 'the login page did not accept the email');
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
      const c = window.__jetCode;
      if (c.seq > otpSeq) {
        otpSeq = c.seq;
        setValue(otp, c.code);
        await sleep(150);
        submit(otp.form);
        otpSubmittedAt = Date.now();
        send('otp_submitted');
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
      await sleep(300);
    }
  })();
})();
