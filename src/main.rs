use std::{
    ffi::{OsStr, OsString},
    fs::read_to_string,
    path::{Path, PathBuf},
    process::{exit, Command},
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use clap::Parser;
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError};
use glob::{glob, Pattern};
use ignore::WalkBuilder;
use notify::{event::ModifyKind, Event, EventKind, RecursiveMode, Watcher};
use path_absolutize::Absolutize;
use serde::Deserialize;
use tracing::{debug, error, warn};

/// Filter out events that should not trigger re-running the command. This takes
/// an &[`Event`] and returns a [`bool`] which indicates whether the `event`
/// should trigger running the command.
fn should_event_trigger(event: &Event) -> bool {
    !matches!(
        event.kind,
        EventKind::Modify(ModifyKind::Metadata(_)) | EventKind::Access(_)
    )
}

/// Run the specified command + args and log any errors that occur.
#[allow(dead_code)]
fn run_command(command: &OsStr, args: &[OsString]) {
    let res = Command::new(command).args(args).status();
    match res.context("command failed to launch") {
        Ok(status) if status.success() => {}
        Ok(status) => match status.code() {
            Some(code) => error!("command exited with code = {code}"),
            None => error!("command exited without code"),
        },
        Err(e) => {
            error!("{:#}", e);
        }
    }
}

/// A helper struct to implement debouncing. It takes a [`Duration`] which
/// indicates the time to wait after a new event before running the command.
struct DebounceTimer {
    start: Option<Instant>,
    duration: Duration,
}

impl DebounceTimer {
    /// Create a [`DebounceTimer`] struct. `duration` is the length of time to
    /// debounce for when using the timer.
    fn new(duration: Duration) -> DebounceTimer {
        DebounceTimer {
            start: None,
            duration,
        }
    }

    fn calculate_timeout(&self) -> Option<Duration> {
        self.start
            .map(|start| self.duration.saturating_sub(start.elapsed()))
    }

    // /// This mimics the [`crossbeam_channel::Receiver::recv_timeout`] behavior
    // /// except that it falls back to [`crossbeam_channel::Receiver::recv`] if
    // /// the timer has not been started.
    // fn timeout(&self, receiver: &Receiver<Event>) -> Result<Event, RecvTimeoutError> {
    //     match self.calculate_timeout() {
    //         Some(duration) => receiver.recv_timeout(duration),
    //         None => receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
    //     }
    // }

    /// Stops the timer.
    fn expired(&self) -> bool {
        match self.start {
            Some(start) => Instant::now() > start + self.duration,
            None => false,
        }
    }

    /// Stops the timer.
    fn stop(&mut self) {
        self.start = None;
    }

    /// Starts the timer if it wasn't previously started.
    fn start_if_stopped(&mut self) {
        if self.start.is_none() {
            self.start = Some(Instant::now());
        }
    }
}

struct DebounceTimerSet {
    timers: Vec<DebounceTimer>,
}

impl DebounceTimerSet {
    fn calculate_timeout(&self) -> Option<Duration> {
        let mut duration = None;
        for timer in &self.timers {
            if let Some(timer_duration) = timer.calculate_timeout() {
                match &mut duration {
                    Some(duration) => *duration = std::cmp::min(*duration, timer_duration),
                    None => duration = Some(timer_duration),
                }
            };
        }
        duration
    }
    /// This mimics the [`crossbeam_channel::Receiver::recv_timeout`] behavior
    /// except that it falls back to [`crossbeam_channel::Receiver::recv`] if
    /// the timer has not been started.
    fn timeout(&self, receiver: &Receiver<Event>) -> Result<Event, RecvTimeoutError> {
        match self.calculate_timeout() {
            Some(duration) => receiver.recv_timeout(duration),
            None => receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
        }
    }
}

/// This takes an [`notify::Event`] and calls [`Watcher::watch`] on any
/// newly created files.
fn watch_new_files(watcher: &mut impl Watcher, event: &Event) {
    if let EventKind::Create(_create_kind) = event.kind {
        for path in &event.paths {
            if path.exists() {
                debug!("watching '{}'", path.display());
                if let Err(e) = watcher.watch(path, RecursiveMode::NonRecursive) {
                    error!("Failed to watch {}: {:#}", path.display(), e);
                }
            }
        }
    }
}

/// seedo recursively watches a specified directory for file system events,
/// which then triggers a specified command to be executed. By default, seedo
/// will respect .gitignore files. The wait time after seeing a file system
/// event is the configurable debounce time.
#[derive(Parser)]
#[clap(author, version, trailing_var_arg = true)]
struct Opts {
    /// Debounce time in milliseconds
    #[clap(short, long = "debounce", default_value_t = 50)]
    debounce_ms: u64,
    /// don't read .gitignore files
    #[clap(long)]
    skip_ignore_files: bool,
    /// seedo.toml file
    #[clap(long, default_value = "seedo.toml")]
    config: PathBuf,
    /// Paths to watch
    #[clap(short, long, default_value = "**")]
    glob: Vec<String>,
    /// Command to run with any arguments
    command_to_run: Vec<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CommandToRun {
    Vec(Vec<String>),
    String(String),
}

impl CommandToRun {
    fn to_command(&self) -> Result<Command> {
        match self {
            CommandToRun::Vec(v) => command_from_iter(v),
            CommandToRun::String(s) => command_from_str(s),
        }
    }
}

fn command_from_iter(iter: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Result<Command> {
    let mut iter = iter.into_iter();
    let mut command = match iter.next() {
        Some(command) => Command::new(command),
        None => bail!("no command given"),
    };
    command.args(iter);
    Ok(command)
}

#[allow(dead_code)]
fn command_from_str(s: &str) -> Result<Command> {
    match shlex::split(s) {
        Some(items) => command_from_iter(items),
        None => bail!("no command given"),
    }
}

#[derive(Deserialize)]
struct SeedoToml {
    seedo: Vec<SeedoConfig>,
}

fn default_debounce_ms() -> u64 {
    50
}

#[derive(Deserialize)]
struct SeedoConfig {
    command_to_run: CommandToRun,
    globs: Vec<String>,
    #[serde(default)]
    skip_ignore_files: bool,
    #[serde(default = "default_debounce_ms")]
    debounce_ms: u64,
}

struct Seedo {
    command: Command,
    patterns: Vec<Pattern>,
}

// TODO glob executes before walkdir which reads gitignore

#[allow(clippy::needless_pass_by_value)]
fn try_main(opts: Opts) -> anyhow::Result<()> {
    let (snd, rcv) = unbounded();
    // Initialize fs watcher
    let mut watcher = notify::recommended_watcher(move |res| match res {
        Ok(event) => {
            if should_event_trigger(&event) {
                if let Err(e) = snd.send(event) {
                    error!("send error: {:?}", e);
                }
            }
        }
        Err(e) => error!("watch error: {:?}", e),
    })?;

    let mut configs = vec![];
    if !opts.command_to_run.is_empty() {
        configs.push(SeedoConfig {
            command_to_run: CommandToRun::Vec(opts.command_to_run.clone()),
            globs: opts.glob.clone(),
            skip_ignore_files: opts.skip_ignore_files,
            debounce_ms: opts.debounce_ms,
        });
    } else {
        let config_bytes = read_to_string(&opts.config)?;
        let seedo_toml: SeedoToml = toml::from_str(&config_bytes)?;
        configs.extend(seedo_toml.seedo);
    }

    let mut seedos = vec![];
    let mut debounce_timers = vec![];
    for config in &configs {
        let command = config.command_to_run.to_command()?;
        let mut abs_globs = vec![];
        for glob in &config.globs {
            let p = Path::new(glob).absolutize()?;
            abs_globs.push(p.to_string_lossy().to_string());
        }
        let mut path_iter = abs_globs
            .iter()
            .filter_map(|p| glob(&p).ok())
            .flatten()
            .filter_map(Result::ok);
        let mut walk_builder = match path_iter.next() {
            Some(path) => WalkBuilder::new(path),
            None => bail!("no paths given"),
        };
        for path in path_iter {
            walk_builder.add(path);
        }
        if config.skip_ignore_files {
            walk_builder
                .ignore(false)
                .git_ignore(false)
                .git_global(false)
                .git_exclude(false);
        }
        for result in walk_builder.build() {
            let entry = result?;
            println!("watching '{}'", entry.path().display());
            watcher.watch(entry.path(), RecursiveMode::NonRecursive)?;
        }

        let mut patterns = vec![];
        for glob in abs_globs {
            patterns.push(Pattern::new(&glob)?);
        }

        debounce_timers.push(DebounceTimer::new(Duration::from_millis(
            config.debounce_ms,
        )));
        seedos.push(Seedo { command, patterns });
    }

    let mut debounce_timer = DebounceTimerSet {
        timers: debounce_timers,
    };

    loop {
        match debounce_timer.timeout(&rcv) {
            Ok(event) => {
                debug!("{:?}", event);
                // We need to watch newly created files for changes.
                if let EventKind::Create(_) = event.kind {
                    watch_new_files(&mut watcher, &event);
                }
                'outer: for (timer, seedo) in
                    debounce_timer.timers.iter_mut().zip(seedos.iter_mut())
                {
                    for path in &event.paths {
                        println!("event path {}", path.display());
                        for pattern in &seedo.patterns {
                            println!("pattern {}", pattern);
                            if pattern.matches_path(path) {
                                println!("start if stopped on {:?}", seedo.command);
                                timer.start_if_stopped();
                                continue 'outer;
                            }
                        }
                    }
                }
                continue;
            }
            // This indicates the channel has closed. It shouldn't happen, but
            // in case it does break from the loop and exit the program.
            Err(RecvTimeoutError::Disconnected) => {
                warn!("event channel closed");
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                debug!("timeout reached. running command");
            }
        };
        for (timer, seedo) in debounce_timer.timers.iter_mut().zip(seedos.iter_mut()) {
            if timer.expired() {
                timer.stop();
                let res = seedo.command.status();
                match res.context("command failed to launch") {
                    Ok(status) if status.success() => {}
                    Ok(status) => match status.code() {
                        Some(code) => error!("command exited with code = {code}"),
                        None => error!("command exited without code"),
                    },
                    Err(e) => {
                        error!("{:#}", e);
                    }
                }
            }
        }
    }

    Ok(())
}

fn main() {
    let args = Opts::parse();
    tracing_subscriber::fmt().init();
    if let Err(e) = try_main(args) {
        eprintln!("{:#?}", e);
        exit(1)
    }
}
