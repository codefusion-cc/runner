#!/usr/bin/env bash
# Sets up a Linux host (Ubuntu 24.04 or Debian 12, x86_64 or arm64, with KVM, or without it in container mode) as a Runner of the console, unattended,
# as root, in two steps that hosts kept as code run apart, or both at once as a cloud VM's startup script does:
#
#   scripts/host-setup.sh install --machine <name> --console <url> [options]   everything but the enrolment
#   scripts/host-setup.sh enroll --code-file <file> [--home <dir>]            the enrolment, then the service
#   scripts/host-setup.sh --machine <name> --console <url> --code-file <file> [options]   both
#
# `install` needs no code and may run any number of times: its last line is `host-setup: changed` or
# `host-setup: unchanged`, and with --dry-run it says each change it would make (`would …`, a diff of the
# configuration) and makes none. It puts in place the packages, the Runner's user, user namespaces, the Runner, the
# newest image of the host's architecture and the configuration. The host takes its GitHub tokens from the console it
# enrols with: no App key is ever on it.
#
# The Runner comes from one of:
# --binary <file> --sha256 <hex>     a build of it, checked against its SHA-256
# --version <v> --sha256 <hex>       the release `runner-<v>` of this repository for the host's architecture
#                                    (codefusion-runner-linux-amd64 or -arm64), checked against its SHA-256
# --ref <commit>                     built from its own checkout at that commit, with Rust (the default: the commit of
#                                    the clone this script is in; needed when the script comes on standard input:
#                                    curl -fsSL <raw URL at that commit> | bash -s -- --ref <commit> …)
# A build from a checkout updates itself from the console once the host is enrolled, so `install` leaves it be then.
# A host given a --binary or --version gets that one: on an enrolled host `install` holds new jobs back, waits for the
# running ones (at most an hour) and installs it; when they outlast the wait it exits 75, the hold lifted, and the
# Runner of before goes on.
#
# --class      shared (the default) or trusted: a host that runs only main's deploy, release and signing jobs of
#              codefusion-cc/codefusion, by the Runner's default rules (crates/trusted), one job at a time
# --sandbox    vm (the default): each job in a machine of its own, which needs KVM; or container: each job in a
#              container of its own in the Runner user's rootless Podman, for a host without virtualization. Such a host
#              takes only `cf-container` jobs of private repositories, gives no job Docker and is never trusted. Its image
#              is a container-image release, which `image install --engine` puts in Podman.
# --settings   a JSON object of more keys for the configuration (`ci_memory_gb`, `ci_cpus`, `vcpus`, `labels` …), never
#              one this script writes; a key a run before set and this one leaves out is taken out again
# --code-file  the console's one-time connect code, alone in a file only root reads; deleted once the host enrolled
# --image      the image release to install (default: the newest of the host's architecture)
# --home       the Runner user's home, where images and job disks go (default: /home/codefusion-runner)
# --repo       where the Runner's checkout is cloned from (default: GitHub's); its origin is GitHub's either way
# --github-api GitHub's API the Runner talks to, for a test against the fakes (`cargo xtask fakes serve`)
# --check      both steps' form only: does every step but the enrolment and the service, and needs no code
#
# GITHUB_TOKEN, when set, goes with its reads of GitHub's API (the image's release), whose rate limit it waits out.
#
# Each step is skipped when it is done already. Run again on an enrolled host with neither --binary nor --version (a
# reboot's startup script), both steps only make sure the service runs. It says each step on standard output; a
# failure ends it with one line on standard error, `host-setup: <step>: <why>`, and a status other than 0.
# Errors end it from inside functions too (-E). A command substitution that may fail on purpose, and says so with
# `|| …`, starts with `trap - ERR`, which keeps the trap's line out of what it says.
set -Eeuo pipefail

USER_NAME=codefusion-runner
GITHUB_REPO=https://github.com/codefusion-cc/runner
RELEASES_API=https://api.github.com/repos/codefusion-cc/runner/releases
# A build needs the compiler and OpenSSL's headers; the Runner itself, passt (its machines' network) and curl's libssl.
PACKAGES=(passt curl jq ca-certificates)
# Container mode's engine: Podman, rootless, with the subordinate ids and the user's D-Bus it runs on.
CONTAINER_PACKAGES=(podman uidmap slirp4netns dbus-user-session)
# The kernel modules of the firewall each container's supervisor loads in its own network namespace, which can load
# none itself; loaded now and named in MODULES_LOAD for every boot.
CONTAINER_MODULES=(nf_tables nft_ct)
MODULES_LOAD=/etc/modules-load.d/codefusion-runner.conf
BUILD_PACKAGES=(git build-essential pkg-config libssl-dev)
# The hold `install` sets while it puts a new Runner in place.
SETUP_HOLD=host-setup
# A wait for running jobs that ran out: the hold is lifted, nothing changed, and a later run tries again.
TIMED_OUT=75
# The keys of the configuration this script writes itself; --settings sets the others.
OWNED_KEYS='["machine", "class", "sandbox", "engine", "state_dir", "build_folder", "image", "console", "github_api", "use"]'
# Keys --settings may not set although this script leaves them be: a trusted host's rules and the Mac app's folder.
REFUSED_SETTINGS='["trusted", "app_dir"]'

mode=both machine='' console='' code_file='' class=shared sandbox=vm ref='' image='' home=/home/$USER_NAME repo=$GITHUB_REPO
github_api='' check=0 dry=0 settings='' engine='' binary='' version='' sha256=''
# The clone this script is in; none when it came on standard input (curl … | bash -s), which then names --ref.
here=''
[[ -f ${BASH_SOURCE[0]:-} ]] && here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

step=arguments
failed=0
fail() {
  echo "host-setup: $step: $*" >&2
  failed=1
  exit 1
}
trap 'fail "the command on line $LINENO failed"' ERR
# A command that fails inside a function (as_user) ends the script without the ERR trap. A hold this run set is lifted
# however it ends: killed outright, the next run lifts it.
holding=0
ended() {
  local status=$?
  ((status == 0 || failed)) || echo "host-setup: $step: a command failed with status $status" >&2
  ((!holding)) || lift_setup_hold || echo "host-setup: $step: the hold $SETUP_HOLD stays; resume it by hand" >&2
}
trap ended EXIT
trap 'exit 143' TERM HUP INT
# What this script writes is no one else's to change, whatever umask it was started with.
umask 022
say() { echo "host-setup: $step: $*"; }

# A change this run makes, or with --dry-run would make: said, and counted for the last line.
changes=0
change() {
  changes=$((changes + 1))
  if ((dry)); then say "would $*"; else say "$*"; fi
}

# A GET of GitHub's API, its body on standard output. It sends GITHUB_TOKEN when that is set (on standard input, never
# on a command line). A 403 or 429 (GitHub's rate limit), a 408, a 5xx or no answer at all is asked again after the
# wait the answer names (Retry-After, else x-ratelimit-reset), else after 2, 4, 8 … s, each wait at most
# GITHUB_WAIT_MAX s, GITHUB_TRIES times in all. Any other status fails at once.
github_get() {
  local url=$1 tries=0 status wait reset headers body
  headers=$(mktemp) body=$(mktemp)
  while :; do
    status=$(
      trap - ERR
      {
        echo 'header = "Accept: application/vnd.github+json"'
        [[ -z ${GITHUB_TOKEN:-} ]] || echo "header = \"Authorization: Bearer $GITHUB_TOKEN\""
      } | curl -sSL --config - -D "$headers" -o "$body" -w '%{http_code}' "$url"
    ) || status=000
    if [[ $status == 2?? ]]; then
      cat "$body"
      rm -f "$headers" "$body"
      return 0
    fi
    if [[ ! $status =~ ^(000|403|408|429|5..)$ ]] || ((++tries >= ${GITHUB_TRIES:-6})); then
      echo "host-setup: $step: GitHub answered $status for $url" >&2
      rm -f "$headers" "$body"
      return 1
    fi
    # The headers of the last answer (-L may have followed a redirect), by name in any case.
    wait=$(tr -d '\r' <"$headers" | awk -F': *' 'tolower($1) == "retry-after" { v = $2 } END { print v }')
    if [[ ! $wait =~ ^[0-9]+$ ]]; then
      reset=$(tr -d '\r' <"$headers" | awk -F': *' 'tolower($1) == "x-ratelimit-reset" { v = $2 } END { print v }')
      if [[ $reset =~ ^[0-9]+$ ]]; then wait=$((reset - $(date +%s))); else wait=$((2 ** tries)); fi
    fi
    ((wait >= 1)) || wait=1
    ((wait <= ${GITHUB_WAIT_MAX:-900})) || wait=${GITHUB_WAIT_MAX:-900}
    echo "host-setup: $step: GitHub answered $status, asking again in ${wait}s" >&2
    sleep "$wait"
  done
}
# The configuration a run writes, on standard output: the one there now ($1, `{}` for none) without the keys the last
# run's settings set ($2) and every key this script owns, then the owned keys this run gives ($3, nulls left out) and
# its settings ($4), sorted on one line. The keys the Runner or the console set themselves stay. Fails for settings that
# are no JSON object, that name a key this script owns, or one of REFUSED_SETTINGS.
merged_config() {
  jq -n -S -c --argjson now "$1" --argjson last "$2" --argjson owned "$3" --argjson settings "$4" \
    --argjson keys "$OWNED_KEYS" --argjson refused "$REFUSED_SETTINGS" '
    def named($list): [$settings | keys[] | select(. as $k | $list | index($k))];
    if ($settings | type) != "object" then error("--settings: no JSON object")
    elif (named($keys) | length) > 0 then error("--settings: \(named($keys) | join(", ")) only this script sets")
    elif (named($refused) | length) > 0 then error("--settings: \(named($refused) | join(", ")) is not set this way")
    else
      ($now | with_entries(select(.key as $k | (($last | has($k)) or ($keys | index($k))) | not)))
      + ($owned | with_entries(select(.value != null)))
      + $settings
    end'
}

# The image an enrolled host's configuration names ($1), on standard output, while it is of the kind its sandbox runs,
# whose names start with $2; nothing for one of the other kind, so a host moved between machines and containers
# installs the newest of its new kind.
kept_image() {
  if [[ -n $1 && $1 == "$2"* ]]; then printf '%s\n' "$1"; fi
}

# Of the modules ($3 on), those the boot list $1 lacks, each as "boot <module>", and those not loaded now (no folder in
# $2, /sys/module), each as "now <module>".
modules_wanted() {
  local list=$1 loaded=$2 module
  shift 2
  for module; do
    grep -qx -- "$module" "$list" 2>/dev/null || printf 'boot %s\n' "$module"
    [[ -d $loaded/$module ]] || printf 'now %s\n' "$module"
  done
}

# Sourced (the tests), it only defines the above.
if (return 0 2>/dev/null); then return 0; fi
case ${1-} in
  install | enroll)
    mode=$1
    shift
    ;;
esac
while (($#)); do
  case $1 in
    --check) check=1; shift; continue ;;
    --dry-run) dry=1; shift; continue ;;
    --machine | --console | --class | --sandbox | --settings | --binary | --version | --sha256 | --ref | --image | --repo | --github-api)
      [[ $mode != enroll ]] || fail "enroll takes $1 from the configuration install wrote"
      ;;
    --code-file) [[ $mode != install ]] || fail "install takes no code; enroll does" ;;
    --home) ;;
    # A stray word may be the code itself, so only an option's name is repeated.
    --*) fail "unknown option $1" ;;
    *) fail "an argument that is no option" ;;
  esac
  (($# >= 2)) || fail "$1 needs a value"
  case $1 in
    --machine) machine=$2 ;;
    --console) console=$2 ;;
    --code-file) code_file=$2 ;;
    --class) class=$2 ;;
    --sandbox) sandbox=$2 ;;
    --settings) settings=$2 ;;
    --binary) binary=$2 ;;
    --version) version=$2 ;;
    --sha256) sha256=$2 ;;
    --ref) ref=$2 ;;
    --image) image=$2 ;;
    --home) home=$2 ;;
    --repo) repo=$2 ;;
    --github-api) github_api=$2 ;;
  esac
  shift 2
done
((!check)) || [[ $mode == both ]] || fail "--check: only where both steps run; install changes nothing with --dry-run"
((!dry)) || [[ $mode == install ]] || fail "--dry-run: install's alone"
[[ $home == /* && $home != *[[:space:]\"\\\$\`\']* ]] || fail "--home: an absolute path without spaces or quotes"
if [[ $mode != enroll ]]; then
  [[ $class == shared || $class == trusted ]] || fail "--class: shared or trusted"
  [[ $sandbox == vm || $sandbox == container ]] || fail "--sandbox: vm or container"
  [[ $sandbox == vm || $class == shared ]] || fail "--sandbox: a trusted host runs its jobs in machines"
  [[ $machine =~ ^[a-z0-9]+(-[a-z0-9]+)*$ && ${#machine} -le 24 ]] ||
    fail "--machine: lowercase letters, digits and dashes, at most 24"
  # As the Runner has it: https, or http only to this host.
  [[ $console =~ ^(https://[A-Za-z0-9.:-]+|http://(127\.0\.0\.1|localhost|\[::1\])(:[0-9]+)?)(/[A-Za-z0-9._/-]*)?$ ]] ||
    fail "--console: an https address, or http to this host"
  [[ -z $github_api || $github_api =~ ^https?://[A-Za-z0-9.:-]+/?$ ]] || fail "--github-api: an http(s) address"
  [[ $repo != *[[:space:]\"\\\$\`\']* ]] || fail "--repo: an address or path without spaces or quotes"
  [[ -z $settings || -f $settings ]] || fail "--settings: $settings is no file"
  [[ -z $binary || -z $version ]] || fail "--binary or --version, not both"
  if [[ -n $binary$version ]]; then
    [[ -z $ref ]] || fail "--ref builds the Runner; --binary and --version bring one built"
    [[ $sha256 =~ ^[0-9a-f]{64}$ ]] || fail "--sha256: the Runner's SHA-256, 64 lowercase hex characters"
    [[ -z $binary || -f $binary ]] || fail "--binary: $binary is no file"
    [[ -z $version || $version =~ ^[0-9]+\.[0-9]+\.[0-9]+-[0-9a-f]{12}$ ]] ||
      fail "--version: a release's version, such as 0.1.0-446125c0a1b2"
  else
    [[ -z $sha256 ]] || fail "--sha256 goes with --binary or --version"
  fi
fi
if [[ $mode == both ]] && ((!check)); then
  [[ -n $code_file ]] || fail "--code-file is needed (or --check)"
fi
# The code gives GitHub tokens until it is spent: a file of root's alone, never a link, as the minutes of the build
# would leave it open to every user of the host.
if [[ -n $code_file && ( -e $code_file || -L $code_file ) ]]; then
  [[ ! -L $code_file && -f $code_file ]] || fail "--code-file: $code_file is a link or no file"
  [[ $(stat -c %u -- "$code_file") == 0 && $(stat -c %a -- "$code_file") =~ ^[0-7]00$ ]] ||
    fail "--code-file: $code_file must be root's, readable by root alone (chmod 600)"
fi
((EUID == 0)) || fail "run it as root"
case $(uname -m) in
  x86_64) image_prefix=amd64- platform=amd64 ;;
  aarch64) image_prefix='' platform=arm64 ;;
  *) fail "this host is $(uname -m); the Runner runs on x86_64 and arm64" ;;
esac
# A machine's image is image-… (arm64) or amd64-image-…; a container's is container-image-… or amd64-container-image-….
if [[ $sandbox == container ]]; then image_prefix+=container-image-; else image_prefix+=image-; fi

# A command as the Runner's user, in its home, with its own systemd reachable and its Rust on the path.
as_user() {
  local uid
  uid=$(id -u "$USER_NAME")
  runuser -u "$USER_NAME" -- env -i HOME="$home" USER="$USER_NAME" LOGNAME="$USER_NAME" \
    PATH="$home/.cargo/bin:/usr/local/bin:/usr/bin:/bin" XDG_RUNTIME_DIR="/run/user/$uid" \
    bash -euo pipefail -c "cd \"\$HOME\"; $1"
}

# A file in the Runner's user's folders, read as that user: root never follows a link that user put there.
user_file() { as_user "cat '$1'"; }

state=$home/.local/state/$USER_NAME
config_dir=$home/.config/$USER_NAME
config=$config_dir/config.json
checkout=$home/runner
# A build brought along waits here for `codefusion-runner install`, which copies it to where the service runs it.
staged=$home/.local/lib/$USER_NAME/$USER_NAME
installed=$state/bin/$USER_NAME
# What last set the configuration's other keys, to take out the ones a later --settings leaves out.
last_settings=$state/host-setup-settings.json
enrolled=0
[[ -e $state/console-token ]] && enrolled=1

built_here=1
[[ -n $binary$version ]] && built_here=0
# An enrolled host that runs a build brought along keeps it: a run with neither --binary nor --version (new settings,
# a reboot) builds nothing and leaves its program be.
keep_program=0
if [[ $mode == enroll ]]; then
  [[ -f $config ]] || fail "no configuration at $config: install first"
  user_file "$config" | jq -e 'has("build_folder")' >/dev/null 2>&1 || built_here=0
elif ((enrolled && built_here)) && [[ -f $config ]] && ! user_file "$config" | jq -e 'has("build_folder")' >/dev/null 2>&1; then
  [[ -z $ref ]] || fail "--ref: this host runs a build brought along; give it a new one with --binary or --version"
  built_here=0 keep_program=1
fi
if [[ $mode != enroll ]] && ((built_here)) && [[ -z $ref ]]; then
  if [[ -z $here ]] || ! ref=$(
    trap - ERR
    git -C "$here" rev-parse HEAD 2>/dev/null
  ); then
    fail "--ref: this script is not in a clone; name the commit"
  fi
fi
[[ -z $ref || $ref =~ ^[0-9a-f]{7,40}$ ]] || fail "--ref: a commit, 7 to 40 lowercase hex characters"
if ((built_here)); then program=$checkout/target/release/$USER_NAME; else program=$staged; fi

start_service() {
  step=service
  if as_user "systemctl --user is-enabled -q $USER_NAME 2>/dev/null"; then
    as_user "systemctl --user start $USER_NAME"
  else
    as_user "'$program' install --config '$config'"
  fi
  as_user "systemctl --user is-active -q $USER_NAME" ||
    fail "the service is not running: journalctl --user -u $USER_NAME as $USER_NAME"
}

# A file's SHA-256, or nothing for a file that is not there.
sha256_of() {
  [[ -f $1 ]] || return 0
  sha256sum -- "$1" | cut -d' ' -f1
}
# The same of a file in the Runner's user's folders, read as that user.
user_sha256() { as_user "if [ -f '$1' ]; then sha256sum -- '$1' | cut -d' ' -f1; fi"; }

# Lifts the hold this script sets, with the Runner that set it.
lift_setup_hold() {
  as_user "'$installed' resume --config '$config' --hold $SETUP_HOLD" >/dev/null
}

# The jobs ran longer than a run waits for them: the Runner of before goes on, and the run ends with TIMED_OUT.
jobs_outlasted() {
  echo "host-setup: $step: jobs still run; the Runner of before goes on, and a run later tries again" >&2
  failed=1
  exit $TIMED_OUT
}

# The service runs the staged program from now on. `install` is refused while a machine stands, so new jobs are held
# back and the running ones waited for; the hold goes again however this ends. A Runner from before holds writes no
# status and takes no hold: `install` is tried again until it finds no machine standing.
replace_running() {
  step=restart
  local status=0
  if [[ ! -e $state/status.json ]]; then
    local tries=0 said
    until said=$(
      trap - ERR
      as_user "'$staged' install --config '$config'" 2>&1
    ); do
      [[ $said == *"machines stand"* ]] || fail "the new Runner was not installed: $said"
      ((++tries < 120)) || jobs_outlasted
      sleep 30
    done
    return
  fi
  holding=1
  as_user "'$staged' pause --config '$config' --hold $SETUP_HOLD --reason 'a new Runner from host-setup' --wait" ||
    status=$?
  if ((status == 0)); then
    as_user "'$staged' install --config '$config'" || status=$?
  fi
  lift_setup_hold || fail "the hold $SETUP_HOLD could not be lifted; resume it by hand"
  holding=0
  ((status != TIMED_OUT)) || jobs_outlasted
  ((status == 0)) || fail "the new Runner was not installed (status $status)"
}

install_host() {
  # A run killed while it held the host left its hold: the host takes jobs again before anything else.
  if ((enrolled && !dry)) && [[ -e $state/holds/$SETUP_HOLD.json ]]; then
    step=hold
    say "lifting the hold $SETUP_HOLD a run before left"
    lift_setup_hold || fail "the hold $SETUP_HOLD could not be lifted"
  fi
  step=packages
  local wanted=("${PACKAGES[@]}") missing=() package present
  ((!built_here)) || wanted+=("${BUILD_PACKAGES[@]}")
  [[ $sandbox != container ]] || wanted+=("${CONTAINER_PACKAGES[@]}")
  # One question for all: a package dpkg does not know is left out of its answer, and so is missing.
  present=$(
    trap - ERR
    dpkg-query -W -f='${Package} ${Status}\n' "${wanted[@]}" 2>/dev/null
  ) || true
  for package in "${wanted[@]}"; do
    grep -qx "$package install ok installed" <<<"$present" || missing+=("$package")
  done
  if ((${#missing[@]})); then
    change "install ${missing[*]}"
    if ((!dry)); then
      local apt_opts=(-o Acquire::Retries=5 -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30
        -o DPkg::Lock::Timeout=300) attempt
      for attempt in 1 2 3; do
        if DEBIAN_FRONTEND=noninteractive apt-get "${apt_opts[@]}" update -q &&
          DEBIAN_FRONTEND=noninteractive apt-get "${apt_opts[@]}" install -y -q --no-install-recommends "${missing[@]}"; then
          break
        fi
        ((attempt < 3)) || fail "apt-get failed three times"
        sleep $((attempt * 20))
      done
    fi
  fi
  [[ $sandbox == container || -e /dev/kvm ]] ||
    fail "no /dev/kvm: on a cloud VM, turn nested virtualization on, or run its jobs in containers (--sandbox container)"
  if ((dry)) && ! command -v jq >/dev/null; then
    change "set up the rest of the host: the Runner's user, the Runner, its image and its configuration"
    return
  fi

  step=user
  if ! id -u "$USER_NAME" >/dev/null 2>&1; then
    change "make $USER_NAME with its home at $home"
    if ((dry)); then
      change "set up the rest of the host: the Runner, its image and its configuration"
      return
    fi
    mkdir -p "$(dirname "$home")"
    useradd --create-home --home-dir "$home" --shell /bin/bash "$USER_NAME"
  fi
  if [[ $sandbox == vm ]] && ! id -nG "$USER_NAME" | tr ' ' '\n' | grep -qx kvm; then
    change "add $USER_NAME to kvm"
    ((dry)) || usermod -aG kvm "$USER_NAME"
  fi
  if [[ ! -e /var/lib/systemd/linger/$USER_NAME ]]; then
    change "let $USER_NAME's systemd run without a login"
    ((dry)) || loginctl enable-linger "$USER_NAME"
  fi
  if ((!dry)); then
    # Lingering starts the user's systemd; the steps below talk to it.
    local bus
    bus=/run/user/$(id -u "$USER_NAME")/bus
    for _ in $(seq 1 30); do
      [[ -S $bus ]] && break
      sleep 1
    done
    [[ -S $bus ]] || fail "the user's systemd did not start"
  fi

  step=namespaces
  # Ubuntu 24.04 keeps unprivileged user namespaces from programs without an AppArmor profile; the vm-host needs them.
  local sysctl_file=/etc/sysctl.d/60-codefusion-runner.conf line=kernel.apparmor_restrict_unprivileged_userns=0
  if [[ -e /proc/sys/kernel/apparmor_restrict_unprivileged_userns ]] &&
    { [[ $(cat "$sysctl_file" 2>/dev/null || true) != "$line" ]] || [[ $(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns) != 0 ]]; }; then
    change "let unprivileged programs make user namespaces"
    if ((!dry)); then
      echo "$line" >"$sysctl_file"
      sysctl -q -p "$sysctl_file"
    fi
  fi

  [[ $sandbox == vm ]] || place_engine
  if ((built_here)); then
    build_here
  else
    place_binary
  fi
  place_image
  write_config
}

# Container mode's engine: the Runner user's own Podman, whose API socket systemd opens and which starts on the first
# call. Its containers' users map into subordinate ids of that user's, never onto the host's root.
place_engine() {
  step=engine
  engine=/run/user/$(id -u "$USER_NAME" 2>/dev/null || echo 0)/podman/podman.sock
  local file start when module
  while read -r when module; do
    if [[ $when == boot ]]; then
      change "load the kernel module $module at every boot ($MODULES_LOAD)"
      ((dry)) || printf '%s\n' "$module" >>"$MODULES_LOAD"
    else
      change "load the kernel module $module"
      ((dry)) || modprobe "$module"
    fi
  done < <(modules_wanted "$MODULES_LOAD" /sys/module "${CONTAINER_MODULES[@]}")
  for file in /etc/subuid /etc/subgid; do
    if ! grep -q "^$USER_NAME:" "$file" 2>/dev/null; then
      change "give $USER_NAME 65536 subordinate ids in $file"
      if ((!dry)); then
        # After every range given, and never below 100000, where useradd starts.
        start=$(cat /etc/subuid /etc/subgid 2>/dev/null | awk -F: '$3 ~ /^[0-9]+$/ { end = $2 + $3; if (end > top) top = end }
          END { print (top > 100000 ? top : 100000) }')
        usermod "--add-${file#/etc/}s" "$start-$((start + 65535))" "$USER_NAME"
      fi
    fi
  done
  if ! as_user "systemctl --user is-enabled -q podman.socket 2>/dev/null && systemctl --user is-active -q podman.socket"; then
    change "open Podman's socket for $USER_NAME at $engine"
    ((dry)) && return
    # Podman reads the subordinate ids when it first runs; one that ran before them knows none.
    as_user "podman system migrate >/dev/null 2>&1 || true; systemctl --user enable --now podman.socket"
  fi
  ((dry)) && return
  as_user "curl -fsS --max-time 30 --unix-socket '$engine' http://engine/_ping >/dev/null" ||
    fail "Podman does not answer at $engine"
}

# The Runner built from its own checkout at --ref. Once enrolled, the console's updates move that checkout, so a run
# again leaves it be.
build_here() {
  step=build
  if ((enrolled)); then
    say "the Runner updates its own checkout from the console; left as it is"
    return
  fi
  step=rust
  if [[ ! -x $home/.cargo/bin/rustup ]]; then
    change "install rustup"
    ((dry)) ||
      as_user 'curl --proto =https --tlsv1.2 -fsSL --retry 5 https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain none --no-modify-path'
  fi
  step=checkout
  if [[ ! -d $checkout/.git ]]; then
    change "clone $repo"
    if ((dry)); then
      change "build the Runner at $ref"
      return
    fi
    as_user "git clone -q -- '$repo' runner"
  fi
  # The checkout is the Runner's build folder: at a commit, never on a branch, and an update fetches GitHub's main.
  if [[ $(as_user 'git -C runner rev-parse HEAD') != "$ref"* ]]; then
    change "check out $ref"
    ((dry)) ||
      as_user "cd runner && (git cat-file -e '$ref^{commit}' 2>/dev/null || git fetch -q origin '$ref') && git checkout -q --detach '$ref'"
  fi
  ((dry)) || as_user "git -C runner remote set-url origin '$GITHUB_REPO'"
  # The whole commit: `version` names twelve characters of it, which a shorter --ref is not.
  local head built
  head=$(as_user 'git -C runner rev-parse HEAD')
  step=build
  built=$(as_user "[ -x '$program' ] && '$program' version || true")
  if ((dry)) && [[ $head != "$ref"* ]]; then
    change "build the Runner at $ref (some minutes)"
  elif [[ $built != "$USER_NAME "*"+${head:0:12}" ]]; then
    change "build the Runner at $ref (some minutes)"
    ((dry)) || as_user 'cd runner && cargo xtask build'
  fi
}

# The Runner of --binary or --version, staged once its SHA-256 is the one named; on an enrolled host, also installed
# in the service.
place_binary() {
  step=program
  if ((keep_program)); then
    say "the Runner it runs stays; a new one comes with --binary or --version"
    return
  fi
  local named=${version:+release runner-$version}
  named=${named:-$binary}
  if [[ $(user_sha256 "$staged") != "$sha256" ]]; then
    change "place the Runner of $named"
    if ((!dry)); then
      local download
      download=$(mktemp -d)
      if [[ -n $version ]]; then
        local asset=$GITHUB_REPO/releases/download/runner-$version/$USER_NAME-linux-$platform
        curl -fsSL --retry 5 --retry-all-errors -o "$download/$USER_NAME" "$asset" ||
          { rm -rf "$download"; fail "no download from $asset"; }
      else
        cp -- "$binary" "$download/$USER_NAME"
      fi
      local got
      got=$(sha256_of "$download/$USER_NAME")
      if [[ $got != "$sha256" ]]; then
        rm -rf "$download"
        fail "$named has the SHA-256 $got, not $sha256"
      fi
      # Written by the Runner's user into its own folder: root writing there would follow a link that user put in.
      as_user "mkdir -p '$(dirname "$staged")' && cat > '$staged.new' && chmod 755 '$staged.new' && mv -f '$staged.new' '$staged'" \
        <"$download/$USER_NAME"
      rm -rf "$download"
      local said
      said=$(
        trap - ERR
        as_user "'$staged' version"
      ) || fail "$named does not run on this host"
      say "it is $said"
    fi
  fi
  if ((enrolled)) && [[ $(user_sha256 "$installed") != "$sha256" ]]; then
    change "run it in the service, once running jobs end"
    ((dry)) || replace_running
  fi
}

place_image() {
  step=image
  if ((enrolled)); then
    # The Runner keeps its image up to date itself; the configuration keeps the one it named.
    local kept
    kept=$(kept_image "$(user_file "$config" | jq -r '.image // empty')" "$image_prefix")
    if [[ -n $kept ]]; then
      image=$kept
      return
    fi
  fi
  if [[ -z $image ]]; then
    # The arm64 releases are image-…, the x86_64 ones amd64-image-… of the same build.
    image=$(github_get "$RELEASES_API?per_page=50" |
      jq -r --arg prefix "$image_prefix" \
        '[.[] | select(.draft | not) | .tag_name | select(startswith($prefix))][0] // empty')
    [[ -n $image ]] || fail "no ${image_prefix}… release at $RELEASES_API"
  fi
  [[ $image =~ ^[A-Za-z0-9.-]+$ ]] || fail "--image: not a release name"
  local into=''
  if [[ $sandbox == container ]]; then
    into=" --engine '$engine'"
    # Its folder keeps the release's record; the files the jobs start from are in Podman, which may have lost them.
    if [[ -d $state/images/$image ]] && ! as_user "podman image exists 'codefusion-runner-guest:$image'"; then
      change "install image $image again: Podman no longer holds it"
      ((dry)) && return
      as_user "rm -rf '$state/images/$image'"
    fi
  fi
  if [[ ! -d $state/images/$image ]]; then
    change "install image $image"
    ((dry)) && return
    local files
    files=$(github_get "$RELEASES_API/tags/$image" | jq -r '.assets[].name' | tr '\n' ' ')
    [[ $files =~ ^([A-Za-z0-9._-]+\ )+$ ]] || fail "release $image has no files, or one whose name is not plain"
    as_user "rm -rf 'downloads/$image' && mkdir -p 'downloads/$image' && cd 'downloads/$image' &&
      for f in $files; do curl -fsSL --retry 5 -o \"\$f\" '$GITHUB_REPO/releases/download/$image/'\"\$f\"; done &&
      '$program' image install \"\$PWD\" --version '$image' --state-dir '$state'$into && cd && rm -rf 'downloads/$image'"
  fi
}

# The host takes its GitHub tokens from the console it enrols with, which holds the GitHub App's key. A trusted host's
# file names its class and no rules, so the Runner's defaults apply. A running Runner starts again on a file whose
# bytes changed, so the file is written only when what it says changes.
write_config() {
  step=config
  local now='{}' last='{}' given='{}' owned wanted
  [[ ! -f $config ]] || now=$(trap - ERR; user_file "$config" | jq -S -c .) || fail "$config is no JSON"
  [[ ! -f $last_settings ]] || last=$(trap - ERR; user_file "$last_settings" | jq -S -c .) || fail "$last_settings is no JSON"
  [[ -z $settings ]] || given=$(trap - ERR; jq -S -c . "$settings") || fail "--settings: $settings is no JSON"
  owned=$(jq -n -c --arg machine "$machine" --arg class "$class" --arg sandbox "$sandbox" --arg engine "$engine" \
    --arg state "$state" --arg image "$image" \
    --arg console "$console" --arg github_api "$github_api" --arg build "$checkout" --argjson built "$built_here" '{
      machine: $machine,
      class: (if $class == "trusted" then $class else null end),
      sandbox: (if $sandbox == "container" then $sandbox else null end),
      engine: (if $sandbox == "container" then $engine else null end),
      state_dir: $state,
      build_folder: (if $built == 1 then $build else null end),
      image: $image,
      console: $console,
      github_api: (if $github_api == "" then null else $github_api end),
      use: "dedicated"
    }')
  wanted=$(
    trap - ERR
    merged_config "$now" "$last" "$owned" "$given" 2>&1
  ) || fail "${wanted#jq: error (at <unknown>): }"
  # All four are sorted and on one line, so equal ones are equal strings.
  if [[ $now != "$wanted" || $last != "$given" ]]; then
    if [[ $now != "$wanted" ]]; then
      change "write $config:"
      diff -u --label before --label after <(jq . <<<"$now") <(jq . <<<"$wanted") | tail -n +3 || true
    fi
    ((dry)) && return
    local tmp
    tmp=$(mktemp)
    jq . <<<"$wanted" >"$tmp"
    as_user "mkdir -p '$config_dir' && (umask 077 && mkdir -p '$state') && umask 022 && cat > '$config.new' && chmod go-w '$config.new' && mv '$config.new' '$config'" <"$tmp"
    as_user "umask 077 && cat > '$last_settings.new' && mv '$last_settings.new' '$last_settings'" <<<"$given"
    rm -f "$tmp"
  fi
}

enroll_host() {
  if ((enrolled)); then
    step=enrolled
    say "this host is enrolled already"
    [[ -z $code_file ]] || rm -f -- "$code_file"
    start_service
    return
  fi
  step=enroll
  [[ -n $code_file ]] || fail "--code-file is needed"
  [[ -f $code_file ]] || fail "--code-file: $code_file is not there"
  say "enrolling at $(user_file "$config" | jq -r .console)"
  # The code goes to enroll on standard input, never on a command line.
  as_user "'$program' enroll --config '$config'" <"$code_file"
  rm -f -- "$code_file"
  enrolled=1
  step=check
  as_user "'$program' check --config '$config'"
  start_service
}

case $mode in
  install)
    install_host
    step=finished
    if ((changes)); then echo "host-setup: changed"; else echo "host-setup: unchanged"; fi
    ;;
  enroll)
    enroll_host
    step=finished
    say "the host runs the Runner"
    ;;
  both)
    # A reboot's startup script on an enrolled host: the Runner updates itself and its image from the console.
    if ((!check && enrolled)) && ((built_here)); then
      enroll_host
      step=finished
      say "$machine runs the Runner"
      exit 0
    fi
    install_host
    if ((check)); then
      step=check
      # Before the enrolment the check misses the console's token; anything else it misses is a problem.
      said=$(as_user "'$program' check --config '$config'" 2>&1) || true
      echo "$said"
      if grep MISSING <<<"$said" | grep -qv enroll; then
        fail "the check misses more than the enrolment"
      fi
      step=finished
      say "--check: everything but the enrolment and the service is in place"
      exit 0
    fi
    enroll_host
    step=finished
    say "$machine runs the Runner${ref:+ at $ref} with image $image"
    ;;
esac
