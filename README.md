# jet

Order food from Just Eat Takeaway ([thuisbezorgd.nl](https://www.thuisbezorgd.nl)) from your terminal.

Unofficial; it talks to the same APIs as the website, so things may break when they change.

Download a build for Linux, macOS or Windows from the [releases](../../releases) and put both
`jet` and `jet-webview` somewhere on your PATH (on macOS, first run
`xattr -dr com.apple.quarantine <unpacked folder>`; the binaries aren't signed). Or build it:

```sh
cargo install --git ssh://git@github.com/unxsist/jet-cli.git   # installs jet and jet-webview

jet address "Damrak 1, Amsterdam"     # where to deliver
jet restaurants pizza --sort rating   # what's open nearby
jet menu 1                            # menu of the first restaurant listed (or a name/slug)
jet add -r lezzet "tavuk kanadi" -o rijst -o knoflook -q 2
jet basket                            # show; also: rm <n>, qty <n> <q>, clear
jet login                             # email + code
jet orders                            # your recent orders
jet reorder 2                         # put order 2 back in the basket
jet checkout                          # address, time, total, payment methods
jet order --pay ideal                 # confirm, then pay via the link / QR code
jet order status                      # paid and placed yet?
```

`jet order` asks for confirmation and prints a payment link (iDEAL, card or PayPal) plus a QR code
for your phone. Nothing is ordered until you've paid. Name and phone number for the order come from
your last order (or `--name`/`--phone`); `--time 19:30` schedules it, `--note` adds a courier note.

Anything you leave out (restaurant, item, size, options) is asked for interactively. Every command
accepts `--json` for scripting.

## Login

`jet login` asks for your email and the emailed code in the terminal. The login page itself runs
in your OS's built-in web view (WKWebView on macOS, WebView2 on Windows, WebKitGTK on Linux) via the
small `jet-webview` helper that ships next to `jet`, so no browser needs to be installed. The window
stays hidden; it only appears for a moment if the site's security check wants a visible page, and
hides itself again. Tokens are refreshed automatically after that, so logging in is rare.

- `jet login --show-browser` keeps the window visible; `jet login --manual` uses your normal browser
  (log in, then paste the URL you land on).
- Linux needs WebKitGTK 4.1 (`webkit2gtk-4.1` on Arch, `libwebkit2gtk-4.1-0` on Debian/Ubuntu). Without
  it, `jet login` falls back to Chromium/Brave if installed, otherwise to `--manual`.
- `JET_DEBUG=1 jet login` prints what the login page does.

Files: `~/.config/jet-cli/` (config, tokens, current basket, mode 0600) and
`~/.local/share/jet-cli/` (login window data). On macOS these live under
`~/Library/Application Support/jet-cli/`.

See [docs/RESEARCH.md](docs/RESEARCH.md) for the API notes.

## Use with AI agents

`jet mcp` runs an [MCP](https://modelcontextprotocol.io) server on stdio with tools for login,
delivery address, restaurant search, menus, basket, orders and reorder. Register it with your agent,
e.g. Claude Code:

```sh
claude mcp add jet -- jet mcp
```

or in any MCP client config:

```json
{ "mcpServers": { "jet": { "command": "jet", "args": ["mcp"] } } }
```

When the agent needs you to log in, it asks for your email, triggers the code email and asks you for
the code.

### Agents in a cloud sandbox (no screen)

JET's login page needs a real browser, so in a sandbox (Daytona, CI, a server) there are two ways:

- **Log in on your own device.** `login_start` / `jet login start` (or `jet login link`) returns a
  link. Open it on your phone or computer and log in; you end on a plain text page. Copy that page's
  address and give it to the agent (`login_complete` / `jet login code '<address>'`). The address is
  safe to share: it only works together with a secret that never leaves the sandbox.
- `jet doctor` (or `--json`) shows which login method works on a machine and why, e.g. a missing
  WebKitGTK library; without a usable window `jet login`, `login start` and `login_start` hand out
  the link automatically, and an unattended login that needs a person for the security check
  switches to the link after ~20 s.
- **Bring an existing login.** Run `jet auth export` where you're logged in and store the output as
  the sandbox's `JET_SESSION` secret (or run `jet auth import <string>` there). Treat it like a
  password; `jet logout` on the sandbox doesn't revoke it. Agents that use the shell instead can run every command with `--json` and use
`jet login start --email …` / `jet login code <code>`; errors come back as JSON with exit code 2
(log in first) or 3 (needs more input).

Ordering: `checkout_preview` shows the total; `place_order` needs the payment method and the exact
total the user confirmed (`--confirm-total` on the CLI), and returns a payment link for the user to
pay. The order only goes to the restaurant once the user has paid; `order_status` tells.

## Releases

CI builds Linux (x86_64, arm64), macOS (Apple Silicon, Intel) and Windows (x86_64) on every push.
Pushing a `v*` tag attaches the binaries (and `SHA256SUMS`) to a GitHub release:

```sh
git tag v0.1.0 && git push origin v0.1.0
```
