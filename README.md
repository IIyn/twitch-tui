# twitch-tui

A Twitch client for the terminal: watch and listen to live streams, read and
write in chat, browse the channels you follow and search for new ones, without
leaving the console.

It is written in Rust with no external crates. Terminal handling, JSON, reading
the SQLite cookie store, the Twitch OAuth login and API clients are all
implemented in the project. Networking and media go through system tools (`curl`, `ffmpeg`,
an audio player).

## Features

- **Twitch login** through the official OAuth device flow: run
  `twitch-tui --login` once and approve a code on twitch.tv, no token to copy
  by hand.
- **Every followed channel** with their live status, title, category, viewer count
  and uptime.
- **Search** for channels on Twitch, and quick filtering of the followed list.
- **Channel page**: description, follower count, whether you follow it.
- **Audio** of the stream, with volume (0 to 150%), mute and pause.
- **Visualizer**: spectrum, mirrored spectrum, oscilloscope, or the stream
  picture.
- **Video** in the terminal, either as a real picture (terminals supporting the
  kitty graphics protocol) or as coloured ASCII everywhere else. The quality is
  adjustable (auto, 360p, 720p, 1080p, source). While the picture is shown,
  the sound is decoded from the same rendition and the frames follow it, so
  they stay in sync.
- **Live chat**: badges, user colours, `/me` actions, removal of moderated
  messages. Sending messages requires being logged in.
- **Open the channel in the browser**, in particular to follow or unfollow it
  (see [Twitch restrictions](#twitch-restrictions)).

## System requirements

**Linux only.** The terminal is driven directly through libc calls (`termios`,
`ioctl`, `sigaction`) using Linux structure layouts and constants. The program
also relies on `/dev/urandom`, `/dev/shm` and `xdg-open`. macOS, the BSDs and
Windows are not supported. WSL2 on Windows may work but has not been tested.

You also need:

- a **24-bit colour (truecolor) terminal**: every modern terminal qualifies
  (kitty, Ghostty, WezTerm, Alacritty, foot, Konsole, GNOME Terminal, recent
  xterm...). The bare Linux console (TTY) does not;
- a **font with the common Unicode block and symbol characters**, used by the
  visualizer and the interface;
- **network access** to `gql.twitch.tv`, `api.twitch.tv`, `id.twitch.tv` and
  `usher.ttvnw.net` over HTTPS, and to `irc.chat.twitch.tv` on TCP port 6667
  for chat.

### Terminal compatibility

Almost everything works in any terminal meeting the requirements above: the
interface, lists, search, chat, audio, the visualizer and ASCII video.

Only **real-picture video** needs a specific terminal. It uses the kitty
graphics protocol with shared-memory transfer: the terminal reads the pixels
straight from RAM and scales them on the GPU. This mode is detected
automatically in **kitty** and **Ghostty**. Elsewhere, video is drawn as
coloured ASCII.

| Terminal                   | Interface, chat, audio | ASCII video | Real-picture video   |
|----------------------------|------------------------|-------------|----------------------|
| kitty                      | yes                    | yes         | yes (automatic)      |
| Ghostty                    | yes                    | yes         | yes (automatic)      |
| Other truecolor terminal   | yes                    | yes         | no, unless forced    |
| SSH session (any terminal) | yes                    | yes         | no                   |

Notes:

- Over SSH the picture mode is always off: shared memory only exists on the
  machine running the terminal.
- `video_output = graphics` forces the picture mode on an unrecognised
  terminal. It only works if that terminal supports the kitty protocol **with
  shared memory** and reports its cell size in pixels.
- The `a` key switches between picture and ASCII at runtime, when the picture
  mode is available.

## Dependencies

### Build

- **Rust 1.88 or newer** (the project uses the 2024 edition and let chains).
  Recommended install: [rustup](https://rustup.rs).
- No external crates: `cargo build` has nothing to download.

### Runtime

| Program                        | Required    | Purpose                                            |
|--------------------------------|-------------|----------------------------------------------------|
| `curl`                         | yes         | HTTPS requests to the Twitch API and playlists     |
| `ffmpeg`                       | yes         | decoding the HLS stream (audio and video)          |
| `pw-cat`, `pacat` or `aplay`   | for sound   | audio output (PipeWire, PulseAudio or ALSA)        |
| `xdg-open`                     | no          | opening a channel in the browser (`o` key)         |

The program refuses to start if `curl` or `ffmpeg` is missing. It picks the
first available audio player in the order `pw-cat`, `pacat`, `aplay`. If none
is found, the interface and chat still work, without sound.

Installing them:

```sh
# Arch, CachyOS, Manjaro
sudo pacman -S curl ffmpeg pipewire xdg-utils

# Debian, Ubuntu
sudo apt install curl ffmpeg pipewire-bin xdg-utils

# Fedora
sudo dnf install curl ffmpeg pipewire-utils xdg-utils
```

On Fedora, the full `ffmpeg` comes from the RPM Fusion repository. Replace the
PipeWire package with `pulseaudio-utils` (for `pacat`) or `alsa-utils` (for
`aplay`) depending on your audio stack.

## Logging in

```sh
twitch-tui --login
```

This prints a code and opens twitch.tv/activate in the browser (open the
printed link yourself over SSH or without a desktop). Once you approve the
code, the session is saved to `~/.local/state/twitch-tui/session` (or
`$XDG_STATE_HOME/twitch-tui/session`), readable by you only, and renewed
automatically. It is revoked and removed by `twitch-tui --logout`.

The login asks Twitch for three permissions: reading your follows
(`user:read:follows`), and reading and writing in chat (`chat:read`,
`chat:edit`). It goes through Twitch's public API, which knows nothing of
subscriptions and cannot follow channels.

Without a login the program runs in **anonymous mode**: searching, playing
streams and reading chat still work. The followed channels list and sending
messages are unavailable.

### Optional: the website token

Separately, the program uses the token of the twitch.tv website itself when it
finds one. It is optional: it brings your subscriber perks to playback (no
ads, subscriber-only streams) and lets the program try to follow or unfollow
channels. It is read from the `auth-token` cookie in the cookie store
(`cookies.sqlite`) of Firefox-family browsers, as long as you are logged in on
twitch.tv there.

| Browser   | Profile locations searched                                                                          |
|-----------|-----------------------------------------------------------------------------------------------------|
| Firefox   | `~/.config/mozilla/firefox`, `~/.mozilla/firefox`, Snap install, Flatpak install (`org.mozilla.firefox`) |
| LibreWolf | `~/.config/librewolf/librewolf`, `~/.librewolf`, Flatpak install                                    |
| Zen       | `~/.config/zen/zen`, `~/.zen`, Flatpak install                                                      |
| Floorp    | `~/.config/floorp/floorp`, `~/.floorp`, Flatpak install                                             |
| Waterfox  | `~/.waterfox`                                                                                       |

Every profile of every one of these browsers is searched, and the most
recently used session wins. To pin a profile, use `--profile DIR` or
`firefox_profile` in the config file. `firefox_login = false` turns the lookup
off.

Chromium-based browsers (Chrome, Chromium, Brave, Edge, Vivaldi, Opera) **are
not supported**: they encrypt cookie values with the system keyring. With
those browsers, set the token by hand:

1. Log in on twitch.tv in your browser.
2. Open the developer tools (`F12`), then Storage or Application, then
   Cookies, then `https://www.twitch.tv`.
3. Copy the value of the `auth-token` cookie.
4. Put it in the config file (`token = ...`) or in the `TWITCH_TOKEN`
   environment variable.

This token grants access to your Twitch account: do not share it. The config
file is created with mode `600` (readable by you only).

## Building and running

```sh
git clone https://github.com/IIyn/twitch-tui
cd twitch-tui

# Build and run
cargo run --release

# Start straight on a channel (login or URL)
cargo run --release -- channel_name
cargo run --release -- https://www.twitch.tv/channel_name
```

To install the binary into `~/.cargo/bin` (which should be in your `PATH`):

```sh
cargo install --path .
twitch-tui
```

The built binary is also available at `target/release/twitch-tui`.

### Checking the login

```sh
twitch-tui --check-login
```

This prints which account the session opens, and where the website token
comes from and whose it is, then exits. Tokens themselves are never printed.

### Command-line options

| Option               | Effect                                                   |
|----------------------|----------------------------------------------------------|
| `CHANNEL`            | channel (login or URL) to play on startup                |
| `-v`, `--volume N`   | initial volume, 0 to 150                                 |
| `--login`            | log in to your Twitch account, then exit                 |
| `--logout`           | revoke and forget the session, then exit                 |
| `--profile DIR`      | Firefox profile to read the Twitch cookie from           |
| `--anonymous`        | start without the session nor the website token          |
| `--channels-only`    | only the channel lists, see below (alias `--compatibility`) |
| `--check-login`      | report the account and the website token, then exit      |
| `-h`, `--help`       | show help                                                |

### Channels-only mode

```sh
twitch-tui --channels-only
```

Shows nothing but the channels panel: the followed channels with their live
status, and search. There is no playback and no chat, so `ffmpeg` and the
audio player are not needed. `Enter` or `o` opens the selected channel in the
browser. `--compatibility` is an alias.

## Configuration

The config file is `~/.config/twitch-tui/config` (or
`$XDG_CONFIG_HOME/twitch-tui/config`). It is created on first run with every
option commented. Format: one `key = value` per line, `#` for comments.

| Key               | Values                                        | Default    | Purpose                                         |
|-------------------|-----------------------------------------------|------------|-------------------------------------------------|
| `firefox_login`   | `true`, `false`                               | `true`     | read the website token from the browser         |
| `firefox_profile` | path to a profile (`~` allowed)               | none       | only look for the cookie in this profile        |
| `token`           | value of the `auth-token` cookie              | none       | website token used instead of the cookie        |
| `volume`          | 0 to 150                                      | `80`       | initial volume                                  |
| `video_fps`       | 5 to 60                                       | `30`       | video frames per second                         |
| `video_output`    | `auto`, `ascii`, `graphics`                   | `auto`     | how video is drawn                              |
| `video_quality`   | `auto`, `360p`, `720p`, `1080p`, `source`...  | `auto`     | rendition decoded (`auto` aims for 480p)        |
| `visualizer`      | `spectrum`, `mirror`, `scope`, `video`        | `spectrum` | visualizer on startup                           |

The website token is picked in this order of precedence: the `--anonymous`
option, then the `TWITCH_TOKEN` variable, then the `token` key, then the
browser cookie.

On a slow terminal or over SSH, lower `video_fps`: each ASCII frame repaints
the whole panel.

## Key bindings

Press `?` in the program to show the help screen.

| Key              | Action                                                  |
|------------------|---------------------------------------------------------|
| `Tab`            | switch between the channel list and chat                |
| `1`, `2`         | following or search tab                                 |
| arrows, `j` `k`  | move, scroll chat                                       |
| `Enter`          | play the selected channel                               |
| `/`              | filter the followed channels or search results          |
| `s`              | search Twitch channels                                  |
| `i`              | write in chat                                           |
| `Space`          | pause or resume audio                                   |
| `+`, `-`         | volume up or down                                       |
| `m`              | mute                                                    |
| `v`              | cycle visualizer (spectrum, mirror, scope, video)       |
| `a`              | video: real picture or ASCII                            |
| `c`              | cycle video quality                                     |
| `z`              | zoom the picture to the whole window                    |
| `f`              | follow or unfollow the channel                          |
| `o`              | open the channel in the browser                         |
| `r`              | refresh                                                 |
| `q`, `Ctrl+C`    | quit                                                    |

## Twitch restrictions

- **Follow and unfollow**: Twitch's public API cannot do it, and the website's
  API guards these actions with an integrity check reserved to twitch.tv. With
  the website token the program still tries, and when Twitch refuses, press
  `o` to do it in the browser.
- Playback, search and channel pages use the undocumented internal GraphQL
  API of twitch.tv, which can change without notice. The login, followed
  channels and follow state go through the public API.

## Architecture

The repository is a Cargo workspace. The `twitch-tui` binary (`src/`) puts
together independent crates living in `crates/`:

```
crates/
  twitch-core/      HTTP (through curl), JSON, GraphQL and public API clients
  twitch-auth/      device flow login and session file, Firefox cookies
                    (SQLite reader), logged-in account
  twitch-channels/  followed channels, search, channel page, follow
  twitch-playlist/  HLS playlist of a live stream, quality selection
  twitch-chat/      IRC chat
  audio/            ffmpeg to PCM, output through pw-cat / pacat / aplay
  video/            ffmpeg to RGB frames sized to the panel, plus synced PCM
  term/             terminal: raw mode, double-buffered screen, keyboard
src/
  main.rs           main loop and events
  app.rs            application state and logic
  ui.rs             interface rendering
  config.rs         config file and command line
  graphics.rs       kitty graphics protocol
  viz.rs            audio visualizers
  theme.rs, util.rs
```

Dependencies only go one way: `twitch-auth`, `twitch-channels`,
`twitch-playlist` and `twitch-chat` depend on `twitch-core`; `video` depends on
`audio` (for its PCM format); `audio` and `term` depend on nothing; no crate depends on the binary. The workers
(chat, audio, video) report back through an `mpsc::Sender` of the
application's event type.

## Development

```sh
cargo build                  # debug build
cargo test --workspace       # tests of every crate
cargo clippy --workspace     # lints
```
