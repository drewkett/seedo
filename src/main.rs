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
            Err(e) => eprintln!("send error: {:?}", e),
        },
        Err(e) => eprintln!("watch error: {:?}", e),
    })?;

    for result in Walk::new(".") {
        let entry = result?;
        watcher.watch(entry.path(), RecursiveMode::NonRecursive)?;
    }

    let mut throttle_start = Instant::now();
    let mut throttle_remaining = Duration::MAX;
    loop {
        match timeout(throttle_remaining, rcv.recv()).await {
            Ok(Some(event)) => {
                if throttle_remaining == Duration::MAX {
                    throttle_start = Instant::now();
                    throttle_remaining = Duration::from_secs(args.debounce);
                } else {
                    throttle_remaining =
                        Duration::from_secs(args.debounce) - throttle_start.elapsed();
                }
                match event.kind {
                    EventKind::Create(_) => {
                        for path in event.paths {
                            if path.exists() {
                                if let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive) {
                                    eprintln!("Failed to watch {}: {:#}", path.display(), e)
                                }
                            }
                        }
                    }
                    _ => {}
                }
                continue;
            }
            Ok(None) => break,
            Err(_) => {}
        };
        let res = Command::new(&args.command).args(&args.args).status().await;
        match res {
            Ok(status) if status.success() => {}
            Ok(status) => match status.code() {
                Some(code) => eprintln!("commanded exited with code = {code}"),
                None => eprintln!("command exited without code"),
            },
            Err(e) => {
                eprintln!("{:#?}", Err::<(), _>(e).context("command failed to launch"))
            }
        }
    }

    Ok(())
}

fn main() {
    let args = Args::parse();
    if let Err(e) = try_main(args) {
        eprintln!("{:#?}", e);
        exit(1)
    }
}
