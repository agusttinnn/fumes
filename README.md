<p align="center"><img src="assets/logo.svg" width="128" height="128" alt="The fumes logo: smoke rising from a glowing vent"></p>

# fumes — a lightweight Steam client for the terminal

A small Steam client in Rust, built on
[steam-vent](https://codeberg.org/steam-vent/steam-vent). It downloads your
games straight from Steam's content servers and runs them with Valve's own
Steam client engine, without Steam's UI, so games get the real Steamworks
API: ownership, achievements, cloud saves, friends and multiplayer.

## Download

Prebuilt fumes, a single program with nothing to install alongside it.
These are built from the latest commit on main, and replaced on every
push:

| Platform | Download |
| --- | --- |
| Windows (x86-64) | [fumes-windows-x86_64.zip](https://github.com/agusttinnn/fumes/releases/download/continuous/fumes-windows-x86_64.zip) |
| macOS (Apple Silicon) | [fumes-macos-arm64.tar.gz](https://github.com/agusttinnn/fumes/releases/download/continuous/fumes-macos-arm64.tar.gz) |
| macOS (Intel) | [fumes-macos-x86_64.tar.gz](https://github.com/agusttinnn/fumes/releases/download/continuous/fumes-macos-x86_64.tar.gz) |
| Linux (x86-64) | [fumes-linux-x86_64.tar.gz](https://github.com/agusttinnn/fumes/releases/download/continuous/fumes-linux-x86_64.tar.gz) |

They're also the [latest release](https://github.com/agusttinnn/fumes/releases/latest);
numbered versions are on the
[releases page](https://github.com/agusttinnn/fumes/releases).
Unpack it and run `fumes` (or `fumes.exe`) from a terminal; with no command
it opens the terminal UI. The builds aren't signed, so macOS blocks a
downloaded one until you clear the quarantine flag
(`xattr -d com.apple.quarantine fumes`), and Windows SmartScreen may ask
you to confirm the first run.

To build it yourself, use `cargo build --release`, or `cargo run --` in
place of `fumes` in the commands below. Builds are made by
`.github/workflows/release.yml`: every push to main updates the
`continuous` release (always marked Latest), and pushing a `v*` tag
publishes that version.

## Usage

```bash
cargo run -- login                     # account name, password, then Steam Guard (code or app approval)
cargo run -- library                   # everything you own; "fumes"/"steam" marks what's installed
cargo run -- library --installed       # only what's on this machine
cargo run -- install "san andreas"     # by app id, exact name, or a unique part of the name
cargo run -- engine setup              # macOS, once: lets games find the engine
cargo run -- launch "san andreas"      # extra game arguments go after `--`
cargo run -- sync "san andreas"        # bring its Steam Cloud saves down without playing
cargo run -- workshop terraria         # the Workshop items you're subscribed to
cargo run -- workshop terraria update  # download new and updated ones now
cargo run -- workshop terraria open    # its Workshop in the browser, to find and subscribe
cargo run -- folder terraria           # open the folder it's installed in
cargo run -- uninstall 12120
cargo run -- logout
```

`cargo run` with no command opens the terminal UI: sections on the left
(library, friends, the store, your profile), the section's items in the
middle, and what you can do with the selected one on the right (play,
install, update, verify, cloud sync, Workshop, open its folder, DLC and
store pages, uninstall).
Arrow keys move, → or Enter goes deeper or runs the action, ← goes back;
`/` searches the library (every word has to be in the name; initials
like "gta" work too), `f` stars a game as a favorite so it's listed first,
`r` refreshes from Steam and Page Up/Down scroll. Actions run the same commands as below, with the terminal handed
over until they finish.

Friends come from your Steam account, over a connection the UI keeps open
(it shows you as online while it runs): who's online and what they're
playing, and friend requests you've received (accept or decline) or sent
(cancel). When a friend's game can be joined, **Join game** starts it
with their session on the command line, the way Steam's "Join Game"
does. Games run in the background: the UI stays up while you play (the
bottom line shows what the game's session is doing, and a cloud-save
conflict is asked about in the UI), so you can message friends and
**Invite to your game**: fumes sends them your session (the game's
`connect` rich presence, or its lobby) as Steam does. Invites friends send
you get **Accept game invite**. fumes waits for a running game to close
before it quits, so its saves upload. Pick a friend to see your recent messages with them and send new
ones (text only). `/` and `f` search and favorite friends the same way
as games; favorites are kept in `favorite_friends.json`.

Installs, updates and verifies download in the background too: the UI
stays usable, the bottom line shows how far along the download is, and
the game is marked ↓ in the library. Downloads go one at a time; starting
another queues it. **Stop download** stops one (or takes it out of the
queue), and quitting mid-download asks first. Either way, starting it
again picks up where it left off. **Cloud sync** runs in the background
as well: the bottom line shows it going, and then whether the saves are
in sync (a conflict is asked about as when playing). Other commands the
UI runs (sign in, uninstall) take the terminal, come back to the UI by themselves when they
work, and wait for Enter when they don't.

Workshop items work as in Steam: you find and subscribe to them on the
game's Workshop pages (**Workshop ↗** opens them in the browser), and
before each launch fumes downloads new and updated subscribed items, and
removes the ones you've unsubscribed from. **Update Workshop items** does
that now, in the background. Items go where Steam keeps them
(`steamapps/workshop/content/<game>/<item>`), recorded in Steam's
`appworkshop_<game>.acf`, so the game finds them through the Steamworks
API like it would under Steam. If Steam can't be reached, the game starts
with the items already there.

Running `install` again updates the game or repairs it, and only downloads
what changed. `--verify` re-checks every file instead of trusting the last
install. `install --steam` and `launch --steam` hand the job to the
official client instead.

On Windows, games also need what Steam calls first-time setup: runtimes
such as DirectX, Visual C++ and .NET (without them a game stops at
errors like "d3dx9_42.dll was not found"), plus registry values and
default files. fumes downloads the shared runtimes a game uses into
`steamapps/common/Steamworks Shared`, as Steam does, and before a launch
runs whatever the game's install scripts still need. Installers need
administrator rights, so Windows asks for permission once; what's done
is recorded where Steam records it, so nothing is installed twice, by
either. Games installed by an earlier fumes get the shared runtimes after
one **Update**.

## What talks to what

- **Sign-in and the owned-games list** go straight to Steam's servers through
  steam-vent. Only the refresh token is stored (owner-readable only, in the
  platform data folder); the password never touches disk. The last list is
  cached, so `library --offline` and `launch` don't need the network.
- **Downloads** use the same protocol as the Steam client. fumes reads the
  game's depots from PICS (app info), picks the ones for your platform,
  language and owned DLC, asks Steam for each depot's key and a manifest
  request code, then fetches manifests and chunks from the content servers.
  Chunks are decrypted (AES-256), decompressed (LZMA, Zstandard or zip) and
  checked (size, Adler-32, SHA-1 against what's already on disk). Steam only
  hands out keys for depots the account owns. Games go to
  `<data folder>/library/steamapps/common/<game>`, a Steam-style library
  (`FUMES_LIBRARY` moves it; `--dir` installs anywhere), with Steam's
  `appmanifest_<id>.acf` next to them.
- **Playing** a game fumes installed starts the Steam engine first: fumes
  downloads just the engine from Valve's client packages (about 100 MB on
  macOS instead of ~400 MB for the whole client; pinned to one client
  build, see `manifests/`), loads it, signs it in with your saved session,
  and registers it where games look for a running Steam. Then the engine
  launches the game exactly as Steam's Play button does: Steam Cloud saves
  come down first and go up after it quits, with Steam's environment and
  playtime. If local and cloud saves both changed, fumes asks which to
  keep. Ctrl-C reaches the game; fumes waits for it, and the upload,
  before signing out. Games the engine doesn't list as installed (Windows
  builds under Wine, or any game on Linux for now) are started directly,
  without cloud sync.
- **Games installed by the official client** still launch through
  `steam://` links. fumes reads those installs from Steam's
  `libraryfolders.vdf` and `appmanifest_*.acf` (`FUMES_STEAM_DIR` if Steam
  isn't in the default place).

[docs/steam-engine.md](docs/steam-engine.md) explains how the engine is
hosted, how games find it on each platform, and how to move the pinned
client build forward.

## The engine commands

```bash
cargo run -- engine status             # pinned build, downloaded?, can games find it?
cargo run -- engine fetch              # download it now instead of on first launch
cargo run -- engine setup              # macOS: install Steam's ipctool launchd agent
cargo run -- engine remove             # undo setup
cargo run -- engine run                # stay signed in until Ctrl-C, for games started by hand
cargo run -- engine test --lib <libsteam_api>   # anonymous login + a game library attaching
```

On macOS games find the engine through a launchd agent that Steam itself
installs (`com.valvesoftware.steam.ipctool`, which runs Valve's small
`ipcserver` on demand); `engine setup` installs it, pointing at fumes' copy.
It replaces a real Steam's agent if one is installed. On Linux and Windows
nothing is installed: while the engine runs fumes sets `~/.steam/steam.pid`
and `~/.steam/sdk{32,64}` (Linux) or the `ActiveProcess` registry values
(Windows), and puts back what was there when it stops.

## Limits

- **Newer games need a newer engine.** Games built with a Steamworks SDK
  newer than the pinned client build may ask for interfaces it lacks; the
  pin then needs a bump (see the docs).
- **Windows builds under Wine** (on macOS or Linux) can't reach the native
  engine, so only games that don't need Steamworks run that way.
- **Not yet tested:** signing in with a real account through the engine,
  and Linux and Windows at runtime (the code type-checks for both).
- On Linux the engine doesn't list fumes' games as installed yet (where it
  keeps its data there isn't settled), so games start directly, without
  cloud sync on launch.
- Beta branches and password-protected branches (only `public` is used).
- First-time setup is Windows-only: Windows builds under Wine don't get
  Steam's runtime installers (Wine has its own DirectX).
- QR-code sign-in (steam-vent doesn't support it yet), and a GUI.

## Notes

- steam-vent is pinned to a Codeberg commit rather than the crates.io
  release: refresh-token login only exists in its unreleased 0.6. At that
  commit anonymous logons are broken, which fumes doesn't use.
- steam-vent's README asks that contributions not be LLM-driven; fumes only
  depends on it.
- The engine is Valve's: fumes downloads it from Valve's servers at runtime
  and doesn't redistribute it. The interface layouts come from
  [OpenSteamworks](https://github.com/OpenSteamClient/OpenSteamworks)
  (MIT), which hosts the same engine for OpenSteamClient.
- Third-party clients are a grey area under the Steam Subscriber Agreement.
  fumes only downloads what the account owns.

```bash
cargo test                      # offline tests (synthetic VDF, manifest and chunk fixtures)
cargo test -- --include-ignored # plus a live handshake with Steam's servers
```

`tests/fixtures/gen.py` regenerates the chunk and manifest fixtures with
tools independent of fumes (`openssl`, Python's lzma/zipfile/zstd).
