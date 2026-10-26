//! SFTP `Fs` implementation on `openssh-sftp-client`, over an `Sftp` built
//! from the stdin/stdout of an `ssh -s <host> sftp` child (or a sudo
//! sftp-server child). Maps protocol and background-task failures to
//! `FsError`, with a lost connection reported as `FsError::Disconnected`.
