# Integration-test sshd fixtures

Docker-based OpenSSH servers used by the scpto integration tests. Requires Docker.

    tests/it/fixtures/up.sh     # build, (re)start, write target/it/*
    tests/it/fixtures/down.sh   # stop and remove the containers

`up.sh` is idempotent: it rebuilds images, recreates the containers, checks that `$SFTP_SERVER` is an executable file in each container, regenerates `known_hosts` and `ssh_config`, and keeps an existing key.

If image pulls hang (for example the Docker Desktop credential helper waiting on the keychain), run with an empty Docker config: `DOCKER_CONFIG="$(mktemp -d)" tests/it/fixtures/up.sh`.

## Images

| Image  | Container        | Base          | OpenSSH | Host port       | sftp-server path            |
|--------|------------------|---------------|---------|-----------------|-----------------------------|
| modern | scpto-it-modern  | alpine:3.22   | newest  | 127.0.0.1:22221 | `/usr/lib/ssh/sftp-server`  |
| legacy | scpto-it-legacy  | alpine:3.9    | 7.9p1 (no `limits@openssh.com`) | 127.0.0.1:22222 | `/usr/lib/ssh/sftp-server` |

Both contain openssh-server, openssh-sftp-server, rsync, tar, sudo and bash. sshd uses the default `MaxSessions` and `Subsystem sftp /usr/lib/ssh/sftp-server`.

## Users

All users authenticate with the single ed25519 key `target/it/id_ed25519` (no passphrase). Password authentication is disabled in sshd; the system password of every user is `scpto-test-pw`, which is also the sudo password.

| User     | sudo                                                        |
|----------|-------------------------------------------------------------|
| plain    | none                                                        |
| sudopw   | ALL, password required                                      |
| sudonopw | ALL, `NOPASSWD`                                             |
| sudotty  | ALL, password required, `Defaults:sudotty requiretty`       |
| limited  | password required, only for `/bin/true`                     |
| noisy    | none; `~/.bashrc` and `~/.bash_profile` print `NOISY-STARTUP-OUTPUT` to stdout, which corrupts the sftp protocol (`~/.ssh/environment` sets `BASH_ENV` because bash skips `.bashrc` for `bash -c`) |

Each home contains `seed/` with: `hello.txt`, `pixel.png`, `nested/deeper/inner.txt`, `link-to-file` (symlink to a file), `link-to-dir` (symlink to a directory), and a CJK file name with spaces (U+7A7A U+767D, space, U+6A94 U+6848, space, U+6E2C U+8A66, `.txt`; written as UTF-8 byte escapes in `entrypoint.sh`). `/srv/rootonly` is owned by root with mode 755, for sudo write tests.

## Generated files (in `target/it/`)

- `id_ed25519`, `id_ed25519.pub`: the test key.
- `known_hosts`: host keys collected with `ssh-keyscan`.
- `ssh_config`: aliases `it-<image>-<user>` for every image and user, plus `it-modern-sideeffects` (user `plain`) which sets `RemoteCommand`, `RequestTTY yes`, a `LocalForward` to 127.0.0.1:22999 and `ExitOnForwardFailure yes`, to prove scpto neutralizes user-config side effects. Plain `ssh host true` against that alias fails with "Cannot execute command-line and remote command" by design.

Example: `ssh -F target/it/ssh_config it-legacy-sudopw`
