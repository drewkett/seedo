# seedo

`seedo` (short for "Monkey See, Monkey Do") is a simple program for recursively
watching a directory for file system events and running a command when they
occur. It will debounce filesystem events based a configurable time parameter.
It respects `.gitignore` files using the `ignore` crate.

A basic example to use with `cargo check` would be

```sh
seedo cargo check
```

This will run `cargo check` within 50ms of a file system change within the
current directory (recursively). It will not trigger `cargo check` if a file is
listed in a `.gitignore` file.

The current command line options are as follows

```text
$ seedo --help
seedo

USAGE:
    seedo [OPTIONS] <COMMAND> [ARGS]...

ARGS:
    <COMMAND>    Command to run
    <ARGS>...    Args for command

OPTIONS:
    -d, --debounce <DEBOUNCE_MS>    Debounce time in milliseconds [default: 50]
    -h, --help                      Print help information
    -p, --path <PATH>               Paths to watch [default: .]
        --skip-ignore-files         Don't read .gitignore files
```

## Installation

```
cargo install seedo
```
