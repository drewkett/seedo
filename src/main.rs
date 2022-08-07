use std::{
    ffi::OsString,
    process::exit,
    time::{Duration, Instant},
};

use anyhow::Context;
use clap::Parser;
use ignore::Walk;
use notify::{EventKind, RecursiveMode, Watcher};
use tokio::{process::Command, time::timeout};
use tracing::{debug, error, metadata::LevelFilter};

#[derive(Parser)]
#[clap(trailing_var_arg = true)]
struct Args {
    #[clap(short, long, default_value_t = 1)]
    debounce: u64,
    command: OsString,
    args: Vec<OsString>,
}

#[tokio::main]
async fn try_main(args: Args) -> anyhow::Result<()> {
    let (snd, mut rcv) = tokio::sync::mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |res| match res {
        Ok(event) => match snd.send(event) {
            Ok(_) => {}
            Err(e) => error!("send error: {:?}", e),
        },
        Err(e) => error!("watch error: {:?}", e),
    })?;

    for result in Walk::new(".") {
        let entry = result?;
        debug!("watching '{}'", entry.path().display());
        watcher.watch(entry.path(), RecursiveMode::NonRecursive)?;
    }

    let mut debounce_start = Instant::now();
    let mut debounce_remaining = Duration::MAX;
    loop {
        match timeout(debounce_remaining, rcv.recv()).await {
            Ok(Some(event)) => {
                debug!("{:?}", event);
                if debounce_remaining == Duration::MAX {
                    debug!("updating timeout");
                    debounce_start = Instant::now();
                    debounce_remaining = Duration::from_secs(args.debounce);
                } else {
                    debug!("updating timeout");
                    debounce_remaining =
                        Duration::from_secs(args.debounce) - debounce_start.elapsed();
                }
                match event.kind {
                    EventKind::Create(_) => {
                        for path in event.paths {
                            if path.exists() {
                                debug!("watching '{}'", path.display());
                                if let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive) {
                                    error!("Failed to watch {}: {:#}", path.display(), e)
                                }
                            }
                        }
                    }
                    _ => {}
                }
                continue;
            }
            Ok(None) => break,
            Err(_) => {
                debug!("timeout reached. running command")
            }
        };
        // Reset throttle
        debounce_remaining = Duration::MAX;
        let res = Command::new(&args.command).args(&args.args).status().await;
        match res {
            Ok(status) if status.success() => {}
            Ok(status) => match status.code() {
                Some(code) => error!("commanded exited with code = {code}"),
                None => error!("command exited without code"),
            },
            Err(e) => {
                error!("{:#?}", Err::<(), _>(e).context("command failed to launch"))
            }
        }
    }

    Ok(())
}

fn main() {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_max_level(LevelFilter::DEBUG)
        .init();
    if let Err(e) = try_main(args) {
        eprintln!("{:#?}", e);
        exit(1)
    }
}
