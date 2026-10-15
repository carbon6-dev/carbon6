#!/usr/bin/env bash
# Tries the .deb packages in a fresh Ubuntu container with systemd as PID 1:
# installs them, checks the head serves, enrolls a worker (c6w@<user>) and the
# memex, checks c6 (the CLI) sees the worker, upgrades over them, then removes them.
#
#   smoke-test.sh <image with systemd (the deb-host stage)> <dir with the .debs> <arch>
set -euo pipefail

image=$1 debs=$2 arch=$3
docker=${DOCKER:-docker}
name=c6-deb-test-$$
trap '$docker rm -f "$name" >/dev/null 2>&1 || true' EXIT

run() { $docker exec "$name" bash -c "$1"; }
step() { printf '\n\033[1m== %s\033[0m\n' "$1"; }
fail() { echo "FAILED: $1" >&2; run 'journalctl --no-pager -n 50 -u c6h -u "c6w@*" -u c6m' >&2 || true; exit 1; }
# Waits up to 90s (emulated architectures are slow) for a command in the container to succeed.
until_ok() {
    for _ in $(seq 90); do run "$1" >/dev/null 2>&1 && return 0; sleep 1; done
    fail "$2"
}

step "Booting $image"
$docker run -d --name "$name" --platform "linux/$arch" --privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
    --tmpfs /run --tmpfs /run/lock "$image" >/dev/null
until_ok 'systemctl is-system-running | grep -qE "running|degraded"' "systemd did not start"
# Not /tmp: newer systemd mounts a tmpfs there, which docker cp cannot see.
run 'mkdir /opt/debs'
for deb in "$debs"/*_"$arch".deb; do $docker cp "$deb" "$name:/opt/debs/"; done

step "Installing $(cd "$debs" && echo *_"$arch".deb)"
run 'apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends /opt/debs/*.deb'

step "Head"
run 'systemctl is-enabled c6h && systemctl is-active c6h' || fail "c6h is not enabled and running"
until_ok 'curl -fsS -o /dev/null http://127.0.0.1:7700/' "the head's UI does not answer on :7700"
run 'stat -c "%U %a %n" /var/lib/c6h /var/lib/c6h/c6h.db'
run 'sudo -u c6h c6h --config /etc/c6h/c6h.toml login-link' | grep -q '^http' || fail "login-link"

token() { run "sudo -u c6h c6h --config /etc/c6h/c6h.toml enroll-token --name $1" | awk '{print $3}'; }

step "Worker as user alice (c6w@alice)"
run 'useradd -m -s /bin/bash alice'
run "sudo -iu alice c6w enroll $(token alice)"
run 'systemctl enable --now c6w@alice'
until_ok 'journalctl -u c6w@alice | grep -q "attached to head"' "c6w@alice did not attach to the head"
run 'systemctl is-active c6w@alice && ps -o user=,args= -C c6w'

step "Memex"
run 'systemctl start c6m; systemctl show -p ConditionResult c6m' | grep -q 'ConditionResult=no' || fail "c6m should be skipped until enrolled"
run "sudo -u c6m c6m --state-dir /var/lib/c6m enroll $(token memex)"
run 'systemctl enable --now c6m'
until_ok 'journalctl -u c6m | grep -q "attached to head"' "c6m did not attach to the head"

step "CLI"
api_token=$(run 'sudo -u c6h c6h --config /etc/c6h/c6h.toml api-token smoke-test')
run "sudo -iu alice c6 login http://127.0.0.1:7700 --token $api_token"
until_ok 'sudo -iu alice c6 workers | grep -qE "online +alice"' "c6 workers does not list alice online"
run 'sudo -iu alice c6 workers'

step "Clean stop (SIGINT) and restart"
run 'systemctl restart c6h c6w@alice c6m'
until_ok 'journalctl -u c6w@alice | grep -c "attached to head" | grep -qv "^1$"' "c6w@alice did not reattach after a restart"
run '! journalctl -u c6h -u c6w@alice -u c6m | grep -E "SIGKILL|Killing process|timed out"' || fail "a service had to be killed"

step "Upgrade over the installed packages"
run 'dpkg -i /opt/debs/*.deb >/dev/null'
run 'for u in c6h c6w@alice c6m; do systemctl is-active -q $u || exit 1; done' || fail "services are not running after the upgrade"
run 'grep -q "^data_dir = \"/var/lib/c6h\"" /etc/c6h/c6h.toml'

step "Remove"
run 'DEBIAN_FRONTEND=noninteractive apt-get remove -y -qq c6h c6w c6m c6'
run '! systemctl is-active -q c6h c6w@alice c6m' || fail "services still running after removal"
run 'test -f /var/lib/c6h/c6h.db' || fail "removal deleted the head's database"

printf '\n\033[1;32mPASS\033[0m: c6h, c6w, c6m and c6 install, run under systemd, upgrade and remove cleanly\n'
