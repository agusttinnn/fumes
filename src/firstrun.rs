//! First-time setup, as Steam does before a game's first launch. The
//! game's install script (`installscript.vdf`) and those of the shared
//! redistributables it uses (DirectX, Visual C++, .NET, XNA, PhysX… in
//! "Steamworks Shared") install runtimes, set registry values and copy
//! default files. Without them a game stops at its first missing DLL, such
//! as d3dx9_42.dll or MSVCP140.dll.
//!
//! A script is KeyValues text with these sections, done in file order:
//! - `Run Process`: installers to run once. When one has run, a DWORD named
//!   after its entry is set under the entry's `HasRunKey`, as Steam does,
//!   so a runtime Steam already installed isn't installed again (and Steam
//!   knows about fumes' too). A newer runtime has a new entry name, so it
//!   runs. Entries go in order and a name that already ran is skipped:
//!   that's how .NET's scripts run a no-op on Windows XP and the installer
//!   everywhere else.
//! - `Registry`: values to set (`string` or `dword`, per language or not).
//! - `Copy Folders`: default files copied into the user's folders, without
//!   overwriting any.
//!
//! Installers and machine-wide registry values need administrator rights,
//! so they go to an elevated copy of fumes (`first-run`) started through
//! Windows' permission prompt. It's handed the steps on its command line
//! and reports back only through the registry values it sets, so it never
//! reads or writes files a non-elevated process could swap underneath it.
//! Scripts are Windows-only, and so is running them.

#![cfg_attr(not(windows), allow(dead_code))]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// An install script and what it belongs to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Script {
    pub path: PathBuf,
    /// What `%INSTALLDIR%` means in it: the folder of the app it came with
    /// (the game's, or Steamworks Shared).
    pub installdir: PathBuf,
    /// What it sets up, for messages: "DirectX Jun2010", or the game.
    pub label: String,
}

/// The name to show for a shared redistributable's script, from where it
/// sits: `_CommonRedist/vcredist/2015/installscript.vdf` → "Visual C++ 2015".
pub fn redist_label(path: &str) -> String {
    let parts: Vec<&str> = path.split(['/', '\\']).collect();
    let folders = &parts[..parts.len().saturating_sub(1)];
    let folders = match folders.first() {
        Some(first) if first.eq_ignore_ascii_case("_CommonRedist") => &folders[1..],
        _ => folders,
    };
    let Some((kind, rest)) = folders.split_first() else {
        return "Steamworks redistributables".into();
    };
    let kind = match kind.to_ascii_lowercase().as_str() {
        "vcredist" => "Visual C++",
        "dotnet" => ".NET",
        "directx" => "DirectX",
        "xna" => "XNA Framework",
        "physx" => "PhysX",
        "openal" => "OpenAL",
        _ => kind,
    };
    std::iter::once(kind)
        .chain(rest.iter().copied())
        .collect::<Vec<_>>()
        .join(" ")
}

// KeyValues

/// A KeyValues value: text, or a block of keys in file order (keys repeat).
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Text(String),
    Block(Block),
}

pub type Block = Vec<(String, Node)>;

/// Text KeyValues, keeping order and repeated keys, which install scripts
/// rely on and keyvalues-parser's sorted maps drop.
pub fn parse(text: &str) -> Result<Block> {
    let tokens = tokenize(text.trim_start_matches('\u{feff}'))?;
    let mut at = 0;
    let block = read_block(&tokens, &mut at, true)?;
    Ok(block)
}

#[derive(Debug, PartialEq)]
enum Token {
    Open,
    Close,
    Text(String),
}

fn tokenize(text: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {}
            '{' => tokens.push(Token::Open),
            '}' => tokens.push(Token::Close),
            '/' if chars.peek() == Some(&'/') => while chars.next_if(|&c| c != '\n').is_some() {},
            // Platform conditionals (`[$WIN32]`) after a value.
            '[' => while chars.next().is_some_and(|c| c != ']') {},
            '"' => {
                let mut s = String::new();
                loop {
                    match chars.next() {
                        None => bail!("a quoted string never ends"),
                        Some('"') => break,
                        // Steam escapes backslashes, but not always: an
                        // unknown escape is a plain backslash.
                        Some('\\') => match chars.peek() {
                            Some('\\') | Some('"') => s.push(chars.next().unwrap()),
                            Some('n') => {
                                chars.next();
                                s.push('\n');
                            }
                            Some('t') => {
                                chars.next();
                                s.push('\t');
                            }
                            _ => s.push('\\'),
                        },
                        Some(c) => s.push(c),
                    }
                }
                tokens.push(Token::Text(s));
            }
            c => {
                let mut s = String::from(c);
                while let Some(c) =
                    chars.next_if(|&c| !c.is_whitespace() && !matches!(c, '{' | '}' | '"'))
                {
                    s.push(c);
                }
                tokens.push(Token::Text(s));
            }
        }
    }
    Ok(tokens)
}

fn read_block(tokens: &[Token], at: &mut usize, top: bool) -> Result<Block> {
    let mut block = Block::new();
    loop {
        let key = match tokens.get(*at) {
            None if top => return Ok(block),
            None => bail!("a block never closes"),
            Some(Token::Close) if !top => {
                *at += 1;
                return Ok(block);
            }
            Some(Token::Text(key)) => key.clone(),
            Some(_) => bail!("expected a key"),
        };
        *at += 1;
        let value = match tokens.get(*at) {
            Some(Token::Text(text)) => Node::Text(text.clone()),
            Some(Token::Open) => {
                *at += 1;
                block.push((key, Node::Block(read_block(tokens, at, false)?)));
                continue;
            }
            _ => bail!("{key:?} has no value"),
        };
        *at += 1;
        block.push((key, value));
    }
}

fn find<'a>(block: &'a [(String, Node)], key: &str) -> Option<&'a Node> {
    block
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v)
}

fn text<'a>(block: &'a [(String, Node)], key: &str) -> Option<&'a str> {
    match find(block, key)? {
        Node::Text(text) => Some(text),
        Node::Block(_) => None,
    }
}

fn sub<'a>(block: &'a [(String, Node)], key: &str) -> Option<&'a Block> {
    match find(block, key)? {
        Node::Block(block) => Some(block),
        Node::Text(_) => None,
    }
}

// What a script asks for

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Hive {
    LocalMachine,
    CurrentUser,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Data {
    Text(String),
    Dword(u32),
}

/// A registry value. Keys are in the 32-bit view, where Steam (a 32-bit
/// program) and the 32-bit games of its time read and write them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Value {
    pub hive: Hive,
    pub key: String,
    pub name: String,
    pub data: Data,
}

/// One `Run Process` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Process {
    /// "Visual C++ 2015 (x64 Update 3 14.0.24215.0)".
    pub label: String,
    /// Set once it has run; it counts as run when the DWORD there is at
    /// least this value's.
    pub ran: Value,
    /// Programs with their command lines, run in order.
    pub commands: Vec<(String, String)>,
    /// Count it as run whatever the installers return.
    pub ignore_exit_code: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Step {
    Set(Value),
    Run(Process),
    /// Copy a folder's files that aren't at the destination yet.
    Copy {
        from: PathBuf,
        to: PathBuf,
    },
}

impl Step {
    /// An installer given by its full path that isn't on disk.
    pub fn missing_program(&self) -> Option<&str> {
        let Step::Run(process) = self else {
            return None;
        };
        process
            .commands
            .iter()
            .map(|(program, _)| program.as_str())
            .find(|program| {
                let path = Path::new(program);
                // A bare name ("msiexec") is looked up on the PATH.
                (path.is_absolute() || program.contains(['\\', '/'])) && !path.exists()
            })
    }

    /// Needs administrator rights.
    pub fn elevated(&self) -> bool {
        match self {
            Step::Set(value) => value.hive == Hive::LocalMachine,
            Step::Run(_) => true,
            Step::Copy { .. } => false,
        }
    }
}

/// What `%NAME%` stands for in a script.
pub trait Vars {
    fn get(&self, name: &str) -> Option<String>;
}

/// The steps a script asks for on this machine, in its order, with
/// variables filled in. Entries that can't be understood are left out
/// with a warning, rather than sinking the rest.
pub fn steps(script: &Script, source: &str, vars: &dyn Vars, language: &str) -> Result<Vec<Step>> {
    let root = parse(source).with_context(|| format!("reading {}", script.path.display()))?;
    let Some(body) = sub(&root, "InstallScript") else {
        bail!("{} has no InstallScript block", script.path.display());
    };
    let fill = |s: &str| expand(s, vars);
    let mut steps = Vec::new();
    for (section, node) in body {
        let Node::Block(entries) = node else { continue };
        match section.to_ascii_lowercase().as_str() {
            "run process" => {
                for (name, node) in entries {
                    let Node::Block(entry) = node else { continue };
                    match process(script, name, entry, &fill) {
                        Ok(Some(process)) => steps.push(Step::Run(process)),
                        Ok(None) => {}
                        Err(e) => tracing::warn!("{}: {name}: {e:#}", script.label),
                    }
                }
            }
            "registry" => {
                for (key, node) in entries {
                    let Node::Block(types) = node else { continue };
                    match registry(key, types, language, &fill) {
                        Ok(values) => steps.extend(values.into_iter().map(Step::Set)),
                        Err(e) => tracing::warn!("{}: {key}: {e:#}", script.label),
                    }
                }
            }
            "copy folders" => {
                for (name, node) in entries {
                    let Node::Block(entry) = node else { continue };
                    for n in 1.. {
                        let (Some(from), Some(to)) = (
                            text(entry, &format!("SrcFolder {n}")),
                            text(entry, &format!("DstFolder {n}")),
                        ) else {
                            break;
                        };
                        match (fill(from), fill(to)) {
                            (Ok(from), Ok(to)) => steps.push(Step::Copy {
                                from: from.into(),
                                to: to.into(),
                            }),
                            (Err(e), _) | (_, Err(e)) => {
                                tracing::warn!("{}: {name}: {e:#}", script.label)
                            }
                        }
                    }
                }
            }
            // Uninstall-time steps, firewall rules (Windows asks when a
            // game first listens), and whatever else isn't a launch blocker.
            other => tracing::debug!("{}: skipping {other:?}", script.label),
        }
    }
    Ok(steps)
}

fn process(
    script: &Script,
    name: &str,
    entry: &Block,
    expand: &dyn Fn(&str) -> Result<String>,
) -> Result<Option<Process>> {
    if let Some(requirement) = sub(entry, "Requirement_OS")
        && !requirement_met(requirement)
    {
        return Ok(None);
    }
    let mut commands = Vec::new();
    for n in 1.. {
        let Some(program) = text(entry, &format!("process {n}")) else {
            break;
        };
        let args = text(entry, &format!("command {n}")).unwrap_or_default();
        commands.push((expand(program)?, expand(args)?));
    }
    if commands.is_empty() {
        bail!("nothing to run");
    }
    let minimum = text(entry, "MinimumHasRunValue")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(1u32)
        .max(1);
    let ran = match text(entry, "HasRunKey") {
        Some(key) => {
            let (hive, key) = split_key(key)?;
            Value {
                hive,
                key,
                name: name.to_owned(),
                data: Data::Dword(minimum),
            }
        }
        // Run once per install of the game, as Steam does without one.
        None => Value {
            hive: Hive::LocalMachine,
            key: format!(
                "Software\\fumes\\FirstRun\\{}",
                &hex::encode(crate::cdn::sha1(
                    script.path.to_string_lossy().to_lowercase().as_bytes()
                ))[..16]
            ),
            name: name.to_owned(),
            data: Data::Dword(minimum),
        },
    };
    Ok(Some(Process {
        label: format!("{} ({name})", script.label),
        ran,
        commands,
        ignore_exit_code: text(entry, "IgnoreExitCode").is_some_and(|v| v.trim() == "1"),
    }))
}

/// fumes runs as a 64-bit program on Windows 10 or later.
fn requirement_met(requirement: &Block) -> bool {
    requirement.iter().all(|(key, value)| {
        let Node::Text(value) = value else {
            return false;
        };
        match key.to_ascii_lowercase().as_str() {
            "is64bitwindows" => value.trim() == "1",
            // "Windows XP", "Windows 7": only that one. Later Windows
            // versions are numbered.
            "ostype" => value
                .trim()
                .strip_prefix("Windows ")
                .and_then(|v| v.trim().parse::<u32>().ok())
                .is_some_and(|v| v >= 10),
            other => {
                tracing::warn!("unknown install script requirement {other:?}; skipping");
                false
            }
        }
    })
}

fn registry(
    key: &str,
    types: &Block,
    language: &str,
    expand: &dyn Fn(&str) -> Result<String>,
) -> Result<Vec<Value>> {
    let (hive, key) = split_key(&expand(key)?)?;
    let mut values = Vec::new();
    for (kind, node) in types {
        let Node::Block(entries) = node else { continue };
        // Either the values themselves, or one block of them per language.
        let entries = if entries.iter().any(|(_, v)| matches!(v, Node::Block(_))) {
            let pick = sub(entries, language)
                .or_else(|| sub(entries, "english"))
                .or_else(|| {
                    entries.iter().find_map(|(_, v)| match v {
                        Node::Block(b) => Some(b),
                        Node::Text(_) => None,
                    })
                });
            match pick {
                Some(entries) => entries,
                None => continue,
            }
        } else {
            entries
        };
        for (name, value) in entries {
            let Node::Text(value) = value else { continue };
            let value = expand(value)?;
            let data = match kind.to_ascii_lowercase().as_str() {
                "string" => Data::Text(value),
                "dword" => Data::Dword(parse_dword(&value)?),
                other => bail!("unknown registry value type {other:?}"),
            };
            values.push(Value {
                hive,
                key: key.clone(),
                name: name.clone(),
                data,
            });
        }
    }
    Ok(values)
}

fn parse_dword(value: &str) -> Result<u32> {
    let value = value.trim();
    let parsed = match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => u32::from_str_radix(hex, 16).ok(),
        None => value
            .parse::<u32>()
            .ok()
            .or_else(|| value.parse::<i32>().ok().map(|v| v as u32)),
    };
    parsed.with_context(|| format!("{value:?} isn't a number"))
}

/// `HKEY_LOCAL_MACHINE\Software\…` → the hive and the rest. Steam isn't
/// careful about the hive's case (Skyrim has `HKEY_lOCAL_MACHINE`).
fn split_key(path: &str) -> Result<(Hive, String)> {
    let (hive, key) = path.split_once('\\').unwrap_or((path, ""));
    let hive = match hive.to_ascii_uppercase().as_str() {
        "HKEY_LOCAL_MACHINE" | "HKLM" => Hive::LocalMachine,
        "HKEY_CURRENT_USER" | "HKCU" => Hive::CurrentUser,
        other => bail!("unsupported registry hive {other:?}"),
    };
    let key = key.trim_matches('\\');
    if key.is_empty() {
        bail!("no registry key in {path:?}");
    }
    Ok((hive, key.to_owned()))
}

/// Fill in `%NAME%`s. An unknown one is an error: guessing at a path
/// would put files or registry values in the wrong place.
fn expand(text: &str, vars: &dyn Vars) -> Result<String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let name = &after[..end];
        match vars.get(&name.to_ascii_uppercase()) {
            Some(value) => out.push_str(value.trim_end_matches(['\\', '/'])),
            None => bail!("unknown variable %{name}%"),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// The variables a script can use on this machine.
pub struct MachineVars {
    pub installdir: PathBuf,
}

impl Vars for MachineVars {
    fn get(&self, name: &str) -> Option<String> {
        let env = |var: &str| std::env::var(var).ok().filter(|v| !v.is_empty());
        match name {
            "INSTALLDIR" => Some(windows_path(&self.installdir)),
            "USER_MYDOCS" => {
                directories::UserDirs::new().and_then(|d| d.document_dir().map(windows_path))
            }
            "USER_APPDATA" => env("APPDATA"),
            "USER_LOCALAPPDATA" => env("LOCALAPPDATA"),
            "COMMON_APPDATA" => env("ProgramData"),
            "WINDIR" => env("windir"),
            "ROOTDRIVE" | "SYSTEMDRIVE" => env("SystemDrive"),
            "PROGRAMFILES" => env("ProgramFiles"),
            _ => None,
        }
    }
}

fn windows_path(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\")
}

/// Copy what's under `from` to `to`, keeping every file already there.
pub fn copy_missing(from: &Path, to: &Path) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(from) else {
        return Ok(());
    };
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in entries.flatten() {
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        if source.is_dir() {
            copy_missing(&source, &target)?;
        } else if !target.exists() {
            std::fs::copy(&source, &target)
                .with_context(|| format!("copying {}", source.display()))?;
        }
    }
    Ok(())
}

/// A script's text. Old ones may be UTF-16 (with a byte-order mark) or in
/// the Windows code page rather than UTF-8.
pub fn read_script(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if let Some(utf16) = bytes.strip_prefix(&[0xff, 0xfe]) {
        let units: Vec<u16> = utf16
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| u16::from_le_bytes(pair))
            .collect();
        return Some(String::from_utf16_lossy(&units));
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
    Some(match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        // Windows-1252 agrees with Latin-1 on everything a script uses.
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    })
}

/// What an installer's exit code means for whether it worked: 3010 and
/// 1641 want a restart, 1638 found the same or a newer version.
pub fn succeeded(code: i32) -> bool {
    matches!(code, 0 | 1638 | 1641 | 3010)
}

#[cfg(windows)]
pub use self::windows::{elevated, prepare};

#[cfg(windows)]
mod windows {
    use std::os::windows::process::CommandExt;
    use std::path::Path;
    use std::process::Command;

    use anyhow::{Context, Result, bail};
    use base64::Engine;
    use winreg::RegKey;
    use winreg::enums::{
        HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WRITE,
    };

    use super::*;
    use crate::installs::Install;
    use crate::tui::game::{report, report_problem};

    /// Get `install` ready for its first launch, or for one after an
    /// update brought new runtimes. Problems are reported rather than
    /// returned: the game may well start anyway.
    pub fn prepare(install: &Install) {
        if let Err(e) = try_prepare(install) {
            report_problem(format_args!("warning: first-time setup: {e:#}"));
        }
    }

    fn try_prepare(install: &Install) -> Result<()> {
        let scripts = match &install.setup {
            Some(scripts) => scripts.clone(),
            None => {
                report_problem(format_args!(
                    "note: {} was installed by an earlier fumes, which didn't fetch the \
                     shared runtimes games need (DirectX, Visual C++…). Updating it once \
                     (Library → Update, or `fumes install {}`) does.",
                    install.name, install.appid
                ));
                legacy_scripts(install)
            }
        };
        let mut pending = Vec::new();
        for script in &scripts {
            let Some(text) = read_script(&script.path) else {
                tracing::warn!("{} is missing", script.path.display());
                continue;
            };
            let vars = MachineVars {
                installdir: script.installdir.clone(),
            };
            match steps(script, &text, &vars, &install.language) {
                Ok(steps) => pending.extend(steps.into_iter().filter(|s| !done(s))),
                Err(e) => report_problem(format_args!("warning: {e:#}")),
            }
        }
        // An installer that isn't there would fail behind a permission
        // prompt on every launch.
        pending.retain(|step| match step.missing_program() {
            Some(program) => {
                report_problem(format_args!(
                    "warning: first-time setup can't find {program}; skipping it"
                ));
                false
            }
            None => true,
        });

        let (elevated, own): (Vec<Step>, Vec<Step>) = pending.into_iter().partition(Step::elevated);
        for step in &own {
            if let Err(e) = apply(step) {
                report_problem(format_args!("warning: first-time setup: {e:#}"));
            }
        }
        if elevated.is_empty() {
            return Ok(());
        }

        let mut labels: Vec<&str> = Vec::new();
        for step in &elevated {
            let label = match step {
                Step::Run(p) => p.label.split(" (").next().unwrap_or(&p.label),
                _ => install.name.as_str(),
            };
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
        report(format_args!(
            "First-time setup: {}. Windows asks for permission…",
            labels.join(", ")
        ));
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&elevated)?);
        let exe = std::env::current_exe().context("finding fumes' own executable")?;
        let code = match run_as_admin(&exe, &format!("first-run {encoded}"))? {
            Some(code) => code,
            None => {
                report_problem(format_args!(
                    "warning: first-time setup was turned down, so {} may be missing \
                     runtimes it needs. It's offered again next launch.",
                    install.name
                ));
                return Ok(());
            }
        };

        // What's still not done is what failed: the elevated side only
        // reports through the registry.
        let failed: Vec<String> = elevated
            .iter()
            .filter(|s| !done(s))
            .map(|s| match s {
                Step::Run(p) => p.label.clone(),
                Step::Set(v) => format!("registry value {}\\{}", v.key, v.name),
                Step::Copy { .. } => unreachable!(),
            })
            .collect();
        if failed.is_empty() {
            report(format_args!("First-time setup done."));
        } else {
            report_problem(format_args!(
                "warning: first-time setup didn't finish (exit code {code}): {}. It's \
                 tried again next launch.",
                failed.join(", ")
            ));
        }
        Ok(())
    }

    /// Installs recorded before fumes kept scripts: the game's own, at the
    /// top of its folder, where Steam looks for it.
    fn legacy_scripts(install: &Install) -> Vec<Script> {
        let Ok(entries) = std::fs::read_dir(&install.path) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case("installscript.vdf")
            })
            .map(|e| Script {
                path: e.path(),
                installdir: install.path.clone(),
                label: install.name.clone(),
            })
            .collect()
    }

    fn done(step: &Step) -> bool {
        match step {
            Step::Set(value) => read(value).as_ref() == Some(&value.data),
            Step::Run(process) => has_run(&process.ran),
            Step::Copy { .. } => false,
        }
    }

    fn has_run(ran: &Value) -> bool {
        let Data::Dword(minimum) = ran.data else {
            return false;
        };
        read_dword(ran).is_some_and(|v| v >= minimum)
    }

    fn root(hive: Hive) -> RegKey {
        RegKey::predef(match hive {
            Hive::LocalMachine => HKEY_LOCAL_MACHINE,
            Hive::CurrentUser => HKEY_CURRENT_USER,
        })
    }

    fn read_dword(value: &Value) -> Option<u32> {
        root(value.hive)
            .open_subkey_with_flags(&value.key, KEY_READ | KEY_WOW64_32KEY)
            .ok()?
            .get_value::<u32, _>(&value.name)
            .ok()
    }

    fn read(value: &Value) -> Option<Data> {
        let key = root(value.hive)
            .open_subkey_with_flags(&value.key, KEY_READ | KEY_WOW64_32KEY)
            .ok()?;
        match value.data {
            Data::Text(_) => key.get_value::<String, _>(&value.name).ok().map(Data::Text),
            Data::Dword(_) => key.get_value::<u32, _>(&value.name).ok().map(Data::Dword),
        }
    }

    fn write(value: &Value) -> Result<()> {
        let (key, _) = root(value.hive)
            .create_subkey_with_flags(&value.key, KEY_WRITE | KEY_WOW64_32KEY)
            .with_context(|| format!("opening registry key {}", value.key))?;
        match &value.data {
            Data::Text(text) => key.set_value(&value.name, text),
            Data::Dword(dword) => key.set_value(&value.name, dword),
        }
        .with_context(|| format!("setting registry value {}\\{}", value.key, value.name))
    }

    fn apply(step: &Step) -> Result<()> {
        match step {
            Step::Set(value) => write(value),
            Step::Copy { from, to } => copy_missing(from, to),
            Step::Run(process) => {
                // A same-named entry earlier in the script may have run.
                if has_run(&process.ran) {
                    return Ok(());
                }
                for (program, args) in &process.commands {
                    let code = run(program, args).with_context(|| format!("running {program}"))?;
                    if !process.ignore_exit_code && !succeeded(code) {
                        bail!("{} failed (exit code {code})", process.label);
                    }
                }
                write(&process.ran)
            }
        }
    }

    /// Run one installer and wait for it. Command lines are passed exactly
    /// as the script has them; `.cmd` scripts go through cmd.exe.
    fn run(program: &str, args: &str) -> Result<i32> {
        let is_script = [".cmd", ".bat"]
            .iter()
            .any(|ext| program.to_ascii_lowercase().ends_with(ext));
        let mut command = if is_script {
            let mut c = Command::new("cmd.exe");
            c.raw_arg(format!("/d /c \"\"{program}\" {args}\""));
            c
        } else {
            let mut c = Command::new(program);
            c.raw_arg(args);
            c
        };
        if let Some(dir) = Path::new(program).parent().filter(|d| d.is_dir()) {
            command.current_dir(dir);
        }
        let status = command.status()?;
        Ok(status.code().unwrap_or(-1))
    }

    /// The elevated side (`fumes first-run <steps>`): do each step in
    /// order. The exit code is 0, or the first failure's.
    pub fn elevated(encoded: &str) -> i32 {
        let steps: Vec<Step> = match base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .ok()
            .and_then(|json| serde_json::from_slice(&json).ok())
        {
            Some(steps) => steps,
            None => return 2,
        };
        let mut code = 0;
        for step in steps.iter().filter(|s| s.elevated()) {
            if let Err(e) = apply(step) {
                tracing::warn!("{e:#}");
                if code == 0 {
                    code = 1;
                }
            }
        }
        code
    }

    /// Start `exe` with administrator rights (Windows' permission prompt)
    /// and wait for it. `None` if the prompt was turned down.
    fn run_as_admin(exe: &Path, parameters: &str) -> Result<Option<u32>> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, GetLastError};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, INFINITE, WaitForSingleObject,
        };
        use windows_sys::Win32::UI::Shell::{
            SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

        let wide = |s: &std::ffi::OsStr| -> Vec<u16> { s.encode_wide().chain([0]).collect() };
        let verb = wide("runas".as_ref());
        let file = wide(exe.as_os_str());
        let parameters = wide(parameters.as_ref());
        unsafe {
            let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
            info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
            info.lpVerb = verb.as_ptr();
            info.lpFile = file.as_ptr();
            info.lpParameters = parameters.as_ptr();
            info.nShow = SW_HIDE;
            if ShellExecuteExW(&mut info) == 0 {
                let error = GetLastError();
                if error == ERROR_CANCELLED {
                    return Ok(None);
                }
                bail!("couldn't start the setup as administrator (error {error})");
            }
            if info.hProcess.is_null() {
                bail!("the setup started without a process to wait for");
            }
            WaitForSingleObject(info.hProcess, INFINITE);
            let mut code = 0u32;
            GetExitCodeProcess(info.hProcess, &mut code);
            CloseHandle(info.hProcess);
            Ok(Some(code))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A key of its own under HKCU, gone afterwards.
        struct TestKey(String);

        impl TestKey {
            fn new(name: &str) -> TestKey {
                let key = TestKey(format!(
                    "Software\\fumes-test\\{name}-{}",
                    std::process::id()
                ));
                let _ = RegKey::predef(HKEY_CURRENT_USER).delete_subkey_all(&key.0);
                key
            }

            fn value(&self, name: &str, data: Data) -> Value {
                Value {
                    hive: Hive::CurrentUser,
                    key: self.0.clone(),
                    name: name.into(),
                    data,
                }
            }
        }

        impl Drop for TestKey {
            fn drop(&mut self) {
                let _ = RegKey::predef(HKEY_CURRENT_USER).delete_subkey_all(&self.0);
            }
        }

        #[test]
        fn sets_and_reads_registry_values() {
            let key = TestKey::new("values");
            let text = key.value("Installed Path", Data::Text("C:\\Games\\Skyrim\\".into()));
            let dword = key.value("Installed", Data::Dword(1));
            assert!(!done(&Step::Set(text.clone())));
            apply(&Step::Set(text.clone())).unwrap();
            apply(&Step::Set(dword.clone())).unwrap();
            assert!(done(&Step::Set(text)));
            assert!(done(&Step::Set(dword)));
            assert!(!done(&Step::Set(key.value("Installed", Data::Dword(2)))));
        }

        fn cmd_script(dir: &Path, name: &str, body: &str) -> String {
            let path = dir.join(name);
            std::fs::write(&path, body).unwrap();
            path.to_string_lossy().into_owned()
        }

        #[test]
        fn runs_installers_with_their_command_lines() {
            let dir = tempfile::tempdir().unwrap();
            let spaced = dir.path().join("Microsoft Visual C++");
            std::fs::create_dir_all(&spaced).unwrap();
            let script = cmd_script(
                &spaced,
                "Microsoft Visual C++ 2015 x64.cmd",
                "@exit /b %1\r\n",
            );
            assert_eq!(run(&script, "3010").unwrap(), 3010);
            // Quotes inside the command line get to the program as written.
            let out = dir.path().join("args.txt");
            let echo = cmd_script(
                dir.path(),
                "echo.cmd",
                &format!("@echo %* > \"{}\"\r\n", out.display()),
            );
            run(&echo, "/i \"C:\\x y\\a.msi\" /quiet").unwrap();
            assert_eq!(
                std::fs::read_to_string(&out).unwrap().trim(),
                "/i \"C:\\x y\\a.msi\" /quiet"
            );
            // A bare program name is found on the PATH.
            assert_eq!(run("cmd.exe", "/c exit 7").unwrap(), 7);
        }

        #[test]
        fn runs_each_entry_once_and_records_it() {
            let dir = tempfile::tempdir().unwrap();
            let key = TestKey::new("ran");
            let count = dir.path().join("count.txt");
            let script = cmd_script(
                dir.path(),
                "setup.cmd",
                &format!("@echo x >> \"{}\"\r\n", count.display()),
            );
            let process = |name: &str, args: &str| {
                Step::Run(Process {
                    label: format!("Test ({name})"),
                    ran: key.value(name, Data::Dword(1)),
                    commands: vec![(script.clone(), args.into())],
                    ignore_exit_code: false,
                })
            };
            let first = process("x64 14.0", "");
            assert!(!done(&first));
            apply(&first).unwrap();
            assert!(done(&first));
            // A second entry of the same name (.NET's XP no-op) is skipped.
            apply(&process("x64 14.0", "")).unwrap();
            assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 1);
            // A newer version has a new name, so it runs.
            assert!(!done(&process("x64 14.1", "")));

            // A failing installer isn't recorded, unless told to ignore it.
            let failing = |ignore| {
                Step::Run(Process {
                    label: "Test (fails)".into(),
                    ran: key.value(if ignore { "ignored" } else { "fails" }, Data::Dword(1)),
                    commands: vec![("cmd.exe".into(), "/c exit 1603".into())],
                    ignore_exit_code: ignore,
                })
            };
            assert!(apply(&failing(false)).is_err());
            assert!(!done(&failing(false)));
            apply(&failing(true)).unwrap();
            assert!(done(&failing(true)));
        }

        #[test]
        fn the_elevated_side_takes_its_steps_from_the_command_line() {
            let key = TestKey::new("elevated");
            let run = |name: &str, exit: u32| {
                Step::Run(Process {
                    label: format!("Test ({name})"),
                    ran: key.value(name, Data::Dword(1)),
                    commands: vec![("cmd.exe".into(), format!("/c exit {exit}"))],
                    ignore_exit_code: false,
                })
            };
            let steps = vec![
                // Not one for administrators: left to the non-elevated side.
                Step::Set(key.value("user", Data::Dword(5))),
                run("works", 0),
                run("fails", 1603),
            ];
            let encoded = base64::engine::general_purpose::STANDARD
                .encode(serde_json::to_vec(&steps).unwrap());
            assert_eq!(elevated(&encoded), 1);
            assert!(!done(&steps[0]));
            assert!(done(&steps[1]));
            assert!(!done(&steps[2]));
            assert_eq!(elevated("not base64!"), 2);
        }

        #[test]
        fn starts_programs_as_administrator() {
            // CI runners are administrators without a prompt.
            if std::env::var_os("CI").is_none() {
                return;
            }
            let cmd = std::path::PathBuf::from(std::env::var("ComSpec").unwrap());
            assert_eq!(run_as_admin(&cmd, "/c exit 7").unwrap(), Some(7));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestVars;

    impl Vars for TestVars {
        fn get(&self, name: &str) -> Option<String> {
            match name {
                "INSTALLDIR" => Some("C:\\Games\\Skyrim\\".into()),
                "USER_MYDOCS" => Some("C:\\Users\\me\\Documents".into()),
                _ => None,
            }
        }
    }

    fn script(label: &str) -> Script {
        Script {
            path: PathBuf::from("C:\\Games\\Skyrim\\installscript.vdf"),
            installdir: PathBuf::from("C:\\Games\\Skyrim"),
            label: label.into(),
        }
    }

    // As Skyrim (72850) ships it, signature left out.
    const SKYRIM: &str = r#""InstallScript"
{
	"Registry"
	{
		"HKEY_LOCAL_MACHINE\\SOFTWARE\\Bethesda Softworks\\Skyrim"
		{
			"string"
			{
				"Installed Path"		"%INSTALLDIR%\\"
			}
		}
	}
	"Run Process"
	{
		"DXSetup"
		{
			"HasRunKey"		"HKEY_lOCAL_MACHINE\\Software\\Valve\\Steam\\Apps\\72850"
			"process 1"		"%INSTALLDIR%\\DirectX10\\DXSETUP.exe"
			"command 1"		"/silent"
			"NoCleanUp"		"1"
		}
		"DotNetFX351"
		{
			"HasRunKey"		"HKEY_lOCAL_MACHINE\\Software\\Valve\\Steam\\Apps\\72850"
			"process 1"		"%INSTALLDIR%\\DotNetFX\\dotnetfx35setup.exe"
			"command 1"		"/q /norestart"
			"NoCleanUp"		"1"
			"ignoreexitcode"		"1"
		}
	}
	"Copy Folders"
	{
		"Skyrim"
		{
			"SrcFolder 1"		"%INSTALLDIR%\\Skyrim"
			"DstFolder 1"		"%USER_MYDOCS%\\My Games\\Skyrim"
		}
	}
}
"kvsignatures"
{
}
"#;

    #[test]
    fn reads_skyrims_script() {
        let steps = steps(&script("Skyrim"), SKYRIM, &TestVars, "english").unwrap();
        let ran = |name: &str| Value {
            hive: Hive::LocalMachine,
            key: "Software\\Valve\\Steam\\Apps\\72850".into(),
            name: name.into(),
            data: Data::Dword(1),
        };
        assert_eq!(
            steps,
            [
                Step::Set(Value {
                    hive: Hive::LocalMachine,
                    key: "SOFTWARE\\Bethesda Softworks\\Skyrim".into(),
                    name: "Installed Path".into(),
                    data: Data::Text("C:\\Games\\Skyrim\\".into()),
                }),
                Step::Run(Process {
                    label: "Skyrim (DXSetup)".into(),
                    ran: ran("DXSetup"),
                    commands: vec![(
                        "C:\\Games\\Skyrim\\DirectX10\\DXSETUP.exe".into(),
                        "/silent".into()
                    )],
                    ignore_exit_code: false,
                }),
                Step::Run(Process {
                    label: "Skyrim (DotNetFX351)".into(),
                    ran: ran("DotNetFX351"),
                    commands: vec![(
                        "C:\\Games\\Skyrim\\DotNetFX\\dotnetfx35setup.exe".into(),
                        "/q /norestart".into()
                    )],
                    ignore_exit_code: true,
                }),
                Step::Copy {
                    from: "C:\\Games\\Skyrim\\Skyrim".into(),
                    to: "C:\\Users\\me\\Documents\\My Games\\Skyrim".into(),
                },
            ]
        );
        assert!(steps[0].elevated() && steps[1].elevated() && !steps[3].elevated());
    }

    #[test]
    fn keeps_order_and_repeats_and_honors_requirements() {
        // .NET 4.8's script: a no-op on XP, the installer elsewhere, under
        // one name; and the lowercase spelling some scripts use.
        let text = r#""InstallScript"
{
	"Run Process"
	{
		"4.8"
		{
			"HasRunKey"		"HKEY_LOCAL_MACHINE\\Software\\Valve\\Steam\\Apps\\CommonRedist\\.NET\\4.8"
			"process 1"		"%INSTALLDIR%\\_CommonRedist\\DotNet\\4.8\\noop.cmd"
			"Requirement_OS" { "OSType" "Windows XP" }
		}
		"4.8"
		{
			"HasRunKey"		"HKEY_LOCAL_MACHINE\\Software\\Valve\\Steam\\Apps\\CommonRedist\\.NET\\4.8"
			"process 1"		"%INSTALLDIR%\\_CommonRedist\\DotNet\\4.8\\Microsoft .NET Framework 4.8.cmd"
		}
		"x86 SP1"
		{
			"hasrunkey"		"HKEY_LOCAL_MACHINE\\Software\\Valve\\Steam\\Apps\\CommonRedist\\XNA\\3.0"
			"process 1"		"msiexec"
			"command 1"		"/i \"%INSTALLDIR%\\_CommonRedist\\XNA\\3.0\\xnafx30_redist.msi\" /quiet"
			"requirement_os" { "is64bitwindows" "1" }
		}
		"x86 only" { "process 1" "a.exe" "Requirement_OS" { "Is64BitWindows" "0" } }
	}
}"#;
        let steps = steps(&script(".NET 4.8"), text, &TestVars, "english").unwrap();
        let commands: Vec<_> = steps
            .iter()
            .map(|s| match s {
                Step::Run(p) => p.commands[0].clone(),
                _ => panic!(),
            })
            .collect();
        assert_eq!(
            commands,
            [
                (
                    "C:\\Games\\Skyrim\\_CommonRedist\\DotNet\\4.8\\Microsoft .NET Framework 4.8.cmd"
                        .to_owned(),
                    String::new()
                ),
                (
                    "msiexec".into(),
                    "/i \"C:\\Games\\Skyrim\\_CommonRedist\\XNA\\3.0\\xnafx30_redist.msi\" /quiet"
                        .into()
                ),
            ]
        );
    }

    #[test]
    fn registry_values_per_language_and_dwords() {
        let text = r#""InstallScript" { "Registry" {
            "HKEY_CURRENT_USER\\Software\\Studio\\Game"
            {
                "string" { "english" { "Language" "english" } "german" { "Language" "deutsch" } }
                "dword" { "Installed" "1" "Flags" "0x10" }
            }
        } }"#;
        let steps = steps(&script("Game"), text, &TestVars, "german").unwrap();
        let data: Vec<_> = steps
            .iter()
            .map(|s| match s {
                Step::Set(v) => (v.hive, v.name.clone(), v.data.clone()),
                _ => panic!(),
            })
            .collect();
        assert_eq!(
            data,
            [
                (
                    Hive::CurrentUser,
                    "Language".into(),
                    Data::Text("deutsch".into())
                ),
                (Hive::CurrentUser, "Installed".into(), Data::Dword(1)),
                (Hive::CurrentUser, "Flags".into(), Data::Dword(16)),
            ]
        );
        assert!(!steps[0].elevated());
    }

    #[test]
    fn entries_without_a_has_run_key_get_one_per_install() {
        let text = r#""InstallScript" { "Run Process" { "Setup" { "process 1" "%INSTALLDIR%\\setup.exe" } } }"#;
        let steps = steps(&script("Game"), text, &TestVars, "english").unwrap();
        let Step::Run(process) = &steps[0] else {
            panic!()
        };
        assert_eq!(process.ran.hive, Hive::LocalMachine);
        assert!(process.ran.key.starts_with("Software\\fumes\\FirstRun\\"));
        assert_eq!(process.ran.name, "Setup");
    }

    #[test]
    fn unknown_variables_drop_only_their_entry() {
        let text = r#""InstallScript" { "Run Process" {
            "Odd" { "process 1" "%STEAMPATH%\\x.exe" }
            "Fine" { "process 1" "%INSTALLDIR%\\y.exe" }
        } }"#;
        let steps = steps(&script("Game"), text, &TestVars, "english").unwrap();
        assert_eq!(steps.len(), 1);
    }

    #[test]
    fn parses_comments_conditionals_and_unescaped_paths() {
        let block = parse(
            "// comment\n\"a\" { \"path\" \"C:\\Games\\x\" [$WIN32] b c \"q\" \"say \\\"hi\\\"\" }",
        )
        .unwrap();
        let Node::Block(a) = &block[0].1 else {
            panic!()
        };
        assert_eq!(text(a, "path"), Some("C:\\Games\\x"));
        assert_eq!(text(a, "B"), Some("c"));
        assert_eq!(text(a, "q"), Some("say \"hi\""));
        assert!(parse("\"a\" { \"b\" \"c\"").is_err());
    }

    #[test]
    fn names_redistributables() {
        assert_eq!(
            redist_label("_CommonRedist/vcredist/2015/installscript.vdf"),
            "Visual C++ 2015"
        );
        assert_eq!(
            redist_label("_CommonRedist/DotNet/3.5 Client Profile/installscript.vdf"),
            ".NET 3.5 Client Profile"
        );
        assert_eq!(
            redist_label("_CommonRedist/DirectX/Jun2010/installscript.vdf"),
            "DirectX Jun2010"
        );
        assert_eq!(
            redist_label("installscript.vdf"),
            "Steamworks redistributables"
        );
    }

    #[test]
    fn reads_scripts_in_any_encoding() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("installscript.vdf");
        let text = "\"InstallScript\" { \"Copy Folders\" { } } // Café";
        let mut utf16 = vec![0xff, 0xfe];
        utf16.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        std::fs::write(&path, &utf16).unwrap();
        assert_eq!(read_script(&path).unwrap(), text);
        std::fs::write(&path, b"// Caf\xe9").unwrap();
        assert_eq!(read_script(&path).unwrap(), "// Café");
        assert!(read_script(&dir.path().join("none")).is_none());
    }

    #[test]
    fn notices_installers_that_are_missing() {
        let run = |program: &str| {
            Step::Run(Process {
                label: String::new(),
                ran: Value {
                    hive: Hive::LocalMachine,
                    key: "k".into(),
                    name: "n".into(),
                    data: Data::Dword(1),
                },
                commands: vec![(program.into(), String::new())],
                ignore_exit_code: false,
            })
        };
        let here = std::env::current_exe().unwrap();
        assert_eq!(run(here.to_str().unwrap()).missing_program(), None);
        assert_eq!(run("msiexec").missing_program(), None);
        let gone = here.with_file_name("not-there.exe");
        assert!(run(gone.to_str().unwrap()).missing_program().is_some());
    }

    #[test]
    fn exit_codes_that_mean_it_worked() {
        assert!(succeeded(0) && succeeded(3010) && succeeded(1638));
        assert!(!succeeded(1603) && !succeeded(-1));
    }

    #[test]
    fn copies_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("from"), dir.path().join("to"));
        std::fs::create_dir_all(from.join("sub")).unwrap();
        std::fs::write(from.join("Skyrim.ini"), "default").unwrap();
        std::fs::write(from.join("sub/x"), "x").unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(to.join("Skyrim.ini"), "mine").unwrap();
        copy_missing(&from, &to).unwrap();
        assert_eq!(
            std::fs::read_to_string(to.join("Skyrim.ini")).unwrap(),
            "mine"
        );
        assert_eq!(std::fs::read_to_string(to.join("sub/x")).unwrap(), "x");
        copy_missing(&dir.path().join("nothing"), &to).unwrap();
    }
}
