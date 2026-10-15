# ClipBridge

ClipBridge keeps the clipboard in sync across your computers on the same local
network. Copy on one machine, paste on another. It's a small tray app (menu bar on
macOS) for **Windows and macOS**, written in Rust.

- Syncs **text and images**. Files are not synced.
- **Finds the other machines on its own** via mDNS/Bonjour. If your network blocks
  that, list the peers by hand.
- **Everything on the wire is encrypted** with ChaCha20-Poly1305. The key comes from
  a passphrase you share between your machines (Argon2id).
- **Light on resources.** About 7 MB RAM in the background and practically zero CPU
  while idle. On Windows it reacts to clipboard change events instead of polling.
- Tray menu: status, pause/resume, settings window, open config, open logs, start
  at login, quit.

## Install / build

You need [Rust](https://rustup.rs) (stable).

```sh
cargo build --release
```

The binary ends up at `target/release/clipbridge` (`clipbridge.exe` on Windows).
CI builds both platforms on every push; download them from the run's **Artifacts**.

### Windows

- Run `clipbridge.exe`. There's no console window; the icon shows up in the
  notification area (you may need to expand the `^` overflow).
- The first time it runs, **Windows Firewall** asks whether ClipBridge may talk on
  the network. Allow it on **private** networks, or other machines can't connect.
  If you clicked it away, allow `clipbridge.exe` under
  *Windows Security → Firewall → Allow an app through firewall*.
- Left-click the tray icon to open Settings; right-click for the menu.

### macOS

- Run `clipbridge`. It lives in the menu bar only and has no Dock icon.
- The binary is not signed, so **Gatekeeper** blocks it the first time. Clear the
  quarantine flag:

  ```sh
  xattr -d com.apple.quarantine /path/to/clipbridge
  ```
- macOS 15+ may ask for **Local Network** access. Allow it, otherwise peers can't
  be found or reached.

## First-time setup

1. Start ClipBridge on every machine.
2. On each machine, open **Settings…** from the tray menu and enter a passphrase.
   **Use the same passphrase everywhere.** Until you set one, the icon is amber and
   nothing syncs.
3. Save. The machines find each other within a few seconds and the icon turns
   green. Settings lists the connected peers.

If they don't find each other (some Wi-Fi networks block multicast), add the other
machine under **Manual peers** as `ip:port`, e.g. `192.168.1.20:47821`. Adding it
on one side is enough.

Tray icon colours: **green** syncing · **grey** paused · **amber** not configured ·
**red** error (see Settings or the log).

## Configuration

`config.toml` sits in the platform config directory:

| OS      | Path |
|---------|------|
| Windows | `%APPDATA%\ClipBridge\config\config.toml` |
| macOS   | `~/Library/Application Support/ClipBridge/config.toml` |

| Key            | Default          | Meaning |
|----------------|------------------|---------|
| `device_id`    | random UUID      | Identity of this machine; don't copy it to other machines. |
| `device_name`  | host name        | Shown to peers. |
| `passphrase`   | placeholder      | Shared secret; must match on all machines. |
| `port`         | `47821`          | TCP port to listen on. |
| `manual_peers` | `[]`             | Extra peers as `"host:port"`. |
| `sync_images`  | `true`           | Sync images as well as text. |
| `max_image_mb` | `20`             | Images larger than this (as PNG) are skipped. Max 30. |
| `discovery`    | `true`           | Use mDNS to find peers. |

Edits made in Settings, or straight in the file, take effect within a second or
so; no restart needed.

Logs are kept for 7 days and rotate daily, in
`%LOCALAPPDATA%\ClipBridge\data\logs` (Windows) or
`~/Library/Application Support/ClipBridge/logs` (macOS). **Open logs folder** in
the tray menu takes you there.

### Command line

```
clipbridge [--config <path>] [--headless] [--verbose] [--settings]
```

- `--headless` runs without a tray icon, logs to stderr, and stops on Ctrl+C.
- `--settings` opens the settings window of the running instance. Launching
  ClipBridge a second time does the same thing.

## How it works

Each machine keeps a direct encrypted TCP connection to every other machine (a
full mesh, nothing is forwarded). After a copy, ClipBridge waits for the clipboard
to settle (150 ms), reads it, and sends the content to all peers: text as-is,
images as PNG. Hashes and clip IDs make sure each clip is applied once per machine
and never bounces back.

Peers exchange pings every 15 s and drop a connection after 45 s of silence.
Dropped connections are retried with exponential backoff (1 s up to 30 s). After
sleep/wake, all connections are re-established right away.

## Known limitations

- Only text and images (PNG). No files, rich text/HTML, or other formats. If both
  text and an image are on the clipboard, the text wins.
- Text is limited to 2 MB; images to `max_image_mb`, which can't go above 30 MB
  because of the 32 MB frame limit.
- Peers must be on the same LAN or reachable via manual `host:port` entries.
  There's no relay through the internet.
- The passphrase is the only secret. Anyone who knows it and can reach the port
  can read and write your clipboard. Choose a long one.
- macOS is not packaged as an `.app` bundle and is not signed or notarized.
- Linux may work (X11) but isn't tested or supported.

## Development

```sh
cargo test                                  # unit + in-process multi-node tests
cargo test -- --ignored                     # also: mDNS on this host, real OS clipboard
cargo run -- --headless --verbose           # run with logs in the terminal
```

Integration tests start several nodes in one process on loopback, using a mock
clipboard. Setting `CLIPBRIDGE_BIND=127.0.0.1` makes the app listen on loopback
only, which is handy for local experiments without firewall prompts.
