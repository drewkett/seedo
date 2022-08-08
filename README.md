# watchme

This is a simple program for recursively watching a directory for file system
events and running a command when that happens. It will debounce filesystem
events based a configurable parameter. It respects `.gitignore` files usng the
`ignore` crate.
