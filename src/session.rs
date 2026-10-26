//! One connection per remote endpoint: the per-process ControlMaster
//! (`/tmp/scpto-<uid>/<pid>/%C`, directory mode 0700), multiplexed
//! background children with BatchMode and the common non-interactive
//! options, `ssh -O check` before each spawn, two SFTP channels (browsing and
//! transfer), stderr drained into a bounded ring buffer for error
//! classification, remote tool probes (tar, rsync), the single
//! remote-command quoting helper, the two-stage sudo start, exponential
//! backoff reconnect, and `ssh -O exit` for masters this process created.
