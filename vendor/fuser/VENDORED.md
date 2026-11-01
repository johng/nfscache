# Vendored fuser 0.17.0 (patched)

Upstream: https://crates.io/crates/fuser/0.17.0

Only change: on macOS, `src/mnt/fuse2.rs` mounts with `fuse_mount()` +
`fuse_chan_fd()` instead of `fuse_mount_compat25()`, and `src/mnt/fuse2_sys.rs`
declares those two functions. macFUSE >= 5.3.0 stubs `fuse_mount_compat25()` to
always return -1 (errno untouched), so stock fuser cannot mount.
See https://github.com/cberner/fuser/issues/752

Known limitation: the libfuse channel returned by `fuse_mount()` is not
destroyed at unmount (leaked for the life of the process). Fine for a
long-lived mount.

Drop this directory and the `[patch.crates-io]` entry in the top-level
Cargo.toml once fuser ships a fix for the issue above.
