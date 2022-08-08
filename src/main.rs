use std::{
    ffi::{OsStr, OsString},
    future::Future,
    path::PathBuf,
    process::exit,
    time::{Duration, Instant},
};

use anyhow::Context;
use clap::Parser;
use ignore::Walk;
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
    match event.kind {
        // Drop metadata change events
        EventKind::Modify(ModifyKind::Metadata(_)) => false,
        // Drop access events
        EventKind::Access(_) => false,
        // Unsure if this needs to be handled
        // EventKind::Modify(ModifyKind::Name(_)) => None,
        EventKind::Modify(_) => true,
        EventKind::Create(_) => true,
        EventKind::Remove(_) => true,
        EventKind::Other => true,
        EventKind::Any => true,
    }
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
            error!("{:#}", e)
        }
    }
}

/// A helper struct to implement debouncing. It takes a [`Duration`] which
/// indicates the time to debounce for when run.
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
        self.start = None
    }

    /// Starts the timer if it wasn't previously started.
    fn start_if_stopped(&mut self) {
        if self.start.is_none() {
            self.start = Some(Instant::now())
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
                    error!("Failed to watch {}: {:#}", path.display(), e)
                }
            }
        }
    }
}

#[derive(Parser)]
#[clap(trailing_var_arg = true)]
struct Args {
    /// Debounce time in milliseconds
    #[clap(short, long = "debounce", default_value_t = 50)]
    debounce_ms: u64,
    /// Paths to watch
    #[clap(short, long, default_value = ".")]
    path: Vec<PathBuf>,
    /// Command to run
    command: OsString,
    /// Args for command
    args: Vec<OsString>,
}

#[tokio::main]
async fn try_main(args: Args) -> anyhow::Result<()> {
    let (snd, mut rcv) = tokio::sync::mpsc::unbounded_channel();
    // Initialize fs watcher
    let mut watcher = notify::recommended_watcher(move |res| match res {
        Ok(event) => {
            if should_event_trigger(&event) {
                if let Err(e) = snd.send(event) {
                    error!("send error: {:?}", e)
                }
            }
        }
        Err(e) => error!("watch error: {:?}", e),
    })?;

    // Use `ignore` to walk all given paths to add watch events.
    for path in &args.path {
        for result in Walk::new(&path) {
            let entry = result?;
            debug!("watching '{}'", entry.path().display());
            watcher.watch(entry.path(), RecursiveMode::NonRecursive)?;
        }
    }

    let mut debounce = DebounceTimer::new(Duration::from_millis(args.debounce_ms));
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
                debug!("timeout reached. running command")
            }
        };
        debounce.stop();
        run_command(&args.command, &args.args).await;
    }

    Ok(())
}

fn main() {
    let args = Args::parse();
    tracing_subscriber::fmt().init();
    if let Err(e) = try_main(args) {
        eprintln!("{:#?}", e);
        exit(1)
    }
}
