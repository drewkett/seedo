use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
};

use anyhow::{bail, ensure, Context, Result};
use notify::{event::CreateKind, Event, EventKind, RecursiveMode, Watcher};
use os_str_bytes::OsStrBytes;
use path_absolutize::Absolutize;
use regex::bytes::Regex;

#[derive(Debug)]
struct Pattern {
    file_regex: Regex,
    directory_regex: Regex,
    base_directory: PathBuf,
}

impl Pattern {
    fn new(pattern: &str) -> Result<Pattern> {
        println!("pattern = {}", pattern);
        let mut file_regex = String::new();
        let mut directory_regex = String::new();
        let mut directory_finished = false;
        let mut directory_closing_parens = 0;
        let abs_pattern = Path::new(pattern).absolutize()?;
        let mut components = abs_pattern.components().peekable();
        let base_directory: PathBuf = components
            .clone()
            .take_while(|c| match c {
                Component::Normal(c) => c.to_str().map(|c| !c.contains('*')).unwrap_or(true),
                _ => true,
            })
            .collect();
        while let Some(comp) = components.next() {
            match comp {
                Component::CurDir => unreachable!(),
                Component::ParentDir => unreachable!(),
                Component::Prefix(_prefix) => unimplemented!(),
                Component::RootDir => {
                    ensure!(file_regex.is_empty(), "root dir found not at beginning");
                    file_regex.push('/');
                    directory_regex.push('/');
                }
                Component::Normal(comp) => {
                    let comp = comp.to_str().context("invalid utf8")?;
                    if "**" == comp {
                        file_regex.push_str(&comp.replace("**", "[^/]+(/[^/])*"));
                        if !directory_finished {
                            directory_regex.push_str(".*");
                            directory_finished = true;
                        }
                        continue;
                    }
                    if comp.contains("**") {
                        bail!("invalid pattern")
                    }
                    if comp.chars().any(|c| {
                        matches!(c, '[' | ']' | '(' | ')' | '+' | '$' | '^' | '{' | '}' | '|')
                    }) {
                        bail!("invalid pattern")
                    }
                    let pattern = comp.replace('*', "[^/]*");
                    file_regex.push_str(&pattern);
                    if !directory_finished {
                        directory_regex.push_str("(:?");
                        directory_closing_parens += 1;
                        directory_regex.push_str(&pattern);
                    }
                    if components.peek().is_some() {
                        file_regex.push('/');
                        if !directory_finished {
                            directory_regex.push_str("(:?");
                            directory_closing_parens += 1;
                            directory_regex.push('/');
                        }
                    }
                }
            }
        }
        for _ in 0..directory_closing_parens {
            directory_regex.push_str(")?");
        }
        println!("file_regex = {}", file_regex);
        println!("directory_regex = {}", directory_regex);
        Ok(Pattern {
            file_regex: Regex::new(&file_regex)?,
            directory_regex: Regex::new(&directory_regex)?,
            base_directory,
        })
    }
}

#[derive(Debug)]
pub struct GlobWatcher {
    patterns: Vec<Pattern>,
    read_ignores: bool,
}

impl GlobWatcher {
    pub fn matches(&self, path: impl AsRef<Path>) -> Result<bool> {
        let path = path.as_ref().absolutize()?;
        if path.is_dir() {
            for pattern in &self.patterns {
                if pattern
                    .directory_regex
                    .is_match(path.to_raw_bytes().as_ref())
                {
                    return Ok(true);
                }
            }
        } else {
            for pattern in &self.patterns {
                if pattern.file_regex.is_match(path.to_raw_bytes().as_ref()) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    pub fn from_pattern(pattern: &str, read_ignores: bool) -> Result<GlobWatcher> {
        GlobWatcher::from_patterns(&[pattern], read_ignores)
    }

    pub fn from_patterns(
        patterns: impl IntoIterator<Item = impl AsRef<str>>,
        read_ignores: bool,
    ) -> Result<GlobWatcher> {
        let patterns: Result<Vec<Pattern>> = patterns
            .into_iter()
            .map(|p| Pattern::new(p.as_ref()))
            .collect();
        let patterns = patterns?;
        let base_directories: BTreeSet<PathBuf> =
            patterns.iter().map(|p| p.base_directory.clone()).collect();
        let mut last_dir_opt = None;
        let base_directories: Vec<_> = base_directories
            .into_iter()
            .filter(|p| match &mut last_dir_opt {
                Some(last_dir) => {
                    if p.starts_with(last_dir) {
                        false
                    } else {
                        last_dir_opt = Some(p.clone());
                        true
                    }
                }
                None => {
                    last_dir_opt = Some(p.clone());
                    true
                }
            })
            .collect();
        let (watch_filter_snd, watch_filter_rcv) = crossbeam_channel::unbounded();
        let (result_snd, result_rcv) = crossbeam_channel::unbounded();
        let mut watcher =
            notify::recommended_watcher(move |res: Result<Event, notify::Error>| match res {
                Ok(event) => match event.kind {
                    EventKind::Access(_) | EventKind::Other => {}
                    _ => {
                        let _ = watch_filter_snd.send(event);
                    }
                    _ => {}
                },
                Err(e) => println!("watch error: {:?}", e),
            })?;
        let mut base_iter = base_directories.iter();
        if read_ignores {
            let first_dir = base_iter.next().context("no based directory")?;
            let mut builder = ignore::WalkBuilder::new(first_dir);
            for p in base_iter {
                builder.add(p);
            }
            for item in builder.build() {
                let item = item?;
                watcher.watch(item.path(), RecursiveMode::NonRecursive)?;
            }
        } else {
            for p in base_iter {
                watcher.watch(p, RecursiveMode::Recursive)?;
            }
        }
        std::thread::spawn(move || {
            while let Ok(event) = watch_filter_rcv.recv() {
                match event.kind {
                    EventKind::Create(CreateKind::File) => {
                        for p in &event.paths {
                            let _ = watcher.watch(p, RecursiveMode::NonRecursive);
                        }
                    }
                    EventKind::Create(CreateKind::Folder) => {
                        for p in &event.paths {
                            let _ = watcher.watch(p, RecursiveMode::NonRecursive);
                        }
                    }
                    EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_) => {}
                    _ => {}
                }
                let _ = result_snd.send(event);
            }
        });
        Ok(GlobWatcher {
            patterns,
            read_ignores,
        })
    }
}
