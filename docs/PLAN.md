# jet-cli plan

Rust CLI for ordering on Just Eat Takeaway (NL / thuisbezorgd.nl first; tenant kept configurable).

## Decisions

- **Language:** Rust (clap, reqwest + rustls, tokio, serde, dialoguer for interactive pickers).
- **UX:** subcommands with `--json` output; interactive prompts when arguments are missing.
- **Login:** both flows.
  - Default: `jet login` asks for the email and code in the terminal; the login page runs in the OS web
    view (wry: WKWebView / WebView2 / WebKitGTK) inside the `jet-webview` helper binary, hidden unless the
    security check needs a visible page (then shown briefly and hidden again). Fallbacks: Chromium + helper
    extension, then `--manual`. The extension fills in the email, waits for Turnstile
    (passes silently in practice), submits, fills in the code you type in the terminal, and hands the
    `/signin-oidc?code=` redirect back to the CLI. The browser profile persists between logins.
  - `jet login --manual`: open the authorize URL in the default browser; the user pastes the final redirect URL.
  - Tokens are stored in `~/.config/jet-cli/` (0600) and refreshed silently.
- **Payment (later):** iDEAL (print link + terminal QR), saved card / PayPal (one-click if the API allows),
  gift card / credit.
- **Scope now:** read-only. No checkout or order placement until explicitly approved; any order command will
  always require explicit confirmation showing the total.

## Milestones

1. ✅ Scaffold, config (tenant, delivery address), HTTP client with JET headers.
2. ✅ Auth: PKCE, `--manual` flow, token storage and refresh, `jet whoami`. (Awaiting first real login.)
3. ✅ Auth: helper-extension window flow. (Verified up to the code step with a dummy address.)
4. ✅ `jet restaurants` (discovery, filters: open now, name/cuisine, sort).
5. ✅ `jet menu` (categories, items, prices, sold-out state).
6. ✅ `jet add` / `jet basket [rm|qty|clear]` (server-side basket, sizes and options, interactive or flags).
7. ✅ `jet orders` (history) and `jet reorder` (rebuilt from today's menu; JET's own reorder endpoint is
   gated by `canReorder`).
8. ✅ Private GitHub repo + Actions: cross-platform builds, releases on `v*` tags.
9. *(Later, gated)* checkout + payment.
