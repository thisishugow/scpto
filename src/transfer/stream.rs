//! Single-file stream copy from any `Fs` to any `Fs`. Remote-to-remote
//! copies relay through local memory without touching disk; same-endpoint
//! remote copies use `copy_within` when the server supports it. Writes to
//! `<name>.scpto-part`, renames to the final name on completion, then sets
//! mtime and permissions (`mode & 0o777`).
