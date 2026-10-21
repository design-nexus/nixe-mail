#!/usr/bin/env bash
# Run nixe-mail-core's network tests against a throwaway Dovecot (IMAP, TLS) and
# an SMTP sink inside the tests. Everything lives in a scratch directory and runs
# as this user; nothing touches real mail or system services.
set -euo pipefail
cd "$(dirname "$0")/.."
command -v dovecot >/dev/null || { echo "Install dovecot to run these tests." >&2; exit 1; }
cargo test --locked -p nixe-mail-core --test imap --no-run

# Short path: Dovecot's sockets must fit in a Unix socket name.
dir=$(mktemp -d /tmp/nm.XXXXXX)
stop() {
  [[ -f $dir/run/master.pid ]] && kill "$(cat "$dir/run/master.pid")" 2>/dev/null || true
  sleep 0.5
  rm -rf "$dir"
}
trap stop EXIT
mkdir -p "$dir/run" "$dir/state" "$dir/home"

openssl req -x509 -newkey rsa:2048 -nodes -keyout "$dir/ca.key" -out "$dir/ca.pem" -days 2 \
  -subj "/CN=Nixe Mail Test CA" -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=keyCertSign" 2>/dev/null
openssl req -newkey rsa:2048 -nodes -keyout "$dir/key.pem" -out "$dir/req.csr" -subj "/CN=127.0.0.1" 2>/dev/null
printf 'subjectAltName=IP:127.0.0.1,DNS:localhost\nextendedKeyUsage=serverAuth\n' >"$dir/ext.cnf"
openssl x509 -req -in "$dir/req.csr" -CA "$dir/ca.pem" -CAkey "$dir/ca.key" -CAcreateserial \
  -out "$dir/cert.pem" -days 2 -extfile "$dir/ext.cnf" 2>/dev/null

free_port() { python3 -I -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'; }
imap=$(free_port)
imaps=$(free_port)

cat >"$dir/dovecot.conf" <<CONF
dovecot_config_version = 2.4.0
dovecot_storage_version = 2.4.0
base_dir = $dir/run
state_dir = $dir/state
log_path = $dir/dovecot.log
protocols = imap
listen = 127.0.0.1
default_internal_user = $(id -un)
default_internal_group = $(id -gn)
default_login_user = $(id -un)
ssl = required
ssl_server_cert_file = $dir/cert.pem
ssl_server_key_file = $dir/key.pem
auth_mechanisms = plain login
passdb static {
  password = secret
}
userdb static {
  fields {
    uid = $(id -u)
    gid = $(id -g)
    home = $dir/home/%{user}
  }
}
mail_driver = maildir
mail_path = ~/Maildir
service imap-login {
  chroot =
  inet_listener imap {
    port = $imap
  }
  inet_listener imaps {
    port = $imaps
    ssl = yes
  }
}
namespace inbox {
  inbox = yes
  mailbox Sent {
    special_use = \\Sent
    auto = subscribe
  }
  mailbox Trash {
    special_use = \\Trash
    auto = subscribe
  }
  mailbox Drafts {
    special_use = \\Drafts
    auto = subscribe
  }
  mailbox Archive {
    special_use = \\Archive
    auto = subscribe
  }
  mailbox Junk {
    special_use = \\Junk
    auto = subscribe
  }
}
CONF

start="dovecot -c $dir/dovecot.conf"
$start
for _ in $(seq 50); do [[ -S $dir/run/login/imap ]] || [[ -f $dir/run/master.pid ]] && break; sleep 0.1; done
sleep 0.5

export NIXE_MAIL_TEST_DIR=$dir NIXE_MAIL_TEST_IMAP=$imap NIXE_MAIL_TEST_IMAPS=$imaps
export NIXE_MAIL_EXTRA_CA=$dir/ca.pem NIXE_MAIL_TEST_CERT=$dir/cert.pem NIXE_MAIL_TEST_KEY=$dir/key.pem
export NIXE_MAIL_TEST_START="$start" NIXE_MAIL_TEST_STOP="kill \$(cat $dir/run/master.pid)"
status=0
cargo test --locked -p nixe-mail-core --test imap -- --ignored --test-threads=1 "$@" || status=$?
[[ $status == 0 ]] || tail -20 "$dir/dovecot.log" >&2
exit "$status"
