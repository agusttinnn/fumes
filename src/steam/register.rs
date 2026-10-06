//! Letting games find the running engine. Each platform's `steam_api` looks
//! somewhere different:
//!
//! - Linux: `~/.steam/steam.pid` must name a live process, and the client
//!   library is loaded from `~/.steam/sdk64/steamclient.so` (`sdk32` for
//!   32-bit games).
//! - Windows: `HKCU\Software\Valve\Steam\ActiveProcess`: `pid`, plus the
//!   client library paths `SteamClientDll64` / `SteamClientDll`.
//! - macOS: the Mach service `com.valvesoftware.steam.ipctool`, served by
//!   Valve's `ipcserver` (a small broker for the shared memory games and the
//!   engine talk through). Mach services must be declared to launchd, so
//!   Steam's bootstrapper writes
//!   `~/Library/LaunchAgents/com.valvesoftware.steam.ipctool.plist` and
//!   loads it; launchd then starts `ipcserver` on first lookup. `install`
//!   does the same once, pointing at the fetched `ipcserver`.
//!
//! On Linux and Windows the state is set while the engine runs and put back
//! as it was when `Registration` is dropped, so nothing outlives the run;
//! there `install` has nothing to do.

#[cfg(all(unix, not(target_os = "macos")))]
pub use linux::*;
#[cfg(target_os = "macos")]
pub use macos::*;
#[cfg(windows)]
pub use windows::*;

/// Linux and Windows register per run, so they're always ready.
#[cfg(not(target_os = "macos"))]
pub fn ready() -> anyhow::Result<bool> {
    Ok(true)
}

/// Linux and Windows register per run, so there's nothing to install.
#[cfg(not(target_os = "macos"))]
pub fn install(_runtime: &std::path::Path) -> anyhow::Result<()> {
    println!("Nothing to install here: `run` registers the engine while it runs.");
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn uninstall() -> anyhow::Result<()> {
    install(std::path::Path::new(""))
}

#[cfg(target_os = "macos")]
mod macos {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use anyhow::{Context, Result, bail};

    pub const SERVICE: &str = "com.valvesoftware.steam.ipctool";

    fn domain() -> Result<String> {
        let uid = String::from_utf8(Command::new("id").arg("-u").output()?.stdout)?;
        Ok(format!("gui/{}", uid.trim()))
    }

    fn plist_path() -> Result<PathBuf> {
        Ok(directories::BaseDirs::new()
            .context("no home directory")?
            .home_dir()
            .join("Library/LaunchAgents")
            .join(format!("{SERVICE}.plist")))
    }

    /// The agent Steam's bootstrapper installs: run `ipcserver` on demand
    /// for whoever looks up the Mach service.
    pub fn plist(ipcserver: &Path) -> String {
        let path = ipcserver
            .to_string_lossy()
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{SERVICE}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{path}</string>
	</array>
	<key>MachServices</key>
	<dict>
		<key>{SERVICE}</key>
		<true/>
	</dict>
</dict>
</plist>
"#
        )
    }

    /// Write the agent and load it, replacing any earlier one (including
    /// real Steam's, which `uninstall` doesn't restore).
    pub fn install(runtime: &Path) -> Result<()> {
        let ipcserver = runtime.join("ipcserver");
        if !ipcserver.is_file() {
            bail!("{} not found; run `fetch` first", ipcserver.display());
        }
        let path = plist_path()?;
        fs::create_dir_all(path.parent().unwrap())?;
        // Unload whatever is there; failing because nothing is loaded is fine.
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("{}/{SERVICE}", domain()?)])
            .output();
        fs::write(&path, plist(&ipcserver))?;
        let out = Command::new("launchctl")
            .args(["bootstrap", &domain()?])
            .arg(&path)
            .output()?;
        if !out.status.success() {
            bail!(
                "launchctl bootstrap failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        println!("Loaded {} ({})", path.display(), ipcserver.display());
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("{}/{SERVICE}", domain()?)])
            .output();
        let path = plist_path()?;
        match fs::remove_file(&path) {
            Ok(()) => println!("Unloaded and removed {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("Not installed."),
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    /// Whether launchd knows the service games look up.
    fn loaded() -> Result<bool> {
        let out = Command::new("launchctl")
            .args(["print", &format!("{}/{SERVICE}", domain()?)])
            .output()?;
        Ok(out.status.success())
    }

    /// Whether games can find the engine (the agent is loaded).
    pub fn ready() -> Result<bool> {
        loaded()
    }

    pub fn status() -> Result<()> {
        if loaded()? {
            println!("{SERVICE} is loaded: games can find the engine");
        } else {
            println!("{SERVICE} isn't loaded: games won't find the engine (`fumes engine setup`)");
        }
        Ok(())
    }

    /// Tells `ipcserver` where the running client is. Nothing to undo:
    /// `ipcserver` drops the entry once the pid is gone.
    pub struct Registration;

    impl Registration {
        pub fn acquire(runtime: &Path) -> Result<Registration> {
            if !loaded()? {
                eprintln!(
                    "warning: {SERVICE} isn't loaded, so games won't find the engine; \
                     run `fumes engine setup`"
                );
                return Ok(Registration);
            }
            // Games treat the registered path as the Steam executable and
            // load steamclient.dylib from its folder; ipcserver only hands
            // it out while the file is readable. The engine itself fits both.
            mach::set_steam_path(&runtime.join("steamclient.dylib"), std::process::id())?;
            Ok(Registration)
        }
    }

    /// `ipcserver`'s protocol, as `libsteam_api` and Steam's bootstrapper
    /// speak it: Mach messages to the service, `msgh_id` = protocol version
    /// 0x68, the command after the header. Command 13 stores the client's
    /// executable path and pid (refused while a live pid is registered; no
    /// reply). Games read them back with command 14 (`GetSteamPath`), which
    /// answers only while the pid is alive and the path is readable.
    mod mach {
        use std::ffi::{CString, c_char};
        use std::path::Path;

        use anyhow::{Result, bail};

        type MachPort = u32;

        unsafe extern "C" {
            static bootstrap_port: MachPort;
            static mach_task_self_: MachPort;
            fn bootstrap_look_up(bp: MachPort, name: *const c_char, sp: *mut MachPort) -> i32;
            fn mach_port_deallocate(task: MachPort, name: MachPort) -> i32;
            fn mach_msg(
                msg: *mut u8,
                option: i32,
                send_size: u32,
                rcv_size: u32,
                rcv_name: MachPort,
                timeout: u32,
                notify: MachPort,
            ) -> i32;
        }

        const PROTOCOL_VERSION: u32 = 0x68;
        const CMD_SET_STEAM_PATH: u32 = 13;
        /// remote: MACH_MSG_TYPE_COPY_SEND (19); no reply port.
        const MSGH_BITS: u32 = 0x13;
        const MACH_SEND_MSG: i32 = 0x1;
        const MACH_SEND_TIMEOUT: i32 = 0x10;
        const TIMEOUT_MS: u32 = 2000;

        /// mach_msg_header_t, then the command, pid and path.
        #[repr(C)]
        struct SetSteamPath {
            bits: u32,
            size: u32,
            remote_port: MachPort,
            local_port: MachPort,
            voucher_port: MachPort,
            id: u32,
            command: u32,
            pid: u32,
            path: [u8; 512],
        }

        pub fn set_steam_path(path: &Path, pid: u32) -> Result<()> {
            let bytes = path.to_string_lossy().into_owned().into_bytes();
            if bytes.len() >= 512 {
                bail!("{} is too long for ipcserver", path.display());
            }
            let name = CString::new(super::SERVICE)?;
            unsafe {
                let mut server: MachPort = 0;
                let kr = bootstrap_look_up(bootstrap_port, name.as_ptr(), &mut server);
                if kr != 0 {
                    bail!("bootstrap_look_up({}) failed: {kr:#x}", super::SERVICE);
                }
                let mut msg = SetSteamPath {
                    bits: MSGH_BITS,
                    size: size_of::<SetSteamPath>() as u32,
                    remote_port: server,
                    local_port: 0,
                    voucher_port: 0,
                    id: PROTOCOL_VERSION,
                    command: CMD_SET_STEAM_PATH,
                    pid,
                    path: [0; 512],
                };
                msg.path[..bytes.len()].copy_from_slice(&bytes);
                let sent = mach_msg(
                    (&raw mut msg).cast(),
                    MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                    msg.size,
                    0,
                    0,
                    TIMEOUT_MS,
                    0,
                );
                mach_port_deallocate(mach_task_self_, server);
                if sent != 0 {
                    bail!("sending SetSteamPath to ipcserver failed: {sent:#x}");
                }
            }
            tracing::debug!(path = %path.display(), pid, "registered with ipcserver");
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn plist_declares_the_mach_service() {
            let p = plist(Path::new("/x/A&B/ipcserver"));
            assert!(p.contains("<string>/x/A&amp;B/ipcserver</string>"));
            assert_eq!(p.matches(SERVICE).count(), 2);
            assert!(p.contains("<key>MachServices</key>"));
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod linux {
    use std::fs;
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result, bail};

    fn steam_dir() -> Result<PathBuf> {
        Ok(directories::BaseDirs::new()
            .context("no home directory")?
            .home_dir()
            .join(".steam"))
    }

    fn alive(pid: &str) -> bool {
        pid.trim()
            .parse::<u32>()
            .is_ok_and(|p| p != std::process::id() && Path::new(&format!("/proc/{p}")).exists())
    }

    pub fn status() -> Result<()> {
        let dir = steam_dir()?;
        let pid = fs::read_to_string(dir.join("steam.pid")).unwrap_or_default();
        println!(
            "~/.steam/steam.pid = {:?} (another live process: {})",
            pid.trim(),
            alive(&pid)
        );
        for link in ["sdk64", "sdk32"] {
            println!(
                "~/.steam/{link} -> {:?}",
                fs::read_link(dir.join(link)).ok()
            );
        }
        Ok(())
    }

    enum Undo {
        Remove(PathBuf),
        Write(PathBuf, Vec<u8>),
        Symlink(PathBuf, PathBuf),
    }

    /// `steam.pid` and the sdk links, put back as they were on drop.
    pub struct Registration {
        undo: Vec<Undo>,
    }

    impl Registration {
        pub fn acquire(runtime: &Path) -> Result<Registration> {
            let dir = steam_dir()?;
            fs::create_dir_all(&dir)?;
            let mut reg = Registration { undo: Vec::new() };

            let pid_file = dir.join("steam.pid");
            match fs::read(&pid_file) {
                Ok(old) => {
                    if alive(&String::from_utf8_lossy(&old)) {
                        bail!(
                            "a Steam client is already running (pid {}); quit it first",
                            String::from_utf8_lossy(&old).trim()
                        );
                    }
                    reg.undo.push(Undo::Write(pid_file.clone(), old));
                }
                Err(_) => reg.undo.push(Undo::Remove(pid_file.clone())),
            }
            fs::write(&pid_file, std::process::id().to_string())?;

            for (link, target) in [("sdk64", "linux64"), ("sdk32", "linux32")] {
                let link = dir.join(link);
                match fs::symlink_metadata(&link) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        let old = fs::read_link(&link)?;
                        fs::remove_file(&link)?;
                        reg.undo.push(Undo::Symlink(link.clone(), old));
                    }
                    Ok(_) => bail!(
                        "{} is a real folder, not a link; leaving it alone",
                        link.display()
                    ),
                    Err(_) => reg.undo.push(Undo::Remove(link.clone())),
                }
                std::os::unix::fs::symlink(runtime.join(target), &link)?;
            }
            Ok(reg)
        }
    }

    impl Drop for Registration {
        fn drop(&mut self) {
            for undo in self.undo.drain(..).rev() {
                let _ = match undo {
                    Undo::Remove(path) => fs::remove_file(path),
                    Undo::Write(path, bytes) => fs::write(path, bytes),
                    Undo::Symlink(link, old) => {
                        fs::remove_file(&link).and_then(|()| std::os::unix::fs::symlink(old, &link))
                    }
                };
            }
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::path::Path;

    use anyhow::Result;
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_ALL_ACCESS};

    const KEY: &str = r"Software\Valve\Steam\ActiveProcess";
    const VALUES: [&str; 3] = ["pid", "SteamClientDll64", "SteamClientDll"];

    pub fn status() -> Result<()> {
        match RegKey::predef(HKEY_CURRENT_USER).open_subkey(KEY) {
            Ok(key) => {
                for name in VALUES {
                    println!("{KEY}\\{name} = {:?}", key.get_raw_value(name).ok());
                }
            }
            Err(_) => println!("{KEY} doesn't exist"),
        }
        Ok(())
    }

    /// The ActiveProcess values, put back as they were on drop.
    pub struct Registration {
        old: Vec<(&'static str, Option<winreg::RegValue<'static>>)>,
    }

    impl Registration {
        pub fn acquire(runtime: &Path) -> Result<Registration> {
            let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(KEY)?;
            let old = VALUES
                .iter()
                .map(|name| (*name, key.get_raw_value(name).ok()))
                .collect();
            key.set_value("pid", &std::process::id())?;
            for (name, dll) in [
                ("SteamClientDll64", "steamclient64.dll"),
                ("SteamClientDll", "steamclient.dll"),
            ] {
                key.set_value(name, &runtime.join(dll).to_string_lossy().into_owned())?;
            }
            Ok(Registration { old })
        }
    }

    impl Drop for Registration {
        fn drop(&mut self) {
            let Ok(key) =
                RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(KEY, KEY_ALL_ACCESS)
            else {
                return;
            };
            for (name, value) in self.old.drain(..) {
                let _ = match value {
                    Some(value) => key.set_raw_value(name, &value),
                    None => key.delete_value(name),
                };
            }
        }
    }
}
