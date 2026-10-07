//! A game running in the background while the UI stays up: `fumes launch`
//! as a child process, its output read line by line, and its question
//! about conflicting cloud saves answered from the UI.
//!
//! The child knows it's run this way from `UI_ENV`. It marks its own
//! progress lines (`report`) so they can be told apart from whatever the
//! game prints, and asks about cloud conflicts with `CONFLICT_PROMPT`,
//! reading the answer from its stdin.

use std::collections::VecDeque;
use std::fmt::Display;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc;

use anyhow::{Context, Result};

pub const UI_ENV: &str = "FUMES_UI";
/// Asks the UI which saves to keep; answered with `c`, `l` or nothing.
pub const CONFLICT_PROMPT: &str = "\u{1e}conflict?";
/// Starts a progress line meant for the UI.
const MARK: char = '\u{1e}';
/// Lines of output kept for showing.
const KEPT: usize = 8;
/// How long to wait for the last lines once the session is over.
const DRAIN: std::time::Duration = std::time::Duration::from_millis(300);

/// A progress line: marked for the UI when it runs this, plain otherwise.
pub fn report(line: impl Display) {
    if std::env::var_os(UI_ENV).is_some() {
        println!("{MARK}{line}");
    } else {
        println!("{line}");
    }
}

/// `report`, for warnings: to stderr on the command line.
pub fn report_problem(line: impl Display) {
    if std::env::var_os(UI_ENV).is_some() {
        println!("{MARK}{line}");
    } else {
        eprintln!("{line}");
    }
}

pub fn from_ui() -> bool {
    std::env::var_os(UI_ENV).is_some()
}

pub struct Running {
    pub appid: u32,
    pub title: String,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
    /// fumes' latest progress line ("Syncing cloud saves…").
    pub state: String,
    /// The latest output, game's included, oldest first.
    pub log: VecDeque<String>,
    /// What went wrong, if fumes said (its final `Error:` line).
    pub error: Option<String>,
}

/// What came out of the game's session since the last look.
#[derive(Debug, Default, PartialEq)]
pub struct News {
    /// It's asking which cloud saves to keep.
    pub conflict: bool,
}

impl Running {
    /// `args` are `fumes` arguments (`launch <appid> [-- …]`).
    pub fn start(appid: u32, title: String, args: &[String]) -> Result<Running> {
        let exe = std::env::current_exe().context("finding fumes' own executable")?;
        let mut command = Command::new(exe);
        command
            .args(args)
            .env(UI_ENV, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Its own process group: Ctrl-C in a command the UI hands the
        // terminal to meanwhile mustn't reach the game.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("starting {title}"))?;

        let (tx, lines) = mpsc::channel();
        let outputs: [Option<Box<dyn Read + Send>>; 2] = [
            child.stdout.take().map(|o| Box::new(o) as _),
            child.stderr.take().map(|o| Box::new(o) as _),
        ];
        for output in outputs.into_iter().flatten() {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(output).lines() {
                    let Ok(line) = line else { break };
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            });
        }
        Ok(Running {
            appid,
            state: "Starting…".into(),
            stdin: child.stdin.take(),
            child,
            title,
            lines,
            log: VecDeque::new(),
            error: None,
        })
    }

    /// Take in what it printed since the last look.
    pub fn poll(&mut self) -> News {
        let mut news = News::default();
        while let Ok(line) = self.lines.try_recv() {
            news.conflict |= self.take(line);
        }
        news
    }

    /// After it's finished: wait a moment for its last lines (the error,
    /// if any) to come through. Something it started may hold the output
    /// open, so this doesn't wait for the end of it.
    pub fn drain(&mut self) {
        let until = std::time::Instant::now() + DRAIN;
        while let Some(left) = until.checked_duration_since(std::time::Instant::now())
            && let Ok(line) = self.lines.recv_timeout(left)
        {
            self.take(line);
        }
    }

    /// One line of output. True if it's the cloud conflict question.
    fn take(&mut self, line: String) -> bool {
        if line == CONFLICT_PROMPT {
            return true;
        }
        match line.strip_prefix(MARK) {
            Some(progress) => self.state = progress.to_owned(),
            None if line.starts_with("Error: ") && self.error.is_none() => {
                self.error = Some(line["Error: ".len()..].to_owned());
            }
            None => {}
        }
        if self.log.len() == KEPT {
            self.log.pop_front();
        }
        self.log.push_back(line.trim_start_matches(MARK).to_owned());
        false
    }

    /// Settle a cloud conflict: keep the local saves (`Some(true)`), the
    /// cloud's (`Some(false)`), or neither and don't play (`None`).
    pub fn answer(&mut self, keep_local: Option<bool>) {
        let answer = match keep_local {
            Some(true) => "l\n",
            Some(false) => "c\n",
            None => "\n",
        };
        if let Some(stdin) = &mut self.stdin {
            stdin.write_all(answer.as_bytes()).ok();
            stdin.flush().ok();
        }
    }

    /// `Some` once the session is over (the game closed and its saves went
    /// up, or it never started).
    pub fn finished(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }
}

/// A stand-in session with `lines` as its output, for tests.
#[cfg(test)]
pub fn fake(appid: u32, title: &str, lines: &[&str]) -> Running {
    let (tx, rx) = mpsc::channel();
    for line in lines {
        tx.send((*line).to_owned()).unwrap();
    }
    // A real child, so the struct is whole; `true` exits at once.
    let child = Command::new("true").spawn().unwrap();
    Running {
        appid,
        title: title.into(),
        child,
        stdin: None,
        lines: rx,
        state: String::new(),
        log: VecDeque::new(),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(lines: &[&str]) -> Running {
        fake(620, "Portal 2", lines)
    }

    #[test]
    fn tells_progress_from_game_output_and_spots_the_conflict_question() {
        let mut game = running(&[
            "\u{1e}Signing in to Steam…",
            "[game] loading shaders",
            "\u{1e}Syncing cloud saves…",
            CONFLICT_PROMPT,
        ]);
        assert_eq!(game.poll(), News { conflict: true });
        assert_eq!(game.state, "Syncing cloud saves…");
        assert_eq!(game.log.len(), 3);

        let mut failed = running(&["Error: the Steam engine didn't start Portal 2", "more"]);
        failed.poll();
        assert_eq!(
            failed.error.as_deref(),
            Some("the Steam engine didn't start Portal 2")
        );
    }
}
