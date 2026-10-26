#!/bin/bash
# Container entrypoint: creates users, seeds home trees, writes sshd_config, runs sshd.
set -euo pipefail

PASSWORD='scpto-test-pw'
USERS=(plain sudopw sudonopw sudotty limited noisy)
# CJK file name with spaces, as UTF-8 bytes so this source stays ASCII:
# U+7A7A U+767D, space, U+6A94 U+6848, space, U+6E2C U+8A66, ".txt".
CJK_NAME=$'\xe7\xa9\xba\xe7\x99\xbd \xe6\xaa\x94\xe6\xa1\x88 \xe6\xb8\xac\xe8\xa9\xa6.txt'
# 1x1 transparent PNG
PNG_B64='iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg=='

for u in "${USERS[@]}"; do
    id "$u" >/dev/null 2>&1 || adduser -D -s /bin/bash "$u"
    # sshd rejects locked accounts; password auth stays disabled in sshd_config.
    echo "$u:$PASSWORD" | chpasswd
    home=/home/$u
    install -d -m 0700 -o "$u" -g "$u" "$home/.ssh"
    install -m 0600 -o "$u" -g "$u" /keys/id_ed25519.pub "$home/.ssh/authorized_keys"

    mkdir -p "$home/seed/nested/deeper" "$home/seed/linkdir-target"
    echo "hello scpto" > "$home/seed/hello.txt"
    echo "nested file" > "$home/seed/nested/deeper/inner.txt"
    echo "in dir" > "$home/seed/linkdir-target/file.txt"
    printf '%s' "$PNG_B64" | base64 -d > "$home/seed/pixel.png"
    echo "space and cjk" > "$home/seed/$CJK_NAME"
    ln -sfn hello.txt "$home/seed/link-to-file"
    ln -sfn linkdir-target "$home/seed/link-to-dir"
    chown -R "$u:$u" "$home/seed"
done

# Shell startup output corrupts the sftp protocol for this user. Bash does not read
# ~/.bashrc for non-interactive `bash -c` by default, so BASH_ENV forces it.
echo 'BASH_ENV=/home/noisy/.bashrc' > /home/noisy/.ssh/environment
chown noisy:noisy /home/noisy/.ssh/environment
for f in .bashrc .bash_profile; do
    echo 'echo "NOISY-STARTUP-OUTPUT"' > "/home/noisy/$f"
    chown noisy:noisy "/home/noisy/$f"
done

install -d -m 0755 -o root -g root /srv/rootonly

ssh-keygen -A
cat > /etc/ssh/sshd_config <<CONF
Port 22
HostKey /etc/ssh/ssh_host_ed25519_key
HostKey /etc/ssh/ssh_host_rsa_key
AllowUsers ${USERS[*]}
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin no
PermitUserEnvironment yes
Subsystem sftp ${SFTP_SERVER}
CONF

mkdir -p /run/sshd
# Modern sshd penalizes the unauthenticated connections used by ssh-keyscan.
# Inspect supported settings so OpenSSH 7.9 never sees an unknown directive.
sshd_settings=$(/usr/sbin/sshd -T)
if [[ "$sshd_settings" == *$'\npersourcepenalties '* ]]; then
    echo 'PerSourcePenalties no' >> /etc/ssh/sshd_config
fi
exec /usr/sbin/sshd -D -e
