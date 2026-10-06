# fumes

A small Steam library client in Rust, built on
[steam-vent](https://codeberg.org/steam-vent/steam-vent). It downloads your
games straight from Steam's content servers and, where it can, runs them
without the Steam client by swapping in the
[gbe_fork](https://github.com/Detanup01/gbe_fork) Steamworks emulator.

```bash
cargo run -- login                     # account name, password, then Steam Guard (code or app approval)
cargo run -- library                   # everything you own; "fumes"/"steam" marks what's installed
cargo run -- library --installed       # only what's on this machine
cargo run -- install "san andreas"     # by app id, exact name, or a unique part of the name
cargo run -- install 12120 --os windows
cargo run -- launch "san andreas"      # extra game arguments go after `--`
cargo run -- emu disable 12120         # put Steam's own libraries back
cargo run -- uninstall 12120
cargo run -- logout
```

Running `install` again updates the game or repairs it, and only downloads
what changed. `--verify` re-checks every file instead of trusting the last
install. `install --steam` and `launch --steam` hand the job to the
official client instead.

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
  `<data folder>/library/<game>` unless you pass `--dir` or set
  `FUMES_LIBRARY`.
- **Launching** a game fumes installed runs its executable directly, using
  the launch options from app info, with `SteamAppId`/`SteamGameId` set the
  way Steam sets them. On macOS and Linux, Windows builds run through Wine:
  fumes looks at `FUMES_WINE`, then `wine`/`wine64` on `PATH`, then Wine
  Stable and CrossOver in `/Applications`. Games installed by the Steam
  client still launch through `steam://` links. fumes reads those installs
  from Steam's `libraryfolders.vdf` and `appmanifest_*.acf`
  (`FUMES_STEAM_DIR` if Steam isn't in the default place).

## The Steamworks emulator

Most games load `steam_api(64).dll` (or `libsteam_api.so`) and quit if
there's no Steam client to talk to. After an install, fumes:

1. downloads gbe_fork's latest release from GitHub, checks it against
   GitHub's SHA-256, and caches it (`fumes emu update` refreshes it;
   `FUMES_GBE_DIR` points at an unpacked release instead);
2. replaces each `steam_api` library in the game with gbe_fork's build of
   the same architecture, keeping the original as `<name>.fumes-orig`;
3. writes `steam_settings/` next to it: the app id, your SteamID, your
   display name (never your login name), the game language, **only the DLC
   your account owns** (`unlock_all=0`), the installed depots, and
   `steam_interfaces.txt` generated from the original library the way
   gbe_fork's `generate_interfaces` tool does it.

`fumes emu disable` restores the originals and removes what fumes created.
`install --no-emu` skips the emulator.

What won't work:

- **Native macOS builds.** gbe_fork only exists for Windows and Linux, so
  `libsteam_api.dylib` can't be replaced. On a Mac, use
  `install --os windows` and run the game through Wine.
- **DRM on top of Steamworks.** SteamStub (an encrypted wrapper around the
  executable), Denuvo and similar check for the real client before the
  emulator loads. fumes warns when an executable carries SteamStub; it
  doesn't remove DRM.
- **Steam's online services.** Matchmaking, Workshop, cloud saves, and
  achievements or stats on your profile aren't available. gbe_fork keeps
  saves locally (`GSE Saves`) and does multiplayer over LAN by
  broadcasting your display name and SteamID on the local network.
- Games that need a launcher or anti-cheat service to run.

DRM-free games, and games whose only Steam dependency is Steamworks, are
the ones that run well.

## Not yet

- Beta branches and password-protected branches (only `public` is used).
- Redistributables Steam installs once per machine (`sharedinstall`
  depots such as DirectX or VC++ runtimes). Under Wine, install them in the
  prefix yourself (for example with winetricks).
- gbe_fork's `steamclient` loader (ColdClientLoader) for games that don't
  work with the plain library swap, and achievement/stat schemas.
- QR-code sign-in (steam-vent doesn't support it yet).
- A GUI.

## Notes

- steam-vent is pinned to a Codeberg commit rather than the crates.io
  release: refresh-token login only exists in its unreleased 0.6. At that
  commit anonymous logons are broken, which fumes doesn't use.
- steam-vent's README asks that contributions not be LLM-driven; fumes only
  depends on it.
- gbe_fork is LGPL-3.0. fumes downloads its official release at runtime and
  doesn't redistribute it.
- Third-party clients are a grey area under the Steam Subscriber Agreement,
  and so is running games through an emulator. fumes only downloads what
  the account owns, and only reports DLC the account owns.

```bash
cargo test                      # offline tests (synthetic VDF, manifest and chunk fixtures)
cargo test -- --include-ignored # plus a live handshake with Steam's servers and
                                # unpacking real gbe_fork archives (FUMES_GBE_ARCHIVES)
```

`tests/fixtures/gen.py` regenerates the chunk and manifest fixtures with
tools independent of fumes (`openssl`, Python's lzma/zipfile/zstd).
