# Avro

A native GTK4/libadwaita client for **Telegram** on Linux. Named after the company
that built delta-winged aircraft like the Vulcan and the Arrow, because it grew out of
Paper Plane.

Avro is modelled on [Paper Plane](https://github.com/paper-plane-developers/paper-plane)
(GTK4 + TDLib) but shares no code with it, and doesn't use its `tdlib-rs` bindings. It
talks to Telegram's own [TDLib](https://github.com/tdlib/td) through a small hand-written
JSON bridge. The app shell (layout, notifications, theming, CLI) follows
[Bubo](https://github.com/turbineBMW/Bubo).

Avro is an unofficial client. It uses the Telegram API but is not made or endorsed by Telegram.

## Layout

- `src/tg/`         — the Telegram side
  - `ffi.rs`        — dlopens `libtdjson.so` (td_create_client_id / td_send / td_receive / td_execute)
  - `client.rs`     — one receive thread, `@extra` request/response correlation, typed requests
  - `types.rs`      — serde mirrors of the part of `td_api.tl` Avro reads (updates, chats, messages, files)
  - `auth.rs`       — API credentials (`~/.config/avro/api.json`) and TDLib's directories
- `src/ui/`         — libadwaita UI: API-credentials page, login (phone / QR / code / 2FA / email),
                      chat list, thread, composer, media, pickers
- `src/notify.rs`, `src/omarchy/`, `src/accent.rs`, `src/gif.rs`, `src/settings.rs` — shared with Bubo
  (notifications with OTP copy, Omarchy theme, system accent, DuckDuckGo GIF search, preferences)
- `build-tdlib.sh`  — builds `libtdjson.so` into `~/.local/lib`

## Building

```sh
sudo pacman -S --needed cmake gperf clang ninja   # TDLib's build dependencies
./build-tdlib.sh                                  # ~10 min; installs ~/.local/lib/libtdjson.so
cargo run                                         # GUI
./install.sh                                      # ~/.local/bin/avro + .desktop + icons
```

Avro loads TDLib at runtime. It looks in `$AVRO_TDJSON`, then `~/.local/lib`, `/usr/local/lib` and
`/usr/lib`, so `cargo build` works without TDLib installed. If the library is missing, the window
says so.

## Running

```sh
cargo run                         # GUI; first launch asks for the API ID/hash, then logs in
cargo run -- login                # headless: phone number, code, 2FA password at the terminal
cargo run -- login --qr           # headless: QR code to scan from Telegram on the phone
cargo run -- probe                # list chats, then tail live updates
cargo run -- probe CHAT_ID        # dump recent messages of one chat
cargo run -- send CHAT_ID hello there
cargo run -- logout
RUST_LOG=avro=debug cargo run -- probe   # see every request
AVRO_TEST_DC=1 cargo run          # Telegram's test servers (separate accounts)
```

TDLib locks its database, so only one copy can run at a time: the GUI or one CLI command.
A second GUI launch raises the first window. Logs: `~/.cache/avro/avro.log` and
`~/.cache/avro/tdlib.log`.

## Getting a Telegram API ID

Every Telegram client identifies itself with an `api_id` and `api_hash`. Avro ships none, so you
make your own:

1. Sign in at <https://my.telegram.org> with your phone number. The code arrives in your
   Telegram app.
2. Open **API development tools** and fill in the form: App title (e.g. `Avro`), Short name
   (e.g. `avro`), Platform: Desktop. URL and description are optional.
3. Copy **App api_id** and **App api_hash** into Avro's first-run page, or run `avro login`.

Things worth knowing:

- **It's free.** Each phone number gets one app. You can't revoke or regenerate the pair, so
  keep the hash out of public repos. Avro reads it from `~/.config/avro/api.json` (mode 600),
  `$AVRO_API_ID`/`$AVRO_API_HASH`, or the same variables at build time.
- **The form often answers just "ERROR".** That usually means Telegram is throttling
  registrations or doesn't like your IP. Turn a VPN off (or on), try a private window, keep the
  title and short name plain Latin, wait a while, and retry.
- **The API terms** (<https://core.telegram.org/api/terms>): don't put "Telegram" in the name
  (or only as "Unofficial Telegram …"), don't use Telegram's logo, say the app uses the Telegram
  API, protect user data, and keep read receipts, typing and online status working. Clients that
  show channels must also show Telegram's sponsored messages (Avro doesn't yet; see Status).
  Telegram gives you 10 days to fix a violation before it cuts the api_id off.
- **Account safety.** Telegram puts accounts that log in through unofficial clients
  "under observation". Since April 2026 it can also warn the people you talk to that you use an
  unofficial app. Log in with an established account made in an official app; don't sign up
  through Avro. FLOOD_WAIT errors mean Telegram wants you to slow down, and Avro shows how long.
- **Test DC.** `AVRO_TEST_DC=1` uses Telegram's test servers. Numbers there look like
  `99966XYYYY` (X = DC 1–3), the login code is X five times, and the accounts are public
  scratch space. Good for exercising the login flow without risking your real account's flood
  limits.

## Appearance

Avro follows the active Omarchy theme by default, including its palette and light or dark mode,
exactly like Bubo. Turn off **Preferences → Appearance → Follow Omarchy theme** to use the
system libadwaita appearance. A theme can ship `avro.css` to replace the generated colours.

## How it works

1. **TDLib does the protocol.** MTProto, encryption, the local SQLite cache, update-gap recovery
   and file transfer all live in TDLib. Avro sends JSON requests (`{"@type": "getChatHistory", …}`)
   and reads JSON back.
2. **One receive thread.** `td_receive` serves every client and is not thread-safe, so a single
   thread drains it. Replies carry the request's numeric `@extra` and wake the waiting future (a
   oneshot, so the GTK thread awaits them directly). Everything else is an update, parsed into
   `types::Update` and sent down an async channel.
3. **Login is TDLib's state machine.** `updateAuthorizationState` says what TDLib is waiting for
   (parameters → phone or QR → code → password, sometimes email), and the UI draws that step.
4. **Chats arrive as updates.** `loadChats` makes TDLib announce chats with `updateNewChat` plus
   `updateChatPosition` and `updateChatLastMessage`. The sidebar is a sort of the kept chats by
   their main-list `order` (an int64, sent as a string), rebuilt once per main-loop turn.
5. **History pages backwards.** `getChatHistory` answers from the local cache first and can come
   back short, so `Client::history` keeps asking until the page is full or empty.
6. **Sending shows the message at once.** `sendMessage` returns a pending message with a
   temporary id. `updateMessageSendSucceeded` later swaps in the real one.

## Status

- [x] API-credentials page, login by phone + code + 2FA password, email-code logins, QR login
- [x] Chat list (pinned, muted, unread counts, typing), history with infinite scroll, rich text and links
- [x] Send text, files, photos and GIFs (picker); read receipts; typing indicator; presence in the header
- [x] Photos, stickers (WebP, WebM; Lottie shown as its emoji), GIFs, videos, voice notes, audio, files
- [x] Desktop notifications honouring Telegram's mute settings (click to open, copy login codes)
- [x] New-chat picker: contacts, @usernames, phone numbers; leave/delete chats; mark as read
- [x] Headless `login` / `probe` / `send` / `logout`
- [ ] Sponsored messages in channels (needed by the API terms before Avro is published)
- [ ] Replies, reactions, editing, forwarding, archive and folders, secret chats, calls

## Licence

Avro's code is MIT. TDLib is Boost Software License 1.0. The modules shared with Bubo carry
Bubo's licence (also MIT).

## Gotchas (learned the hard way)

- **QR login is one-way.** From `authorizationStateWaitOtherDeviceConfirmation`, TDLib refuses
  `setAuthenticationPhoneNumber`. Going back to phone login means `logOut`, waiting for
  `authorizationStateClosed`, and starting a fresh client. The login view does exactly that.
- **int64 fields are strings** in TDLib's JSON (`chatPosition.order`, sticker ids). int53 and
  int32 fields are numbers.
- **Entity offsets count UTF-16 code units**, not bytes or chars. An emoji before a bold span
  shifts it by two.
- **Close TDLib before exiting.** The window hides, sends `close`, and quits on
  `authorizationStateClosed` (or after 3 s), so the database isn't left mid-write.
