use std::{
    ffi::{OsStr, OsString},
    future::Future,
    path::PathBuf,
    process::exit,
    time::{Duration, Instant},
};

use anyhow::{bail, Context};
use clap::Parser;
use ignore::WalkBuilder;
use notify::{event::ModifyKind, Event, EventKind, FsEventWatcher, RecursiveMode, Watcher};
use tokio::{
    process::Command,
    time::{error::Elapsed, timeout},
};
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
async fn run_command(command: &OsStr, args: &[OsString]) {
    let res = Command::new(command).args(args).status().await;
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

    /// This mimics the [`tokio::time::timeout`] interface. If the timer is
    /// running, calculate the remaining duration until the timer is finished,
    /// and pass that to [`tokio::time::timeout`] along with the future. If the
    /// timer is stopped, this will simply call `await` on the given [`Future`]
    async fn timeout<F: Future>(&self, fut: F) -> Result<F::Output, Elapsed> {
        match self.start {
            Some(start) => {
                let duration = self.duration.saturating_sub(start.elapsed());
                timeout(duration, fut).await
            }
            None => Ok(fut.await),
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

/// This takes an [`notify::Event`] and calls [`FsEventWatcher::watch`] on any
/// newly created files.
fn watch_new_files(watcher: &mut FsEventWatcher, event: &Event) {
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
#[clap(trailing_var_arg = true)]
struct Opts {
    /// Debounce time in milliseconds
    #[clap(short, long = "debounce", default_value_t = 50)]
    debounce_ms: u64,
    /// Don't read .gitignore files
    #[clap(long)]
    skip_ignore_files: bool,
    /// Paths to watch
    #[clap(short, long, default_value = ".")]
    path: Vec<PathBuf>,
    /// Command to run with any arguments
    #[clap(required = true)]
    command_to_run: Vec<OsString>,
}

#[tokio::main]
async fn try_main(opts: Opts) -> anyhow::Result<()> {
    let (snd, mut rcv) = tokio::sync::mpsc::unbounded_channel();
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

    if opts.command_to_run.is_empty() {
        bail!("No command given")
    }
    let mut command_to_run = opts.command_to_run;
    let command = command_to_run.remove(0);
    let args = command_to_run;

    // Use `ignore` to walk all given paths to add watch events.
    let mut path_iter = opts.path.iter();
    let mut walk_builder = WalkBuilder::new(path_iter.next().expect("at least one path required"));
    for path in path_iter {
        walk_builder.add(path);
    }
    if opts.skip_ignore_files {
        walk_builder
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false);
    }
    for result in walk_builder.build() {
        let entry = result?;
        debug!("watching '{}'", entry.path().display());
        watcher.watch(entry.path(), RecursiveMode::NonRecursive)?;
    }

    let mut debounce = DebounceTimer::new(Duration::from_millis(opts.debounce_ms));
    loop {
        match debounce.timeout(rcv.recv()).await {
            Ok(Some(event)) => {
                debug!("{:?}", event);
                // We need to watch newly created files for changes.
                if let EventKind::Create(_) = event.kind {
                    watch_new_files(&mut watcher, &event);
                }
                debounce.start_if_stopped();
                continue;
            }
            // This indicates the channel has closed. It shouldn't happen, but
            // in case it does break from the loop and exit the program.
            Ok(None) => {
                warn!("event channel closed");
                break;
            }
            Err(_) => {
                debug!("timeout reached. running command");
            }
        };
        debounce.stop();
        run_command(&command, &args).await;
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
