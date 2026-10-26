#!/usr/bin/env bash
# Builds and (re)starts the sshd test containers and writes target/it/{id_ed25519,known_hosts,ssh_config}.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../../.." && pwd)"
out="$root/target/it"
users=(plain sudopw sudonopw sudotty limited noisy)
images=(modern legacy)
port_of() { case "$1" in modern) echo 22221 ;; legacy) echo 22222 ;; esac; }

mkdir -p "$out"
[ -f "$out/id_ed25519" ] || ssh-keygen -q -t ed25519 -N '' -C scpto-it -f "$out/id_ed25519"
chmod 0600 "$out/id_ed25519"

: > "$out/known_hosts"
for img in "${images[@]}"; do
    name="scpto-it-$img"
    port="$(port_of "$img")"
    docker build -q -f "$here/Dockerfile.$img" -t "$name" "$here" >/dev/null
    docker rm -f "$name" >/dev/null 2>&1 || true
    docker run -d --name "$name" -p "127.0.0.1:$port:22" -v "$out:/keys:ro" "$name" >/dev/null
    # shellcheck disable=SC2016 # $SFTP_SERVER expands inside the container.
    docker exec "$name" sh -c 'test -f "$SFTP_SERVER" && test -x "$SFTP_SERVER"' \
        || { echo "$name: \$SFTP_SERVER is not an executable file" >&2; exit 1; }

    for _ in $(seq 1 60); do
        if ssh-keyscan -T 2 -p "$port" 127.0.0.1 2>/dev/null | grep -q ssh-ed25519; then break; fi
        sleep 1
    done
    ssh-keyscan -T 5 -p "$port" 127.0.0.1 2>/dev/null >> "$out/known_hosts"
    grep -q "^\[127.0.0.1\]:$port " "$out/known_hosts" || { echo "sshd on port $port did not come up" >&2; exit 1; }
done

{
    for img in "${images[@]}"; do
        for u in "${users[@]}"; do
            cat <<CONF
Host it-$img-$u
    HostName 127.0.0.1
    Port $(port_of "$img")
    User $u
    IdentityFile $out/id_ed25519
    IdentitiesOnly yes
    UserKnownHostsFile $out/known_hosts
    StrictHostKeyChecking yes

CONF
        done
    done
    cat <<CONF
Host it-modern-sideeffects
    HostName 127.0.0.1
    Port $(port_of modern)
    User plain
    IdentityFile $out/id_ed25519
    IdentitiesOnly yes
    UserKnownHostsFile $out/known_hosts
    StrictHostKeyChecking yes
    RemoteCommand echo side-effect
    RequestTTY yes
    LocalForward 127.0.0.1:22999 127.0.0.1:22
    ExitOnForwardFailure yes
CONF
} > "$out/ssh_config"

echo "ready: ssh -F $out/ssh_config it-modern-plain"
