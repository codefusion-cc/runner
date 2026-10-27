#!/usr/bin/env bash
# Sets up a fresh Linux host (Ubuntu 24.04 or Debian 12, x86_64 or arm64, with KVM) as a Runner of the console,
# unattended, as root: the packages, the Runner's user, user namespaces, Rust, the Runner built from its own checkout,
# the newest image of the host's architecture, the configuration, the enrolment with a connect code, and the systemd
# service. The host takes its GitHub tokens from the console: no App key is ever on it. A cloud VM's startup script runs
# it from a clone of this repository:
#
#   scripts/host-setup.sh --machine <name> --console <url> --code-file <file>
#
# --code-file  the console's one-time connect code, alone in a file only root reads; deleted once the host enrolled
# --ref        the commit the Runner is built at (default: the commit of the clone this script is in)
# --image      the image release to install (default: the newest of the host's architecture)
# --home       the Runner user's home, where images and job disks go (default: /home/codefusion-runner)
# --repo       where the Runner's checkout is cloned from (default: GitHub's); its origin is GitHub's either way
# --github-api GitHub's API the Runner talks to, for a test against the fakes (`cargo xtask fakes serve`)
# --check      does every step but the enrolment and the service, and needs no code
#
# Each step is skipped when it is done already. A host that enrolled is set up: run again (a reboot's startup script),
# it only makes sure the service runs. It says each step on standard output; a failure ends it with one line on
# standard error, `host-setup: <step>: <why>`, and a status other than 0.
set -euo pipefail

USER_NAME=codefusion-runner
GITHUB_REPO=https://github.com/codefusion-cc/runner
RELEASES_API=https://api.github.com/repos/codefusion-cc/runner/releases
PACKAGES=(passt git curl jq build-essential pkg-config libssl-dev ca-certificates)

machine='' console='' code_file='' ref='' image='' home=/home/$USER_NAME repo=$GITHUB_REPO github_api='' check=0
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

step=arguments
fail() {
  echo "host-setup: $step: $*" >&2
  exit 1
}
trap 'fail "the command on line $LINENO failed"' ERR
say() { echo "host-setup: $step: $*"; }

while (($#)); do
  case $1 in
    --machine) machine=${2-} ;;
    --console) console=${2-} ;;
    --code-file) code_file=${2-} ;;
    --ref) ref=${2-} ;;
    --image) image=${2-} ;;
    --home) home=${2-} ;;
    --repo) repo=${2-} ;;
    --github-api) github_api=${2-} ;;
    --check) check=1; shift; continue ;;
    *) fail "unknown argument $1" ;;
  esac
  (($# >= 2)) || fail "$1 needs a value"
  shift 2
done
[[ $machine =~ ^[a-z0-9]+(-[a-z0-9]+)*$ && ${#machine} -le 24 ]] || fail "--machine: lowercase letters, digits and dashes, at most 24"
[[ $console =~ ^https?://[A-Za-z0-9.:-]+(/[A-Za-z0-9._/-]*)?$ ]] || fail "--console: an http(s) address"
[[ -z $github_api || $github_api =~ ^https?://[A-Za-z0-9.:-]+/?$ ]] || fail "--github-api: an http(s) address"
[[ $home == /* && $home != *[[:space:]\"\\\$\`\']* ]] || fail "--home: an absolute path without spaces or quotes"
[[ $repo != *[[:space:]\"\\\$\`\']* ]] || fail "--repo: an address or path without spaces or quotes"
if ((!check)); then
  [[ -n $code_file ]] || fail "--code-file is needed (or --check)"
fi
((EUID == 0)) || fail "run it as root"
if [[ -z $ref ]]; then
  ref=$(git -C "$here" rev-parse HEAD 2>/dev/null) || fail "--ref: this script is not in a clone; name the commit"
fi
[[ $ref =~ ^[0-9a-f]{7,40}$ ]] || fail "--ref: a commit, 7 to 40 lowercase hex characters"
case $(uname -m) in
  x86_64) image_prefix=amd64-image- ;;
  aarch64) image_prefix=image- ;;
  *) fail "this host is $(uname -m); the Runner runs on x86_64 and arm64" ;;
esac

state=$home/.local/state/$USER_NAME
config_dir=$home/.config/$USER_NAME
config=$config_dir/config.json
checkout=$home/runner
program=$checkout/target/release/$USER_NAME

# A command as the Runner's user, in its home, with its own systemd reachable and its Rust on the path.
as_user() {
  local uid
  uid=$(id -u "$USER_NAME")
  runuser -u "$USER_NAME" -- env -i HOME="$home" USER="$USER_NAME" LOGNAME="$USER_NAME" \
    PATH="$home/.cargo/bin:/usr/local/bin:/usr/bin:/bin" XDG_RUNTIME_DIR="/run/user/$uid" \
    bash -euo pipefail -c "cd \"\$HOME\"; $1"
}

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

# The Runner updates itself and its image from the console once it is enrolled, so a run again leaves all that be,
# and a code given again is left unused.
if ((!check)) && [[ -e $state/console-token ]]; then
  step=enrolled
  say "$machine is enrolled already"
  start_service
  step=finished
  say "$machine runs the Runner"
  exit 0
fi

step=packages
missing=()
for package in "${PACKAGES[@]}"; do
  dpkg-query -W -f='${Status}' "$package" 2>/dev/null | grep -q 'install ok installed' || missing+=("$package")
done
if ((${#missing[@]})); then
  say "installing ${missing[*]}"
  apt_opts=(-o Acquire::Retries=5 -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30 -o DPkg::Lock::Timeout=300)
  for attempt in 1 2 3; do
    if DEBIAN_FRONTEND=noninteractive apt-get "${apt_opts[@]}" update -q &&
      DEBIAN_FRONTEND=noninteractive apt-get "${apt_opts[@]}" install -y -q --no-install-recommends "${missing[@]}"; then
      break
    fi
    ((attempt < 3)) || fail "apt-get failed three times"
    sleep $((attempt * 20))
  done
fi
[[ -e /dev/kvm ]] || fail "no /dev/kvm: on a cloud VM, turn nested virtualization on"

step=user
if ! id -u "$USER_NAME" >/dev/null 2>&1; then
  say "making $USER_NAME with its home at $home"
  mkdir -p "$(dirname "$home")"
  useradd --create-home --home-dir "$home" --shell /bin/bash "$USER_NAME"
fi
id -nG "$USER_NAME" | tr ' ' '\n' | grep -qx kvm || usermod -aG kvm "$USER_NAME"
[[ -e /var/lib/systemd/linger/$USER_NAME ]] || loginctl enable-linger "$USER_NAME"
# Lingering starts the user's systemd; the steps below talk to it.
bus=/run/user/$(id -u "$USER_NAME")/bus
for _ in $(seq 1 30); do
  [[ -S $bus ]] && break
  sleep 1
done
[[ -S $bus ]] || fail "the user's systemd did not start"

step=namespaces
# Ubuntu 24.04 keeps unprivileged user namespaces from programs without an AppArmor profile; the vm-host needs them.
if [[ -e /proc/sys/kernel/apparmor_restrict_unprivileged_userns ]]; then
  echo kernel.apparmor_restrict_unprivileged_userns=0 >/etc/sysctl.d/60-codefusion-runner.conf
  sysctl -q -p /etc/sysctl.d/60-codefusion-runner.conf
fi

step=rust
if [[ ! -x $home/.cargo/bin/rustup ]]; then
  say "installing rustup"
  as_user 'curl --proto =https --tlsv1.2 -fsSL --retry 5 https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain none --no-modify-path'
fi

step=checkout
if [[ ! -d $checkout/.git ]]; then
  say "cloning $repo"
  as_user "git clone -q '$repo' runner"
fi
# The checkout is the Runner's build folder: at a commit, never on a branch, and an update fetches GitHub's main.
if [[ $(as_user 'git -C runner rev-parse HEAD') != "$ref"* ]]; then
  say "checking out $ref"
  as_user "cd runner && (git cat-file -e '$ref^{commit}' 2>/dev/null || git fetch -q origin '$ref') && git checkout -q --detach '$ref'"
fi
as_user "git -C runner remote set-url origin '$GITHUB_REPO'"

step=build
built=$(as_user "[ -x '$program' ] && '$program' version || true")
if [[ $built != "$USER_NAME "*"+${ref:0:12}" ]]; then
  say "building the Runner at $ref (some minutes)"
  as_user 'cd runner && cargo xtask build'
fi

step=image
if [[ -z $image ]]; then
  # The arm64 releases are image-…, the x86_64 ones amd64-image-… of the same build.
  image=$(curl -fsSL --retry 5 "$RELEASES_API?per_page=50" |
    jq -r --arg prefix "$image_prefix" \
      '[.[] | select(.draft | not) | .tag_name | select(startswith($prefix))][0] // empty')
  [[ -n $image ]] || fail "no ${image_prefix}… release at $RELEASES_API"
fi
[[ $image =~ ^[A-Za-z0-9.-]+$ ]] || fail "--image: not a release name"
if [[ ! -d $state/images/$image ]]; then
  say "installing image $image"
  files=$(curl -fsSL --retry 5 "$RELEASES_API/tags/$image" | jq -r '.assets[].name' | tr '\n' ' ')
  [[ $files =~ ^([A-Za-z0-9._-]+\ )+$ ]] || fail "release $image has no files, or one whose name is not plain"
  as_user "rm -rf 'downloads/$image' && mkdir -p 'downloads/$image' && cd 'downloads/$image' &&
    for f in $files; do curl -fsSL --retry 5 -o \"\$f\" '$GITHUB_REPO/releases/download/$image/'\"\$f\"; done &&
    '$program' image install \"\$PWD\" --version '$image' --state-dir '$state' && cd && rm -rf 'downloads/$image'"
fi

step=config
# No app_dir: the host takes its GitHub tokens from the console it enrols with.
as_user "mkdir -p '$config_dir' && umask 022 && cat > '$config.new' <<EOF
{
  \"machine\": \"$machine\",
  \"state_dir\": \"$state\",
  \"build_folder\": \"$checkout\",
  \"image\": \"$image\",
  \"console\": \"$console\",${github_api:+
  \"github_api\": \"$github_api\",}
  \"use\": \"dedicated\"
}
EOF
chmod go-w '$config.new' && mv '$config.new' '$config'"

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

step=enroll
[[ -f $code_file ]] || fail "--code-file: $code_file is not there"
say "enrolling $machine at $console"
# The code goes to enroll on standard input, never on a command line.
as_user "'$program' enroll --config '$config'" <"$code_file"
rm -f "$code_file"

step=check
as_user "'$program' check --config '$config'"

start_service
step=finished
say "$machine runs the Runner at $ref with image $image"
