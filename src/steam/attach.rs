//! Act like a game: load a real Steamworks library and initialise it, the
//! way any game does at startup. `fumes engine test` runs this in a separate
//! process while the engine is up.

use std::ffi::{CStr, c_char, c_void};
use std::path::Path;

use anyhow::{Context, Result, bail};
use libloading::Library;

/// Steamworks' `SteamErrMsg`.
type ErrMsg = [c_char; 1024];

pub fn attach(lib: &Path, appid: u32) -> Result<()> {
    // What the Steam client sets for the games it starts.
    // SAFETY: single-threaded at this point.
    unsafe {
        std::env::set_var("SteamAppId", appid.to_string());
        std::env::set_var("SteamGameId", appid.to_string());
    }
    let lib = unsafe { Library::new(lib) }.with_context(|| format!("loading {}", lib.display()))?;
    unsafe {
        let is_running: libloading::Symbol<unsafe extern "C" fn() -> bool> =
            lib.get(b"SteamAPI_IsSteamRunning\0")?;
        println!("SteamAPI_IsSteamRunning() = {}", is_running());

        let init: libloading::Symbol<unsafe extern "C" fn(*mut ErrMsg) -> i32> =
            lib.get(b"SteamAPI_InitFlat\0")?;
        let mut err: ErrMsg = [0; 1024];
        let result = init(&mut err);
        let message = CStr::from_ptr(err.as_ptr()).to_string_lossy();
        println!(
            "SteamAPI_InitFlat() = {result} {message:?}  \
             (0 = OK, 1 = failed, 2 = no Steam client, 3 = version mismatch)"
        );
        if result != 0 {
            bail!("SteamAPI_InitFlat failed: {message}");
        }

        let user = accessor(&lib, "SteamUser")?;
        let steam_id: unsafe extern "C" fn(*mut c_void) -> u64 =
            *lib.get(b"SteamAPI_ISteamUser_GetSteamID\0")?;
        let friends = accessor(&lib, "SteamFriends")?;
        let persona: unsafe extern "C" fn(*mut c_void) -> *const c_char =
            *lib.get(b"SteamAPI_ISteamFriends_GetPersonaName\0")?;
        let apps = accessor(&lib, "SteamApps")?;
        let subscribed: unsafe extern "C" fn(*mut c_void, u32) -> bool =
            *lib.get(b"SteamAPI_ISteamApps_BIsSubscribedApp\0")?;
        let shutdown: unsafe extern "C" fn() = *lib.get(b"SteamAPI_Shutdown\0")?;

        println!("SteamID: {}", steam_id(user()));
        println!(
            "persona: {:?}",
            CStr::from_ptr(persona(friends())).to_string_lossy()
        );
        println!("owns app {appid}: {}", subscribed(apps(), appid));
        shutdown();
        println!("SteamAPI_Shutdown() done");
    }
    Ok(())
}

type Accessor = unsafe extern "C" fn() -> *mut c_void;

/// `SteamAPI_<name>_vNNN`, whichever version this SDK exports (they move
/// between SDK releases).
fn accessor(lib: &Library, name: &str) -> Result<Accessor> {
    (1..=40)
        .rev()
        .find_map(|v| unsafe {
            lib.get::<Accessor>(format!("SteamAPI_{name}_v{v:03}\0").as_bytes())
                .ok()
                .map(|f| *f)
        })
        .with_context(|| format!("no SteamAPI_{name}_vNNN export"))
}
