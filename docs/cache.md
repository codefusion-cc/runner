# Jobs' caches

A host keeps one cache per repository: a 20 GB disk (sparse: it holds what was written) with npm's cache, pnpm's
store and, for Docker jobs, the daemon's data, images and build cache. A job does not download, pull or build again
what the one before did. `cache_gb` (30) is what all caches of a host may hold together; 0 keeps none and deletes
those there were, and a host whose state folder cannot share blocks between files (APFS can; on Linux btrfs and XFS
can, ext4 cannot) keeps none either.

## What a job gets

- Only a private repository's jobs get a cache, and only when GitHub named its number and default branch
  (`cf_daemon::caches::CacheRepo::of`). A public repository's get none: anyone may open a pull request there, and
  the Runner gives a machine its disk at boot, before GitHub says which job it runs, so it cannot tell a pull request
  from a push then. A repository GitHub's listing did not say is private counts as public; a set made public since
  gives its next machine no disk.
- Every other job of a scale set gets a copy of its repository's cache that shares every block until it is written
  (APFS's clonefile, Linux's reflink), or an empty one the guest makes a file system on, writing no zeros, so a new
  cache holds of the host's disk what the job wrote. It writes only its copy: jobs of one repository that run at
  once never write the same file, and the cache is one job's copy, whole. The Runner's own machines (a test run,
  the Docker check) get none.
- The guest mounts it at `/mnt/cache`, without devices or set-id programs. `npm/` and `pnpm-store/` there are the
  job user's alone (0700), and only they are in the job's container; the top level and `docker/` are root's, and
  Docker's data reaches only the job's daemon. A link or a file where one of these folders goes is deleted, never
  followed: the guest's root works on the disk, and the job's root (sudo) could leave one there. The job is told
  `CF_JOB_CACHE=/mnt/cache`, `npm_config_cache=/mnt/cache/npm` and `pnpm_config_store_dir=/mnt/cache/pnpm-store`;
  its own environment wins. A disk that cannot be had leaves npm, pnpm and Docker on the job's own disk.
- At the job's end, once the job's control group holds no process, the guest deletes everything but packages and
  Docker's data (`cf_supervisor::cache::leftovers`): any other top-level entry, and in `npm/` all but `_cacache`
  (npm's logs, npx's installs). It gives what the job's root left in `npm/` and `pnpm-store/` back to the job's
  user (a folder it cannot give back is emptied), trims the file system so the host's copy gives back the blocks
  the job freed, then lets the disk go.
- A host with less free disk than one whole disk (20 GB) gives no copy: a copy can grow to that whatever it starts
  as.

## What becomes the cache

Once the job is over, the copy becomes the repository's cache only when everything says it may
(`cf_daemon::caches::refusal`):

- GitHub said the job succeeded, on a push, a schedule or a manual dispatch.
- The workflow is the repository's own, at `refs/heads/<its default branch>`.
- GitHub's REST API says the run checked out that branch of this repository, and its commit is that branch's tip or
  behind it, the branch being the repository's default as the newest look at GitHub named it (read while the job
  runs, or within two minutes of its end; a workflow's ref is where its file is, and a reusable workflow at the default
  branch called from another branch, or from a tag of the branch's name, runs that ref's code).
- The guest let the disk go cleanly, and for a Docker job its daemon still answered, held no container or volume
  (what the end hook could not remove would start again in the next job), and stopped by itself.

Anything else is thrown away: a pull request, a fork, another branch, a failed or cancelled job, a job GitHub never
said the end of, a run that could not be read, a disk let go uncleanly. So a cache holds what the default branch
downloaded and built, and nothing a pull request, a fork or another repository left. A pull request's job still
starts from the default branch's cache, as GitHub's own cache lets it: it can read all of it, private packages and
images the default branch fetched with its secrets among them. On a private repository only people who may read the
repository open pull requests, and what they can read in the cache is what they could build themselves. A cache's key
is GitHub's number for its repository (`repo-<id>`): a rename keeps the cache, and a repository made again under the
name of one that had a cache never gets it. Its record names the number too, which is checked before it is used, and
a record the Runner cannot read goes with its disk when the store opens.

## How it fails

- A cache that does not mount: the image's `e2fsck` checks it first; one it cannot make whole is made anew and the
  job runs on an empty one (`replaced`).
- A Docker job's daemon could not run on the cache: the cache is deleted (`dropped`), and the next job starts empty.
  Only a job that could have written the cache drops it, and only the cache it started from: a pull request that
  stops its own daemon, or a job that started before a newer cache was kept, drops nothing. A job without Docker
  never drops a cache.
- A cache that cannot be copied (its file unreadable) is deleted, and the job starts an empty one.
- A store that cannot share blocks any more, a full disk among the reasons, keeps no cache and deletes the copies a
  Runner before left. An image whose record was never written is deleted when the store opens.
- Over the budget the cache used longest ago goes (`evicted`), never one a job holds now.
- A job the host stopped (its deadline, a cancellation) is never kept, so nothing is tidied or waited for.
- The report's `jobCaches` and `jobCacheEvents` list each host's caches with their size and last write, and what
  last happened to them (`kept`, `discarded` with why, `replaced`, `dropped`, `evicted`).

## A Linux host on ext4

A cloud VM's boot disk is usually ext4, which cannot share blocks, so its host keeps no cache. Give it a second disk
formatted XFS (reflink is on by default since xfsprogs 5.1), mounted at the state folder's `job-cache` before the
Runner starts. Copies and caches both live in that folder, so one disk holds them all. On GCP, with the VM paused in
the console and its jobs ended (`<vm>`, `<project>`, `<zone>`, `<user>` and `<state>` are the host's own):

```bash
gcloud compute disks create <vm>-job-cache --project <project> --zone <zone> --type pd-balanced --size 40GB
gcloud compute instances attach-disk <vm> --project <project> --zone <zone> --disk <vm>-job-cache --device-name job-cache
```

On the VM, as root:

```bash
mkfs.xfs -L job-cache /dev/disk/by-id/google-job-cache
mkdir -p <state>/job-cache
echo 'LABEL=job-cache <state>/job-cache xfs defaults,nofail 0 2' >> /etc/fstab
systemctl daemon-reload && mount <state>/job-cache
chown <user>: <state>/job-cache && chmod 700 <state>/job-cache
```

Then restart the Runner and resume the VM. `nofail` boots the VM without the disk, and the Runner then starts with
no cache instead of not at all. An attached disk stays through a spot VM's stop and start, and survives the VM's
deletion (`--auto-delete` is off for an attached disk), so a rebuilt VM attaches the same one, with no `mkfs`. Keep
`cache_gb` below the disk's size: copies of running jobs need room beside the caches.

## Tests

- `crates/daemon/tests/caches.rs`: the rule (each condition refusing alone, with and without Docker, and a property:
  nothing but a successful run of the repository's own workflow on its default branch with a clean disk is kept),
  the key (one per repository number, two numbers never one), who gets a cache (a private repository GitHub named
  fully, never a public one or one whose listing left something out), a repository made again under a name, eviction (least recently used first, none
  in use), and the store on a real file system: kept, thrown away, replaced, dropped, a record naming another
  repository, a copy in use or whose machine never started, sweeping after a restart, a budget of 0, and what a
  store reads back.
- `crates/daemon/tests/job_cache.rs`: the same through the daemon: a push to main keeps its copy and the next job
  starts from it; a pull request, another branch, a failure, a cancellation, another repository's workflow, an
  unclean disk, a job GitHub never said the end of, a reusable workflow at main run from another branch and a run
  never read do not; a copy outlives a restart; two jobs at once each write their own copy; a Docker job and a job
  without Docker start from one cache; only a Docker job whose daemon failed drops it.
- `crates/daemon/tests/service.rs`: against a fake GitHub, a public repository's job gets no disk while a private
  one's on the same host does, a repository made public gives its next machine none, and only a run whose commit is
  on the default branch writes the cache.
- `crates/supervisor/tests/decisions.rs`: what the job is told and that its own entries win; what the job's end
  deletes, a link where a kept folder goes among it (and a property: never a kept folder, always the rest).
- `crates/guest-proto/tests/protocol.rs`: a cache end without Docker names no daemon.
- `cargo xtask it --only job-cache`, against real machines: a job without Docker finds npm and pnpm pointed at its
  cache, its own folders, and cannot write the top level; the next one on a copy finds the packages and not the
  first job's log, and what the first job's root left there is the user's again. `--only docker-cache`: a first
  Docker job fills an empty cache and leaves it clean, the next one on a copy has its image, none of its containers
  or volumes, and builds from the cache, and a disk of garbage is made anew.
