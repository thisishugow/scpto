//! tar acceleration between the local machine and a non-sudo remote: the
//! `tar` crate packs or unpacks locally through `tokio_util::io::SyncIoBridge`
//! in `spawn_blocking` (symlinks not followed; absolute and `..` entries
//! rejected), streaming through remote `tar -C <dir> -xf -` / `-cf -`.
