//! Calling into Valve's Steam client engine (`steamclient.dylib`).
//!
//! The library exports `CreateInterface`; asking it for
//! `CLIENTENGINE_INTERFACE_VERSION005` returns the engine object
//! (`IClientEngine`), the same entry point Steam's own UI uses. Its methods
//! and those of the objects it hands out (`IClientUser`, …) are C++ virtual
//! functions: a call is "read the vtable, take slot N, call it with the
//! object as the first argument". Slot numbers come from OpenSteamworks'
//! headers (github.com/OpenSteamClient/OpenSteamworks,
//! cpp/include/steamclient), which match client build 1745623383 — the
//! build `fetch` downloads. A different build can move them. No overloaded
//! or destructor slots come before the ones used, so MSVC (Windows) lays
//! them out the same as the Itanium ABI (macOS, Linux).

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;

use anyhow::{Context, Result, bail};
use libloading::Library;

// The slots and the plain C calling convention for methods hold for 64-bit
// builds (Itanium ABI on macOS/Linux, MSVC x64 on Windows); 32-bit Windows
// would need `thiscall`.
#[cfg(not(target_pointer_width = "64"))]
compile_error!("the engine spike only supports 64-bit hosts");

pub type HSteamPipe = i32;
pub type HSteamUser = i32;

const CLIENTENGINE_INTERFACE: &CStr = c"CLIENTENGINE_INTERFACE_VERSION005";

/// IClientEngine vtable slots.
mod engine_slot {
    pub const CREATE_GLOBAL_USER: usize = 2;
    pub const RELEASE_USER: usize = 6;
    pub const GET_ICLIENT_USER: usize = 8;
    pub const GET_UNIVERSE_NAME: usize = 12;
    pub const RUN_FRAME: usize = 19;
    pub const GET_ICLIENT_REMOTE_STORAGE: usize = 24;
    pub const GET_ICLIENT_APP_MANAGER: usize = 43;
    pub const BRELEASE_STEAM_PIPE: usize = 1;
}

/// IClientRemoteStorage vtable slots. OpenSteamworks maps them for build
/// 1745623383; aligning that build's IPC method table with 1788652215's (by
/// argument and return types) shows slots 0-84 unchanged, so these hold.
/// The object is also checked by class name before use.
mod storage_slot {
    pub const IS_CLOUD_ENABLED_FOR_ACCOUNT: usize = 23;
    pub const IS_CLOUD_ENABLED_FOR_APP: usize = 24;
    pub const LOAD_LOCAL_FILE_INFO_CACHE: usize = 70;
    pub const GET_SYNC_STATE: usize = 73;
    pub const RESOLVE_SYNC_CONFLICT: usize = 77;
    pub const SYNCHRONIZE_APP: usize = 78;
    pub const IS_APP_SYNC_IN_PROGRESS: usize = 79;
}

/// Which way to sync (`ERemoteStorageSyncType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncDirection {
    Down = 1,
    Up = 2,
}

/// `ERemoteStorageSyncFlags` Steam passes around a launch.
const SYNC_FLAGS_LAUNCH: u64 = 2;
const SYNC_FLAGS_EXIT: u64 = 4;

/// IClientAppManager vtable slots, verified the same way (slots 0-32 are
/// unchanged between the two builds, with matching signatures).
mod app_slot {
    pub const LAUNCH_APP: usize = 2;
    pub const GET_APP_INSTALL_STATE: usize = 4;
}

/// `EAppState` flags.
pub const APP_FULLY_INSTALLED: u32 = 1 << 2;
pub const APP_RUNNING: u32 = 1 << 13;

/// `ELaunchSource`: Play in the library's game page, what a click on Play
/// in Steam sends.
const LAUNCH_SOURCE_LIBRARY: u32 = 100;

/// The C++ class name the compiler recorded for `object` (RTTI), used to
/// check that a slot handed back the object expected before calling into
/// it: a moved slot shows up as the wrong class instead of a wrong call.
pub fn class_name(object: *mut c_void) -> Option<String> {
    if object.is_null() {
        return None;
    }
    unsafe {
        let vtable = *(object as *const *const *const c_void);
        #[cfg(not(windows))]
        {
            // Itanium: vtable[-1] is the type_info; its second word is the
            // mangled name ("20IClientRemoteStorage").
            let type_info = *vtable.sub(1) as *const *const c_char;
            let name = *type_info.add(1);
            Some(CStr::from_ptr(name).to_string_lossy().into_owned())
        }
        #[cfg(windows)]
        {
            // MSVC x64: vtable[-1] is the complete object locator, whose
            // offsets are relative to the module's base.
            #[repr(C)]
            struct Locator {
                signature: u32,
                offset: u32,
                cd_offset: u32,
                type_descriptor: i32,
                class_descriptor: i32,
                this: i32,
            }
            let locator = *vtable.sub(1) as *const Locator;
            if (*locator).signature != 1 {
                return None;
            }
            let base = locator as usize - (*locator).this as usize;
            // TypeDescriptor: vftable pointer, spare pointer, then the name
            // (".?AVCClientRemoteStorage@@").
            let name = (base + (*locator).type_descriptor as usize + 16) as *const c_char;
            Some(CStr::from_ptr(name).to_string_lossy().into_owned())
        }
    }
}

/// IClientUser vtable slots (OpenSteamworks' "index" minus one).
mod user_slot {
    pub const LOG_ON: usize = 1;
    pub const LOG_OFF: usize = 3;
    pub const BLOGGED_ON: usize = 4;
    pub const GET_LOGON_STATE: usize = 5;
    pub const BCONNECTED: usize = 6;
    pub const SET_LOGIN_INFORMATION: usize = 54;
    pub const SET_LOGIN_TOKEN: usize = 56;
}

/// Steamworks' `CallbackMsg_t`.
#[repr(C)]
struct CallbackMsg {
    user: HSteamUser,
    callback: c_int,
    param: *const u8,
    param_len: c_int,
}

type CreateInterfaceFn = unsafe extern "C" fn(*const c_char, *mut c_int) -> *mut c_void;
type BGetCallbackFn = unsafe extern "C" fn(HSteamPipe, *mut CallbackMsg) -> bool;
type FreeLastCallbackFn = unsafe extern "C" fn(HSteamPipe);

/// Function pointer at `slot` of `object`'s vtable.
///
/// # Safety
/// `object` must be a live C++ object whose vtable has at least `slot + 1`
/// entries, and `F` must match that method's signature.
unsafe fn vfn<F: Copy>(object: *mut c_void, slot: usize) -> F {
    unsafe {
        let vtable = *(object as *const *const *const c_void);
        let entry = *vtable.add(slot);
        std::mem::transmute_copy::<*const c_void, F>(&entry)
    }
}

pub struct Engine {
    /// Never unloaded: the engine's own threads outlive logging off, and
    /// unloading the library under them crashes the process.
    _lib: std::mem::ManuallyDrop<Library>,
    engine: *mut c_void,
    get_callback: BGetCallbackFn,
    free_callback: FreeLastCallbackFn,
    pub pipe: HSteamPipe,
    pub user: HSteamUser,
}

impl Engine {
    /// Load the engine library at `path` and get the engine object.
    pub fn load(path: &Path) -> Result<Engine> {
        if !path.is_file() {
            bail!("{} not found; run `fetch` first", path.display());
        }
        // SAFETY: loading Valve's library runs its initialisers; that's the
        // point of the spike.
        let lib = unsafe { open(path) }.with_context(|| format!("loading {}", path.display()))?;
        unsafe {
            let create: CreateInterfaceFn = *lib.get(b"CreateInterface\0")?;
            let get_callback: BGetCallbackFn = *lib.get(b"Steam_BGetCallback\0")?;
            let free_callback: FreeLastCallbackFn = *lib.get(b"Steam_FreeLastCallback\0")?;
            let mut error: c_int = 0;
            let engine = create(CLIENTENGINE_INTERFACE.as_ptr(), &mut error);
            if engine.is_null() {
                bail!("CreateInterface({CLIENTENGINE_INTERFACE:?}) failed (code {error})");
            }
            Ok(Engine {
                _lib: std::mem::ManuallyDrop::new(lib),
                engine,
                get_callback,
                free_callback,
                pipe: 0,
                user: 0,
            })
        }
    }

    /// Become the Steam client: create the global user that games and other
    /// processes connect to.
    pub fn create_global_user(&mut self) -> Result<()> {
        let mut pipe: HSteamPipe = 0;
        let user = unsafe {
            let f: unsafe extern "C" fn(*mut c_void, *mut HSteamPipe) -> HSteamUser =
                vfn(self.engine, engine_slot::CREATE_GLOBAL_USER);
            f(self.engine, &mut pipe)
        };
        if pipe == 0 || user == 0 {
            bail!("CreateGlobalUser failed (pipe {pipe}, user {user})");
        }
        self.pipe = pipe;
        self.user = user;
        Ok(())
    }

    pub fn universe_name(&self, universe: c_int) -> String {
        unsafe {
            let f: unsafe extern "C" fn(*mut c_void, c_int) -> *const c_char =
                vfn(self.engine, engine_slot::GET_UNIVERSE_NAME);
            let name = f(self.engine, universe);
            if name.is_null() {
                return String::new();
            }
            CStr::from_ptr(name).to_string_lossy().into_owned()
        }
    }

    pub fn client_user(&self) -> Result<ClientUser> {
        let object = unsafe {
            let f: unsafe extern "C" fn(*mut c_void, HSteamUser, HSteamPipe) -> *mut c_void =
                vfn(self.engine, engine_slot::GET_ICLIENT_USER);
            f(self.engine, self.user, self.pipe)
        };
        if object.is_null() {
            bail!("GetIClientUser returned null");
        }
        tracing::debug!(class = class_name(object), "IClientUser");
        Ok(ClientUser(object))
    }

    /// Installing and launching apps. Checked by class name like
    /// `remote_storage`.
    pub fn app_manager(&self) -> Result<AppManager> {
        let object = unsafe {
            let f: unsafe extern "C" fn(*mut c_void, HSteamUser, HSteamPipe) -> *mut c_void =
                vfn(self.engine, engine_slot::GET_ICLIENT_APP_MANAGER);
            f(self.engine, self.user, self.pipe)
        };
        let class = class_name(object);
        tracing::debug!(?class, "IClientAppManager");
        match &class {
            Some(name) if name.contains("AppManager") => Ok(AppManager(object)),
            _ => bail!(
                "this engine build's app interface isn't where fumes expects it (found {class:?})"
            ),
        }
    }

    /// Cloud saves. Refuses unless the object really is the remote storage
    /// one, so a moved slot can't turn into a wrong call on someone's saves.
    pub fn remote_storage(&self) -> Result<RemoteStorage> {
        let object = unsafe {
            let f: unsafe extern "C" fn(*mut c_void, HSteamUser, HSteamPipe) -> *mut c_void =
                vfn(self.engine, engine_slot::GET_ICLIENT_REMOTE_STORAGE);
            f(self.engine, self.user, self.pipe)
        };
        let class = class_name(object);
        tracing::debug!(?class, "IClientRemoteStorage");
        match &class {
            Some(name) if name.contains("RemoteStorage") => Ok(RemoteStorage(object)),
            _ => bail!(
                "this engine build's cloud interface isn't where fumes expects it (found {class:?}); \
                 cloud saves are off until fumes is updated for it"
            ),
        }
    }

    pub fn run_frame(&self) {
        unsafe {
            let f: unsafe extern "C" fn(*mut c_void) = vfn(self.engine, engine_slot::RUN_FRAME);
            f(self.engine)
        }
    }

    /// Hand every pending callback (id, payload) to `f`.
    pub fn drain_callbacks(&self, mut f: impl FnMut(i32, &[u8])) {
        loop {
            let mut msg = CallbackMsg {
                user: 0,
                callback: 0,
                param: std::ptr::null(),
                param_len: 0,
            };
            if !unsafe { (self.get_callback)(self.pipe, &mut msg) } {
                return;
            }
            let payload = if msg.param.is_null() || msg.param_len <= 0 {
                &[][..]
            } else {
                unsafe { std::slice::from_raw_parts(msg.param, msg.param_len as usize) }
            };
            f(msg.callback, payload);
            unsafe { (self.free_callback)(self.pipe) };
        }
    }

    pub fn release(&mut self) {
        if self.pipe == 0 {
            return;
        }
        unsafe {
            let release_user: unsafe extern "C" fn(*mut c_void, HSteamPipe, HSteamUser) =
                vfn(self.engine, engine_slot::RELEASE_USER);
            release_user(self.engine, self.pipe, self.user);
            let release_pipe: unsafe extern "C" fn(*mut c_void, HSteamPipe) -> bool =
                vfn(self.engine, engine_slot::BRELEASE_STEAM_PIPE);
            release_pipe(self.engine, self.pipe);
        }
        self.pipe = 0;
        self.user = 0;
    }
}

/// macOS and Linux find the engine's own libraries next to it
/// (`@loader_path`, `$ORIGIN`). Windows only does with an absolute path and
/// LOAD_WITH_ALTERED_SEARCH_PATH.
unsafe fn open(path: &Path) -> Result<Library, libloading::Error> {
    #[cfg(windows)]
    unsafe {
        const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 0x8;
        libloading::os::windows::Library::load_with_flags(path, LOAD_WITH_ALTERED_SEARCH_PATH)
            .map(Into::into)
    }
    #[cfg(not(windows))]
    unsafe {
        Library::new(path)
    }
}

pub struct ClientUser(*mut c_void);

impl ClientUser {
    pub fn set_login_token(&self, token: &str, account: &str) -> Result<()> {
        let token = CString::new(token)?;
        let account = CString::new(account)?;
        unsafe {
            let f: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) =
                vfn(self.0, user_slot::SET_LOGIN_TOKEN);
            f(self.0, token.as_ptr(), account.as_ptr());
        }
        Ok(())
    }

    /// Legacy name/password login; only used here for the anonymous account.
    pub fn set_login_information(&self, account: &str, password: &str) -> Result<()> {
        let account = CString::new(account)?;
        let password = CString::new(password)?;
        unsafe {
            let f: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, bool) =
                vfn(self.0, user_slot::SET_LOGIN_INFORMATION);
            f(self.0, account.as_ptr(), password.as_ptr(), false);
        }
        Ok(())
    }

    /// Start logging on; returns an EResult (1 = OK, meaning "started").
    pub fn log_on(&self, steam_id: u64) -> i32 {
        unsafe {
            let f: unsafe extern "C" fn(*mut c_void, u64) -> i32 = vfn(self.0, user_slot::LOG_ON);
            f(self.0, steam_id)
        }
    }

    pub fn log_off(&self) {
        unsafe {
            let f: unsafe extern "C" fn(*mut c_void) = vfn(self.0, user_slot::LOG_OFF);
            f(self.0)
        }
    }

    pub fn logged_on(&self) -> bool {
        unsafe {
            let f: unsafe extern "C" fn(*mut c_void) -> bool = vfn(self.0, user_slot::BLOGGED_ON);
            f(self.0)
        }
    }

    pub fn connected(&self) -> bool {
        unsafe {
            let f: unsafe extern "C" fn(*mut c_void) -> bool = vfn(self.0, user_slot::BCONNECTED);
            f(self.0)
        }
    }

    /// ELogonState: 0 logged off, 1 connecting, 2 connected, 3 logging on,
    /// 4 logged on, 5 logging off.
    pub fn logon_state(&self) -> i32 {
        unsafe {
            let f: unsafe extern "C" fn(*mut c_void) -> i32 =
                vfn(self.0, user_slot::GET_LOGON_STATE);
            f(self.0)
        }
    }
}

/// What the engine knows about an app's cloud files
/// (`ERemoteStorageSyncState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncState {
    Disabled,
    Unknown,
    Synchronized,
    InProgress,
    ChangesInCloud,
    ChangesLocally,
    ChangesInCloudAndLocally,
    Conflict,
    NotInitialized,
    Other(u32),
}

impl From<u32> for SyncState {
    fn from(value: u32) -> SyncState {
        match value {
            0 => SyncState::Disabled,
            1 => SyncState::Unknown,
            2 => SyncState::Synchronized,
            3 => SyncState::InProgress,
            4 => SyncState::ChangesInCloud,
            5 => SyncState::ChangesLocally,
            6 => SyncState::ChangesInCloudAndLocally,
            7 => SyncState::Conflict,
            8 => SyncState::NotInitialized,
            other => SyncState::Other(other),
        }
    }
}

pub struct RemoteStorage(*mut c_void);

impl RemoteStorage {
    /// Steam Cloud on for the account (Steam's Settings > Cloud).
    pub fn enabled_for_account(&self) -> bool {
        unsafe {
            let f: unsafe extern "C" fn(*mut c_void) -> bool =
                vfn(self.0, storage_slot::IS_CLOUD_ENABLED_FOR_ACCOUNT);
            f(self.0)
        }
    }

    /// Steam Cloud on for one game (its Properties > General).
    pub fn enabled_for_app(&self, appid: u32) -> bool {
        unsafe {
            let f: unsafe extern "C" fn(*mut c_void, u32) -> bool =
                vfn(self.0, storage_slot::IS_CLOUD_ENABLED_FOR_APP);
            f(self.0, appid)
        }
    }

    /// Load what the engine knows about an app's local cloud files (its
    /// "init" sync). Nothing else cloud-related works for the app before it.
    pub fn load_local_cache(&self, appid: u32) {
        unsafe {
            let f: unsafe extern "C" fn(*mut c_void, u32) =
                vfn(self.0, storage_slot::LOAD_LOCAL_FILE_INFO_CACHE);
            f(self.0, appid)
        }
    }

    /// Start a sync; false if the engine wouldn't (cloud off, conflict).
    pub fn synchronize(&self, appid: u32, direction: SyncDirection) -> bool {
        let flags = match direction {
            SyncDirection::Down => SYNC_FLAGS_LAUNCH,
            SyncDirection::Up => SYNC_FLAGS_EXIT,
        };
        unsafe {
            let f: unsafe extern "C" fn(*mut c_void, u32, i32, u64) -> bool =
                vfn(self.0, storage_slot::SYNCHRONIZE_APP);
            f(self.0, appid, direction as i32, flags)
        }
    }

    pub fn state(&self, appid: u32) -> SyncState {
        unsafe {
            let f: unsafe extern "C" fn(*mut c_void, u32) -> u32 =
                vfn(self.0, storage_slot::GET_SYNC_STATE);
            f(self.0, appid).into()
        }
    }

    pub fn in_progress(&self, appid: u32) -> bool {
        unsafe {
            let f: unsafe extern "C" fn(*mut c_void, u32) -> bool =
                vfn(self.0, storage_slot::IS_APP_SYNC_IN_PROGRESS);
            f(self.0, appid)
        }
    }

    /// Keep the local files (`true`) or the cloud ones (`false`).
    pub fn resolve_conflict(&self, appid: u32, keep_local: bool) -> bool {
        unsafe {
            let f: unsafe extern "C" fn(*mut c_void, u32, bool) -> bool =
                vfn(self.0, storage_slot::RESOLVE_SYNC_CONFLICT);
            f(self.0, appid, keep_local)
        }
    }
}

pub struct AppManager(*mut c_void);

impl AppManager {
    /// Launch like Steam's Play button: the engine syncs cloud saves, starts
    /// the game with Steam's environment, and tracks it. `option` is the key
    /// of the app info launch entry. Returns the async call handle (0 if
    /// the engine refused outright).
    pub fn launch(&self, appid: u32, option: u32) -> u64 {
        // CGameID of a plain Steam app is just its id.
        let game_id: u64 = appid as u64;
        unsafe {
            let f: unsafe extern "C" fn(*mut c_void, *const u64, u32, u32, *const c_char) -> u64 =
                vfn(self.0, app_slot::LAUNCH_APP);
            f(
                self.0,
                &game_id,
                option,
                LAUNCH_SOURCE_LIBRARY,
                c"".as_ptr(),
            )
        }
    }

    /// `EAppState` flags (`APP_FULLY_INSTALLED`, `APP_RUNNING`, …).
    pub fn install_state(&self, appid: u32) -> u32 {
        unsafe {
            let f: unsafe extern "C" fn(*mut c_void, u32) -> u32 =
                vfn(self.0, app_slot::GET_APP_INSTALL_STATE);
            f(self.0, appid)
        }
    }
}
