# NavaCast

NavaCast is a radio server. One process runs AutoDJ, accepts a live source, and sends the same encoded audio to HTTP listeners and to HLS. That station can also feed TeamSpeak 3 voice bots. `navad` is the server. `navactl` is the control CLI. The dashboard is a React app embedded in `navad` after you build `web/`.

Sources connect with the Icecast source protocol (`SOURCE` or `PUT`, HTTP Basic authentication). Listeners that speak the Icecast listener protocol use HTTP GET, with optional ICY metadata. NavaCast does not terminate TLS.

The dashboard is English by default. Switching the UI to Persian uses the name نوا کست.

NavaCast is open source under the MIT License. See [LICENSE](LICENSE).

## Features

- **AutoDJ.** A station library, playlists, a queue, shuffle, repeat (`off`, `one`, `all`), crossfade, gain, an intro bed, and a jingle every N tracks. Weekly schedule windows pick the playlist. Seek, jump, and play-next work while AutoDJ owns the mount.
- **Live takeover.** A connected source is decoded and re-encoded on the mount, so existing listeners stay on the same stream. When the source disconnects, AutoDJ resumes on those connections. Seek is rejected while the live source owns the mount. A second source is refused unless the mount allows takeover.
- **HLS.** Each mount publishes `index.m3u8` and `playlist.m3u8`. Segments are MPEG-TS. The segment length is the mount's `hls_segment_ms` (default 1000, clamped to 250–2000).
- **Icecast listeners.** `GET` on the mount returns a continuous HTTP audio stream (`audio/mpeg` or `audio/ogg`). A client that sends `Icy-Metadata: 1` receives `icy-metaint` metadata. The server name on that response is `navad`.
- **Encoders.** `ffmpeg` (external `ffmpeg`, MP3), `native` (in-process LAME MP3), and `opus` (in-process Ogg Opus). Native MP3 and Opus are linked into `navad`.
- **Relay.** While no live source is connected, a mount can pull an `http` or `https` audio stream and re-encode it for listeners.
- **Recording.** Each mount can save the encoded bytes listeners already hear, including live, AutoDJ, and relay. One encoder. Files stay under the data directory.
- **TeamSpeak 3.** Bots live on their own tab. Each one is a voice client: connect it, then click a channel to join. Several bots can be connected at once. A station page turns the public Icecast and HLS stream on or off and chooses which bots that station feeds. Opus frames go out on the TeamSpeak voice port. Nothing new binds on the stream port.
- **Domain and reverse proxy.** Bind addresses and public URLs are separate. `X-Forwarded-Host`, `X-Forwarded-Proto`, and `X-Forwarded-For` are honored only from peers listed in `trusted_proxies`.

## Quick start

Build the dashboard, then the binaries:

```bash
cd web && npm install && npm run build && cd ..
cargo build --release
```

```bash
cp navad.example.json navad.json
./target/release/navad --config navad.json
```

`navad.example.json` is the sample. It contains no passwords. The first start creates the admin account and prints the username, password, and API token once. The default username is `admin` unless `NAVA_ADMIN_USER` is set. The password is random unless `NAVA_ADMIN_PASSWORD` is set (at least 8 characters). Store the token. It is not shown again.

Ctrl+C stops the process. On Windows the same handler runs for the console control event. The handler closes listeners, mount threads, and encoder children, then the process returns.

Defaults after a sample config:

- Stream (listeners, sources, HLS): `http://127.0.0.1:8000`. No `/api` and no `/admin`.
- Control API and dashboard: `http://127.0.0.1:8088`.
- Optional `public_listen` serves the listener pages and `/api/public/*` only.

Sign in at the control URL. The listener page is `#/listen/<slug>` and has no admin controls.

For dashboard development:

```bash
cd web && npm run dev
```

Vite serves `http://127.0.0.1:5173` and proxies `/api` to `127.0.0.1:8088`.

Point an encoder at a mount with the Icecast source URL. The user is the mount source user (`source` unless you set another). Pass the source password with your encoder's own credential field, not in a file you commit. The shell form is `icecast://<source-user>:<source-password>@127.0.0.1:8000/live`. Listeners open `http://127.0.0.1:8000/live`. HLS is `http://127.0.0.1:8000/live/index.m3u8`.

## Configuration

`navad` reads `--config PATH`, or `navad.json` in the working directory when that file exists, or built-in defaults. Environment variables override the file.

| Field | Default | Meaning |
| --- | --- | --- |
| `data_dir` | `data` | SQLite database `navad.db` and uploaded media. `NAVA_DATA_DIR`. |
| `stream_listen` | `:8000` | Stream bind. A leading colon means all interfaces (`:8000` is `0.0.0.0:8000`). `NAVA_STREAM_LISTEN`. |
| `http_listen` | `127.0.0.1:8088` | Control API and dashboard. `NAVA_HTTP_LISTEN`. |
| `public_listen` | empty | Optional bind with no admin routes. `NAVA_PUBLIC_LISTEN`. |
| `stream_public_url` | `http://127.0.0.1:8000` | URL players and HLS playlists use. `NAVA_STREAM_PUBLIC_URL`. |
| `http_public_url` | `http://127.0.0.1:8088` | URL the dashboard publishes. `NAVA_HTTP_PUBLIC_URL`. |
| `trusted_proxies` | `127.0.0.1`, `::1` | Peers allowed to set forwarded headers. An empty list is treated as localhost. |
| `max_upload_bytes` | `67108864` | Library upload cap (64 MiB). `NAVA_MAX_UPLOAD_BYTES`. |
| `source_timeout_sec` | `15` | Read timeout on a live source. `0` is treated as 15. |
| `max_conns_per_ip` | `16` | Listener connections per IP on the stream port. `0` is treated as 16. |
| `meta_int` | `16000` | ICY metadata interval in bytes. Values below 256 are raised to 16000. |
| `fade_ms` | `250` | Engine default crossfade. Each mount stores its own `crossfade_ms` (0–10000). |
| `tz_offset_min` | `0` | Minutes added to UTC for schedule windows. |
| `allow_private_relay` | `false` | Allow a relay to pull loopback, link-local, or private addresses. `NAVA_ALLOW_PRIVATE_RELAY` is `1`, `true`, `yes`, or `on`. |
| `max_recording_bytes` | `1073741824` | Cap on all recording files together (1 GiB). `NAVA_MAX_RECORDING_BYTES`. |

Public URLs are stored without a trailing slash. `navad.json` is local configuration. Do not commit it. Keep using `navad.example.json` as the sample.

### Domain and reverse proxy

`stream_listen` and `http_listen` are bind addresses. `stream_public_url` and `http_public_url` are the names browsers and players use, for example `https://radio.example.com`. HLS playlists and the dashboard use those URLs.

TLS terminates at the reverse proxy. `navad` speaks plain HTTP. A typical split is `/live/` to the stream port with buffering off, and `/` to the control port. Forwarded headers are trusted only when the socket peer is in `trusted_proxies`.

```nginx
server {
    listen 443 ssl;
    server_name radio.example.com;
    ssl_certificate     /etc/ssl/radio.crt;
    ssl_certificate_key /etc/ssl/radio.key;

    location /live/ {
        proxy_pass http://127.0.0.1:8000/live/;
        proxy_http_version 1.1;
        proxy_buffering off;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Host $host;
        proxy_set_header X-Forwarded-Proto https;
        proxy_set_header X-Forwarded-For $remote_addr;
    }

    location / {
        proxy_pass http://127.0.0.1:8088;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Host $host;
        proxy_set_header X-Forwarded-Proto https;
        proxy_set_header X-Forwarded-For $remote_addr;
    }
}
```

```json
{
  "stream_listen": "0.0.0.0:8000",
  "http_listen": "127.0.0.1:8088",
  "stream_public_url": "https://radio.example.com",
  "http_public_url": "https://radio.example.com",
  "trusted_proxies": ["127.0.0.1", "::1"]
}
```

With that proxy, listeners use `https://radio.example.com/live` and HLS uses `https://radio.example.com/live/index.m3u8`. The control API is on the same host. Put the dashboard on a separate name if you do not want it on the public origin.

## TeamSpeak 3

The **TeamSpeak** tab holds the voice bots. Add one, set the host and the voice port, and connect it. The bot is a TeamSpeak 3 voice client, the same idea as SinusBot: it joins a channel and plays audio. It is not a ServerQuery script, and it is not TeamSpeak 5. A bot is not owned by a station. Deleting a station leaves the bot in place. Several bots can be connected at once. Each one has its own nickname, server, and channel.

On a station page, **Feeds** is where that station chooses its outputs. **Public radio** turns the Icecast and HLS stream on or off. The chips beside it choose which bots that station feeds. Turning a chip off stops the feed and leaves the bot on the tab. One bot follows one station at a time. Switching the station crossfades and does not reconnect. Opus frames go out on the TeamSpeak voice port. Nothing new binds on the stream port.

| Field | Meaning |
| --- | --- |
| Nickname, phonetic nickname, away message | Shown on the TeamSpeak client. |
| Host | TeamSpeak server hostname or IP. |
| Voice port | UDP voice port. `9987` is the usual one. |
| Follows | Which station this bot plays. |
| Default channel | Rejoined after a reconnect. Set it from the current channel, or by clicking a channel in the tree. |
| Server password | Stored for the bot. After save the form stays blank and only shows that one is set. |
| Channel password | Same. A password field appears only when the channel you click is locked. |
| Volume | `0`–`100` for this bot, plus a master volume for every bot. |
| Reconnect | Optional. If the server drops the bot, it tries again. |

While connected, the tab shows the live channel tree. Click a channel to join it. The current channel is marked. People in the channel can be muted, kicked, or moved when the bot has that server permission. If it does not, the button stays disabled and says why. Now playing is written as the bot's channel description.

The connection log records connected, joined a channel, a failed password, and dropped. It does not record passwords.

A stuck bot, a bad packet, a failed reconnect, or a panic in one bot does not stop Icecast, HLS, AutoDJ, or the other bots. Audio crosses a short queue. If the bot falls behind, frames are dropped. The radio does not wait on TeamSpeak.

Playback can follow the station or a queue of tracks already in that station's library: pause, skip, stop, clear, drag to reorder, repeat one, and repeat all. There is no YouTube or other remote download.

Listeners in the channel can type commands. The prefix defaults to `!` and each command can be turned off: play, queue, skip, stop, now playing, volume, radio, and help. Commands can be limited to everyone or to one named server group, and they are rate-limited.

To aim a bot at a test server, set the host, the voice port (usually `9987`), the server password if the server has one, and a nickname. Save, then Connect. Click the channel in the tree. The identity is generated and stored in the local database. It is not shown again and it is not written to the log.

## navactl

```bash
export NAVA_API=http://127.0.0.1:8088
export NAVA_TOKEN
navactl stations create --name "Night" --slug night
navactl mounts create --station STATION_ID --path /live --password "$SOURCE_PASSWORD"
navactl upload --station STATION_ID --file track.mp3
navactl playlists create --station STATION_ID --name nights
navactl playlists add --playlist PLAYLIST_ID --media MEDIA_ID
navactl live
navactl seek --mount MOUNT_ID --position 15000
navactl jump --mount MOUNT_ID --media MEDIA_ID
navactl play-next --mount MOUNT_ID --media MEDIA_ID
navactl mounts update --id MOUNT_ID --repeat one --shuffle true --gain -3 --buffer 250 --hls-segment 1000 --encoder native
navactl logs
```

Set `NAVA_TOKEN` to the token printed at first start, or to a token from `navactl login`. Set `SOURCE_PASSWORD` in your shell. Do not write either value into the repository. `navactl --help` lists the commands: stations, mounts, upload, media, playlists, queue, seek, jump, play-next, schedules, listeners, users, login, live, logs, and health.

Mount paths start with `/`, are at most 64 characters, and contain only ASCII letters, digits, `/`, `_`, and `-`. The source password is at least 4 characters. Usernames are 3–32 characters of ASCII letters, digits, and `_`.

## Encoders

Each mount stores `ffmpeg`, `native`, or `opus`. The dashboard and `navactl mounts update --encoder` set it. The change is written to the audit log.

| Id | Output | Requirement |
| --- | --- | --- |
| `ffmpeg` | MP3 (`audio/mpeg`) via `libmp3lame` | `ffmpeg` on `PATH` |
| `native` | MP3 (`audio/mpeg`) via in-process LAME | none |
| `opus` | Ogg Opus (`audio/ogg`) via in-process libopus | none |

A new mount uses `ffmpeg` when that binary is installed, otherwise `native`. Choosing `ffmpeg` while it is missing, or naming an unknown encoder, returns an error and does not leave the mount wedged. The server still starts, and AutoDJ runs on the native MP3 encoder, when `ffmpeg` is absent.

Mount bitrate is clamped to 32–320 kbps (default 128). Native MP3 snaps to the nearest LAME rate in that range. Opus always encodes at 48 kHz and resamples from the mount rate. Sample rate is one of 8000, 11025, 16000, 22050, 32000, 44100, and 48000 (default 44100). Channels are mono or stereo (default stereo). Output gain is clamped to −24 dB through +24 dB. The listener buffer is 50–1000 ms (default 250). A longer buffer or a longer HLS segment is heard later.

Library uploads are checked by file header. Accepted types are MP3, WAV, FLAC, Ogg, M4A, and AAC, up to `max_upload_bytes`. The native and Opus decoders play MP3, WAV, and raw PCM (`audio/L16`, `audio/pcm`). FLAC, Ogg, M4A, and AAC need the `ffmpeg` encoder so `ffmpeg` can decode them. A track that fails to decode is skipped and logged. The process stays up.

A live source is decoded with the mount's encoder and mixed into the same output. `audio/L16` and `audio/pcm` are already PCM. Declared bitrate (`Ice-Bitrate` or `icy-br`) above the mount's `max_bitrate` (default 320 kbps) is rejected. A source that then exceeds that cap is dropped. If the encoder exits, stops accepting PCM, or goes silent, that mount's pump restarts. Playing is reported only while encoded bytes reach listeners.

`PATCH /api/mounts/:id` accepts `encoder` values `ffmpeg`, `native`, and `opus`.

## Relay

Set `relay_url` in the dashboard or with `navactl mounts update --id MOUNT_ID --relay URL`. An empty URL turns the relay off.

While no live source is connected, `navad` pulls that URL and sends the audio to listeners. A live `SOURCE` or `PUT` replaces the relay. Listeners stay connected. When the source leaves, the relay is tried again.

Rules enforced by the server:

- Only `http` and `https`. `file:` and every other scheme are rejected. User info in the URL is rejected. The URL is at most 2048 characters.
- Paths under `/api` and `/admin` are rejected. The pull will not connect to the control listener or the public listener.
- Loopback, link-local, private, CGNAT (`100.64.0.0/10`), unspecified, multicast, and documentation addresses are rejected unless `allow_private_relay` is true.
- Redirects are followed at most three times, and each target is checked again.
- The pull is capped at the mount's max bitrate. ICY metadata on the upstream stream is stripped and can update the now-playing title.
- A failed pull retries with backoff, starting at 300 ms and doubling up to 8 seconds. After three failures the mount falls back to AutoDJ and tries the relay again about 10 seconds later. Playing stays false until relay bytes reach the fanout.

## Recording

Recording writes the encoded bytes already sent to listeners. It does not start a second encoder. Live, AutoDJ, and relay all land in the same file while recording is on. The on/off flag is stored on the mount. Each file stops at its max duration (default one hour, from 1 second to 24 hours). All recording files together stay under `max_recording_bytes`.

Files live in `data_dir/recordings/<mount-id>/`. The control API is the only way to download one. The stream port and the dashboard's static files do not serve them.

```bash
navactl record start --mount MOUNT_ID --max-ms 3600000
navactl record list --mount MOUNT_ID
navactl record stop --mount MOUNT_ID
navactl record delete --mount MOUNT_ID --id RECORDING_ID
navactl record delete --mount MOUNT_ID --all
```

| Method | Path | |
| --- | --- | --- |
| `POST` | `/api/mounts/:id/record/start` | Body `{"max_duration_ms":3600000}`. Persists recording on. |
| `POST` | `/api/mounts/:id/record/stop` | Closes the file and persists recording off. |
| `GET` | `/api/mounts/:id/recordings` | Start time, duration, format, size, mount. |
| `GET` | `/api/mounts/:id/recordings/:rid` | Authenticated download. A recording from another mount is rejected. |
| `DELETE` | `/api/mounts/:id/recordings/:rid` | Delete one. |
| `DELETE` | `/api/mounts/:id/recordings` | Delete every recording on the mount. |
| `POST` | `/api/mounts/:id/recordings/:rid/jump` | Play the recording now, like a library jump. |
| `POST` | `/api/mounts/:id/recordings/:rid/queue` | Queue the recording next, like a library track. |

Start, stop, and delete are written to the audit log. If the disk write fails, the recording stops with `disk write failed` and the live stream stays up.

## Security

- The control API binds to localhost unless you change `http_listen`. The stream port does not serve admin routes. `public_listen`, when set, serves the listener UI and `/api/public/*` only.
- Admin and source passwords are stored as Argon2id. API tokens are random, shown once, and stored as SHA-256. `navactl login` and `POST /api/tokens/rotate` issue a new token and revoke the previous ones for that user.
- Login failures and failed source logins are limited to 8 attempts per IP per 60 seconds. Unknown mounts still run a password check so the failure does not return early.
- The control API sends `Access-Control-Allow-Origin: *`. Do not publish port 8088 on the open internet. Terminate TLS and access control at a proxy, or keep the control port on localhost.
- Forwarded headers are ignored unless the TCP peer is in `trusted_proxies`. A client that is not on that list cannot spoof the public URL or the client IP.
- Uploads are size-capped. Media ids are generated by the server. Request paths cannot traverse out of the media directory.
- Relay targets follow the rules in the previous section. Leave `allow_private_relay` false unless you mean to pull a private address.
- Source passwords are at least 4 characters. Admin passwords are at least 8. Neither belongs in `navad.json`, in shell history you share, or in this repository.

## Build on Linux

You need a Rust toolchain (`cargo`), Node.js and npm, and a C toolchain with CMake. libopus is compiled while `navad` builds. `ffmpeg` is optional and is used only when a mount selects that encoder.

The lockfile pins the TLS crates to the Rust 1.83 dependency graph.

```bash
cd web && npm install && npm run build && cd ..
cargo build --release
cargo test
```

`npm run build` writes `web/dist`. `navad` embeds that directory, so build the UI before `cargo build` when the binary should contain the dashboard. `cargo test` does not need a browser.

Release binaries are `target/release/navad` and `target/release/navactl`.

## Build on Windows

The Windows build used by this tree is the GNU target `x86_64-pc-windows-gnu`. That includes cross-compilation from Linux with mingw-w64 (`x86_64-w64-mingw32-gcc`) and CMake. This repository does not document an MSVC build.

Build the dashboard first, on either operating system:

```bash
cd web && npm install && npm run build && cd ..
rustup target add x86_64-pc-windows-gnu
RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --target x86_64-pc-windows-gnu
```

On Windows PowerShell, set the flag with `$env:RUSTFLAGS = "-C target-feature=+crt-static"` before `cargo build`. `+crt-static` links the GNU runtime into the executables so a second machine does not need `libgcc` or `libwinpthread` beside them.

Outputs:

- `target/x86_64-pc-windows-gnu/release/navad.exe`
- `target/x86_64-pc-windows-gnu/release/navactl.exe`

Ship those with `navad.example.json`. On the Windows host, install `ffmpeg.exe` on `PATH` only if a mount uses the `ffmpeg` encoder. Native MP3 and Opus do not need an extra DLL.

A prebuilt 64-bit zip is not produced by `cargo build`. Operators who only have the binaries can follow `WINDOWS.txt`.

## Tests

```bash
cargo test
```

## License

MIT. Copyright (c) NavaCast contributors. The full text is [LICENSE](LICENSE).
