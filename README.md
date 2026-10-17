# jet

Order food from Just Eat Takeaway ([thuisbezorgd.nl](https://www.thuisbezorgd.nl)) from your terminal.

Unofficial; it talks to the same APIs as the website, so things may break when they change.

Download a binary for Linux, macOS or Windows from the
[releases](../../releases), or build it yourself:

```sh
cargo install --path .

jet address "Damrak 1, Amsterdam"     # where to deliver
jet restaurants pizza --sort rating   # what's open nearby
jet menu 1                            # menu of the first restaurant listed (or a name/slug)
jet add -r lezzet "tavuk kanadi" -o rijst -o knoflook -q 2
jet basket                            # show; also: rm <n>, qty <n> <q>, clear
jet login                             # email + code
jet orders                            # your recent orders
jet reorder 2                         # put order 2 back in the basket
```

Anything you leave out (restaurant, item, size, options) is asked for interactively. Every command
accepts `--json` for scripting.

## Login

`jet login` asks for your email and the emailed code in the terminal. Behind the scenes it runs a
headless Chromium with a small helper extension that fills in JET's login form. If the site's
security check won't pass in a hidden browser, a small window opens instead (it fills itself in;
you still type the code in the terminal) and closes when you're logged in. `--show-browser` always
shows it. Tokens are refreshed automatically after that, so this is rare.

The login browser runs with its own profile and ignores your `chromium-flags.conf`.

- Needs Chromium or Brave (Google Chrome and Edge no longer allow loading the helper). Set
  `browser = "/path/to/browser"` in `~/.config/jet-cli/config.toml` to choose one.
- `jet login --manual` uses your normal browser instead: log in, then paste the URL you land on.
- `JET_DEBUG=1 jet login` prints what the helper sees.

Files: `~/.config/jet-cli/` (config, tokens, current basket, mode 0600) and
`~/.local/share/jet-cli/` (login window profile).

See [docs/RESEARCH.md](docs/RESEARCH.md) for the API notes.

## Releases

CI builds Linux (x86_64, arm64), macOS (Apple Silicon, Intel) and Windows (x86_64) on every push.
Pushing a `v*` tag attaches the binaries (and `SHA256SUMS`) to a GitHub release:

```sh
git tag v0.1.0 && git push origin v0.1.0
```
