#!/bin/bash
# Docker jobs on one Mac: installs an image release with Docker's tools, then names it and the repositories whose
# cf-docker jobs get a daemon in the Runner's configuration file. The running Runner takes the new file up by itself:
# it takes no new job until no machine stands, then starts again on it (README, the configuration). Run as the Mac's
# own user, from another Mac:
#
#   ssh <mac> 'bash -s' -- <image version> < docker-rollout-mac.sh
#
# The Runner must take up a changed file by itself (codefusion-cc/runner#30 and later); the script checks its log for
# that and puts the file back when it does not. It ends once the Runner started again on the new file, or says why
# not: a file the Runner would not start on (the file goes back), or a wait for the machines that ran out.
set -euo pipefail
VERSION=${1:?the image version, image-YYYY.MM.DD-N}
REPOS='["codefusion-cc/bigusson-presta","codefusion-cc/runner-first-job"]'
SERVED="codefusion-cc/bigusson-presta codefusion-cc/runner-first-job"
WAIT_MINUTES=55

PLIST=$HOME/Library/LaunchAgents/cc.codefusion.vm-runner.plist
[ -f "$PLIST" ] || { echo "no Runner installed here ($PLIST)"; exit 1; }
CONFIG=$(/usr/libexec/PlistBuddy -c 'Print :ProgramArguments' "$PLIST" | awk 'found { sub(/^ +/, ""); print; exit } $1 == "--config" { found = 1 }')
[ -f "$CONFIG" ] || { echo "the Runner's config file is not where launchd says: $CONFIG"; exit 1; }
STATE=$(plutil -extract state_dir raw -o - "$CONFIG")
RUNNER=$STATE/bin/codefusion-runner
LOG=$STATE/logs/runner.log
echo "Runner $("$RUNNER" version), config $CONFIG"

# The release, verified by the Runner itself as it installs it.
DOWNLOAD=$(mktemp -d)
trap 'rm -rf "$DOWNLOAD"' EXIT
BASE=https://github.com/codefusion-cc/runner/releases/download/$VERSION
for file in Image image.json initramfs.cpio.gz SHA256SUMS SHA256SUMS.sig; do
  curl -fsSL --retry 3 -o "$DOWNLOAD/$file" "$BASE/$file"
done
for part in $(awk '{ print $2 }' "$DOWNLOAD/SHA256SUMS" | grep '^rootfs\.erofs\.'); do
  curl -fsSL --retry 3 -o "$DOWNLOAD/$part" "$BASE/$part"
done
"$RUNNER" image install "$DOWNLOAD" --version "$VERSION" --state-dir "$STATE"

# The new settings, checked by the Runner before they replace the old ones.
cp -p "$CONFIG" "$CONFIG.before-docker"
NEXT=$CONFIG.next
cp -p "$CONFIG" "$NEXT"
plutil -replace image -string "$VERSION" "$NEXT"
plutil -replace docker_repos -json "$REPOS" "$NEXT"
# The repositories with Docker are served too. An empty or missing `repos` already serves every private repository;
# one named there would narrow it to that one, so only a list that names some gets them added.
listed=$(plutil -extract repos json -o - "$NEXT" 2>/dev/null | tr -d '\\' || true)
if [ -n "$listed" ] && [ "$listed" != "[]" ]; then
  for repo in $SERVED; do
    printf '%s' "$listed" | grep -qiF "\"$repo\"" || plutil -insert repos -string "$repo" -append "$NEXT"
  done
fi
if ! "$RUNNER" check --config "$NEXT" > "$DOWNLOAD/check.txt" 2>&1; then
  rm -f "$NEXT"
  echo "not changed: the Runner refused the new settings:"
  tail -20 "$DOWNLOAD/check.txt"
  exit 1
fi

# Swapped in; the running Runner looks every 30 seconds.
lines=$(wc -l < "$LOG" | tr -d ' ')
mv "$NEXT" "$CONFIG"
said() { tail -n +"$((lines + 1))" "$LOG" | grep -F "$1" | tail -1; }
for minute in $(seq 1 "$WAIT_MINUTES"); do
  sleep 60
  if [ -n "$(said 'the Runner starts')" ]; then
    echo "done: the Runner started again on image $(plutil -extract image raw -o - "$CONFIG"), docker_repos $(plutil -extract docker_repos json -o - "$CONFIG" | tr -d '\\'), repos $(plutil -extract repos json -o - "$CONFIG" 2>/dev/null | tr -d '\\' || echo 'every private one')"
    rm -f "$CONFIG.before-docker"
    exit 0
  fi
  unusable=$(said 'no Runner could start on it')
  if [ -n "$unusable" ]; then
    mv "$CONFIG.before-docker" "$CONFIG"
    echo "not changed: the Runner would not start on the new file, which went back: $unusable"
    exit 1
  fi
  if [ "$minute" = 2 ] && [ -z "$(said 'new settings')" ]; then
    mv "$CONFIG.before-docker" "$CONFIG"
    echo "not changed: this Runner does not take up a changed file by itself (update it first); the file went back"
    exit 1
  fi
  gave_up=$(said 'new settings were not taken up')
  if [ -n "$gave_up" ]; then
    echo "waiting gave up: $gave_up. The new file stays; the Runner takes it up when it next starts, or when the file changes."
    exit 1
  fi
  [ $((minute % 5)) = 0 ] && echo "still waiting for the machines that stand to end ($minute min)"
done
echo "still waiting after $WAIT_MINUTES minutes; the new file stays and the Runner takes it up once no machine stands"
exit 1
