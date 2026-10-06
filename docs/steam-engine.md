# The Steam engine

Games built on Steamworks load `steam_api`, which connects to a running
Steam client engine (`steamclient`). Steam's UI is one host for that
engine; fumes is another. This is how it works and how to update it.

## What fumes downloads

Valve ships the client as zip packages listed in per-platform manifests
(`client-update.steamstatic.com/steam_client_<platform>`). fumes takes only
the engine and the few libraries it needs, from the manifests pinned in
`manifests/`, and checks each package against the manifest's SHA-256.
Valve keeps old packages on its CDN, so a pinned build stays fetchable.

| Platform | Package | Files |
|---|---|---|
| macOS | `bins_client_osx` | `steamclient.dylib` |
| | `bins_osx` | `libtier0_s`, `libvstdlib_s`, `libaudio`, `steamservice`, `crashhandler`, `ipcserver`, `libSDL3` (controllers), `libvideo` (voice chat) |
| | `bins_codecs_osx` | the ffmpeg and Vorbis libraries `libvideo` links |
| | `breakpad_osx` | `Breakpad.framework` (the whole bundle: its code signature covers it) |
| Linux | `bins_sdk_ubuntu12` | `linux64/steamclient.so`, `linux64/crashhandler.so`, and `linux32/` for 32-bit games |
| Windows | `bins_win64` | `steamclient64.dll`, `tier0_s64`, `vstdlib_s64`, `crashhandler64`, and the 32-bit set for 32-bit games |

About 100 MB on macOS, versus ~400 MB for the whole client. On Linux and
Windows the engine's optional `SDL3` (controllers) and `video` (voice chat)
libraries aren't fetched yet; the engine runs without them and says so.

## Hosting it

`steamclient` exports `CreateInterface`; asking it for
`CLIENTENGINE_INTERFACE_VERSION005` returns the engine (`IClientEngine`),
the entry point Steam's UI uses. `CreateGlobalUser` makes the process the
Steam client. fumes then calls `SetLoginToken(refresh token, account)` and
`LogOn(steamid)` on `IClientUser`, and calls `RunFrame` and drains
callbacks (`Steam_BGetCallback`) every 15 ms. The engine runs its own
threads for everything else.

The engine's interfaces are C++ classes; fumes calls their virtual methods
by vtable slot (`src/steam/engine.rs`). Slots come from
[OpenSteamworks](https://github.com/OpenSteamClient/OpenSteamworks)
(`cpp/include/steamclient`). No overloaded or destructor slots come before
the ones used, so MSVC (Windows) lays them out like the Itanium ABI.

## Launching and cloud saves

`fumes launch` drives what Steam's client does around a launch:

1. `LoadLocalFileInfoCache`: the engine's "init" for the app's cloud
   files. Nothing else cloud-related works for an app before it (an
   evaluation started earlier just never finishes).
2. `SynchronizeApp(down, AutoCloud launch)`: the cloud's saves come down.
   If a file changed on both sides the engine marks a conflict and
   overwrites nothing; fumes asks which side to keep
   (`ResolveSyncConflict`) and syncs again.
3. `IClientAppManager::LaunchApp`, the call behind Steam's Play button:
   the engine starts the game with Steam's environment and tracks it and
   its playtime. fumes waits for the app's `AppRunning` state to appear
   and clear.
4. `SynchronizeApp(up, AutoCloud exit)`: saves go up.

The engine's `LaunchApp` alone doesn't sync cloud saves (in Steam that's
orchestrated by the UI), and it refuses to launch until the account's
licenses have arrived after login (`LicensesUpdated_t`, callback 125).
The engine also needs to list the game as installed: fumes writes Steam's
`appmanifest_<id>.acf` and adds its library to the engine's
`libraryfolders.vdf` (macOS for now).

## How games find it

| Platform | The game's `steam_api` looks for | fumes |
|---|---|---|
| macOS | Mach service `com.valvesoftware.steam.ipctool`, served by Valve's `ipcserver` and declared to launchd by `~/Library/LaunchAgents/com.valvesoftware.steam.ipctool.plist`. It asks `ipcserver` for the client's path and pid (command 14) and loads `steamclient.dylib` from that path's folder. | `fumes engine setup` installs the agent once. Each run registers the engine with `ipcserver` (command 13: pid + path). |
| Linux | `~/.steam/steam.pid` naming a live process; loads `~/.steam/sdk64/steamclient.so` (`sdk32` for 32-bit games) | Set while the engine runs, put back as it was afterwards |
| Windows | `HKCU\Software\Valve\Steam\ActiveProcess`: `pid`, `SteamClientDll64`, `SteamClientDll` | Same |

`ipcserver`'s protocol (from Valve's `libsteam_api` and `ipcserver`):
Mach messages with `msgh_id` = 0x68 (protocol version) and the command
right after the 24-byte header. Command 13 (`SetSteamPath`) carries the pid
at 0x1c and a path at 0x20 (512 bytes); it's ignored while a live pid is
registered and has no reply. Command 14 (`GetSteamPath`) answers only while
that pid is alive and the path is readable, so nothing needs undoing when
the engine exits.

## What's verified

On macOS (Apple Silicon), with client build 1788652215:

- The engine loads, becomes the client, logs in (anonymous account) and
  logs off cleanly.
- A separate process using a real `libsteam_api.dylib` (Steamworks SDKs
  from mid-2025 and 2026) finds it, initialises (`SteamAPI_InitFlat` OK),
  gets a SteamID and app ownership from it. `fumes engine test --lib …`
  repeats this.

Not yet verified: logging in with a real account; Linux and Windows at
runtime (the code type-checks for both; Linux has no 64-bit `steamservice`,
which OpenSteamClient replaces with its own); a real game end to end.

## Verifying slots for a build

OpenSteamworks' slot numbers are for client build 1745623383. For another
build, compare the two builds' IPC method tables, which list each
interface's methods in vtable order with their argument and return types:

1. Build [steamworks_dumper](https://github.com/Rosentti/steamworks_dumper)
   (OpenSteamClient's dumper). On macOS it needs `cmake`, `capstone`,
   `pkgconf`, musl's `elf.h` on the include path, and two fixes: a bounds
   check in `ModuleImage::UpdatePltSymbols` and clamping
   `ClientModule::GetFunctionSize` to the image (it reads past the end of
   the last function).
2. Dump `ubuntu12_32/steamclient.so` (package `bins_ubuntu12`) of both
   builds.
3. Align each interface's method list by signature (`difflib` does it).
   A slot is safe to reuse if it falls in an unchanged run whose
   signatures match OpenSteamworks' declarations.

For 1788652215: `IClientUser` slots used are unchanged (and work),
`IClientRemoteStorage` slots 0-84 and `IClientAppManager` slots 0-32 are
unchanged. At runtime fumes also checks each interface object's C++ class
name (RTTI) before calling into it, so a moved accessor fails safely.

## Updating the pinned build

Games built with a newer Steamworks SDK can need interfaces an older engine
lacks (`SteamClient023` arrived after mid-2025), so the pin needs an
occasional bump:

1. Replace `manifests/steam_client_{osx,ubuntu12,win64}.vdf` with the
   current ones (same build for all three).
2. Check the slots in `src/steam/engine.rs` as described above, then run
   `fumes engine test --lib <newest libsteam_api>`.
3. Recheck each platform's file list (`otool -L`, `objdump -p`) and update
   `src/steam/runtime.rs` if the engine links something new.
4. On macOS, run `fumes engine setup` again so the agent points at the new
   build's `ipcserver`.
