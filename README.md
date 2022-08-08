# seedo

`seedo` (short for "Monkey See, Monkey Do") is a simple program for recursively watching a directory for file system
events and running a command when they occur. It will debounce filesystem
events based a configurable time parameter. It respects `.gitignore` files
using the `ignore` crate.
