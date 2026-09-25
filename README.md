# Conduit

A small Rust host app that lets an **approved** website do things the browser
sandbox forbids: keep real files in a per-site sandbox (far bigger than cookies
or localStorage), read and write a specific host folder the user picks, launch
applications, read hardware info and live stats, control media/power/clipboard,
list and end processes, and request elevation — each ability gated by a
permission the user grants with a click.

Grants can be scoped in time (this session, an hour, a day, or always) and are
revocable at any moment, even on an open connection. A page can subscribe to
live file-change events over the WebSocket, and a site's sandbox can be exported
to and restored from a `.zip`.

Native GUIs on every OS (WinUI 3 on Windows, GTK 4 on Linux, SwiftUI on macOS),
each with 20-language i18n and an animated, Deny-by-default consent prompt, plus
a TurboWarp extension. Speaks WebSocket **and** plain HTTP JSON.

```bash
cargo build --release
./target/release/conduit             # GUI + tray, listens on 127.0.0.1:8765
./target/release/conduit --minimized # start in the tray (idle ≈ 14 MB)
./target/release/conduit --headless  # no GUI; console consent prompts
```

Release exe is a single ~3.6 MB file. Idle footprint is ~14 MB (tray + server);
the dashboard's WebView2 is created only when you open a window and freed when
you close it, so an idle Conduit stays light.

* test page: <http://127.0.0.1:8765/test>
* extension harness: <http://127.0.0.1:8765/test/extension>
* extension URL for TurboWarp: `http://127.0.0.1:8765/turbowarp/extension.js`

## GUI

* **Overview** — elevation state, live CPU (per-core), memory, network and
  battery, refreshed every 1.5 s.
* **Sites** — every paired origin, its permissions (editable), remembered
  launch targets, "open files" (its sandbox folder), and revoke.
* **Activity** — the last 200 method calls with result codes.
* **Extension** — the TurboWarp URL with copy / open buttons.
* **Settings** — language (21), theme (system/light/dark), close-to-tray,
  start with Windows, and handle `conduit://` links.
* **Consent pop-up** — a small always-on-top window. Pairing shows a
  per-permission checklist the user can trim; launch/kill/power/clipboard/
  elevate each show what is being asked and a "deny by default" countdown.
  The GUI reaches the core over WebView IPC only — it exposes **no** HTTP
  surface a website could reach.

Elevation: the GUI (or a page with the `system` permission) can ask to relaunch
elevated; this triggers the normal Windows UAC prompt and the new copy takes
over the port. `conduit://` links and a second launch just bring the existing
window forward (single-instance, authenticated with a per-run key a web page
cannot read).

## Security model

The threat is any web page — including one you did not open on purpose — reaching a
local daemon that can write files and start processes. Every layer below is enforced
on every request.

| Layer | What it stops |
|---|---|
| Bound to `127.0.0.1` only | anything off-box |
| `Host` must be loopback | DNS rebinding (`evil.com` → 127.0.0.1) |
| `Origin` required, http(s) only | `null`/`file://` origins, sandboxed iframes |
| Grant per origin, created only by a human answering a console prompt | drive-by pages |
| Bearer token in a header (never a cookie); only its SHA-256 is stored | CSRF, token theft off disk |
| Permission scopes `fs` / `hw` / `launch` | a save-game site touching `app.launch` |
| Per-origin sandbox dir + path validation (no `..`, absolute, UNC, device names, illegal chars, symlink escape) | reading `C:\Users\…\.ssh` or another site's saves |
| Per-file cap, byte quota, file-count cap, 48 MB body cap | disk-filling |
| Token bucket (30/s, burst 60), max 4 sockets per origin | request floods |
| `app.launch` is argv-only — never a shell — refuses `.bat .cmd .ps1 .vbs .js .lnk .scr .msi`, requires an absolute existing `.exe`, and asks for consent per call unless pre-allowed | RCE, argument injection (CVE-2024-24576 class) |
| `hw.info` returns no serials, hostname or usernames | fingerprinting/identity leakage |

Flags: `--port`, `--data-dir`, `--allow-origin O` (skip the pairing prompt for one
origin), `--launch-allow PATH` (skip the launch prompt for one executable),
`--max-file`, `--quota`, `--yes` (auto-approve — tests/kiosk only), `--deny`
(auto-deny, for headless boxes).

Grants live in `<data-dir>/grants.json`; site files in `<data-dir>/sites/<origin>/`.
Delete a grant line to revoke access; a page can revoke its own with the `revoke` method.

## Protocol

Pair once (this is what triggers the console prompt):

```bash
curl -X POST http://127.0.0.1:8765/pair -H 'Origin: https://example.com' \
     -H 'content-type: application/json' -d '{"perms":["fs","hw"]}'
# → {"ok":true,"result":{"token":"…","perms":["fs","hw"]}}
```

Then either transport, same JSON both ways:

```js
// HTTP
fetch('http://127.0.0.1:8765/rpc', {
  method: 'POST',
  headers: { 'content-type': 'application/json', authorization: `Bearer ${token}` },
  body: JSON.stringify({ id: 1, method: 'fs.write', params: { path: 'saves/a.json', data: '{}' } }),
});

// WebSocket — first frame must be auth
const ws = new WebSocket('ws://127.0.0.1:8765/ws');
ws.onopen = () => ws.send(JSON.stringify({ id: 1, method: 'auth', params: { token } }));
```

Replies are `{"id":1,"ok":true,"result":{…}}` or
`{"id":1,"ok":false,"error":{"code":"bad_path","message":"…"}}`.

| Method | Perm | Params |
|---|---|---|
| `ping`, `perms`, `revoke` | – | – |
| `hw.info` | `hw` | – |
| `fs.write` | `fs` | `path`, `data`, `encoding` (`utf8`\|`base64`), `append` |
| `fs.read` | `fs` | `path`, `encoding` |
| `fs.list` | `fs` | `path` (default root) |
| `fs.stat`, `fs.mkdir` | `fs` | `path` |
| `fs.delete` | `fs` | `path`, `recursive` |
| `fs.quota` | `fs` | – |
| `fs.copy`, `fs.move` | `fs` | `from`, `to`, `overwrite` |
| `fs.reveal` | `fs` | `path` (opens Explorer, throttled) |
| `app.list` | `launch` | – |
| `app.launch` | `launch` | `path` (absolute exe), `args` (array), `cwd` (inside the sandbox); asks per call unless remembered |
| `sys.stats` | `hw` | – (cpu, per-core, mem, net rate, uptime, battery) |
| `sys.battery` | `hw` | – |
| `sys.elevation` | – | – |
| `sys.elevate` | `system` | – (UAC prompt, relaunches elevated) |
| `sys.media` | `system` | `key` (play_pause/next/prev/stop/volume_up/volume_down/mute), `times` |
| `sys.open_url` | `system` | `url` (http/s, throttled) |
| `sys.processes` | `process` | `sort` (memory/cpu/name), `limit` |
| `sys.kill` | `process` | `pid` (asks; refuses PIDs ≤ 4 and critical processes) |
| `sys.power` | `power` | `action` (lock/sleep/logoff/restart/shutdown/abort; asks, 10 s grace) |
| `clipboard.write` | `clipboard` | `text` |
| `clipboard.read` | `clipboard` | – (asks every time) |
| `notify` | `notify` | `title`, `body` (tagged with the site name, throttled) |

Error codes: `bad_host`, `bad_origin`, `unauthorized`, `denied`, `rate_limited`,
`bad_params`, `bad_path`, `not_found`, `not_utf8`, `too_large`, `exists`, `quota`,
`io`, `no_method`, `bad_json`, `too_many_connections`.

Permissions: `fs`, `hw`, `launch`, `system`, `process`, `power`, `clipboard`,
`notify`. A grant only holds the permissions the user ticked at pairing;
`sys.kill`, `sys.power`, `clipboard.read`, `sys.elevate` and un-remembered
`app.launch` also ask again per call.

## TurboWarp extension

38 blocks. File basics (save / append / read / exists / size / list / delete /
make folder / copy / move / reveal / quota) plus an **easy key-value store** —
`save [v] as [k]`, `load [k]`, `has [k]?`, `delete saved [k]`, `all saved keys`,
`use save file [name]` — so a project can save state in two blocks with no paths
or JSON. Then hardware field + JSON + `live [stat]` + `on battery?`; launch app,
media keys, open URL, notify, clipboard read/write; list processes, end process,
power actions, `running as administrator?`, request administrator; and a raw
`call [method] with params [json]` escape hatch. Load it **unsandboxed** — a
sandboxed extension runs in a null-origin iframe, which the host refuses by
design:

```
https://turbowarp.org/editor?extension=http://127.0.0.1:8765/turbowarp/extension.js
```

The token is cached in `localStorage`, so the pairing prompt appears once per origin.

## Tests

```bash
cargo test
```

14 end-to-end tests drive the real binary over real sockets (traversal, quota,
rate limiting, Host spoofing, cross-origin token reuse, permission scoping, WS auth
ordering, launch refusals) plus 3 unit tests — run with `--headless`. Two browser
pages cover the rest: `/test` exercises the wire protocol from a real page, and
`/test/extension` runs the TurboWarp extension against a Scratch VM shim
(38 checks — file basics, the key-value store, copy/move, live stats, processes,
clipboard write, admin query). `/gui/preview` and `/gui/preview/consent` render
the GUI with mock data for visual checks without the native shell.
