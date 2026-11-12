//! Container mode: each job in a container of its own, in any Docker-compatible engine (Docker, rootless Docker,
//! Podman, OrbStack), reached at the Unix socket the machine's settings name. A machine on a host that cannot run
//! virtual machines, or one that should not hold memory for each machine's kernel, runs its jobs here.
//!
//! [`OciVmm`] stands where a hypervisor stands ([`cf_vmm::Vmm`]), so the job's whole life above it (the vm-host,
//! its state machine, its waits, the daemon) is the same in both modes. A "machine" here is one container:
//!
//! - made from the image `codefusion-runner-guest:<version>` the engine holds ([`image_tag`]), with nothing of the
//!   host's mounted in it: no folder, no device and never the engine's socket;
//! - as the image's user `runner`, every capability dropped, `no-new-privileges`, never privileged;
//! - held to the machine's size of memory with no swap ([`cf_vmm::VmSpec::memory_bytes`]), its CPUs, and 4096
//!   processes;
//! - its writable layer, where its job's files go, held to the job's scratch size: by the engine where its storage
//!   can bound a layer ([`layer_bound`]), and everywhere by the vm-host, which measures it ([`Vm::layer_bytes`]) and
//!   stops a job past it;
//! - on a network of its own, made with it and named as it is, or none;
//! - with the guest's own supervisor, taken from the verified initramfs, put in at `/run/codefusion/supervisor`
//!   before the container starts, as its first process (`container-guest`);
//! - talked to over its standard input and output, attached once before it starts, its input closed when that attach
//!   goes, so the supervisor ends the job when its vm-host dies; its standard error is the console;
//! - thrown away, with its anonymous volumes and its network, when the vm-host lets go of it, and found and thrown
//!   away by the next vm-host when the one that made it is gone ([`OciVmm::new`]).

#[cfg(test)]
mod fake;
pub mod http;
pub mod image;
pub mod initramfs;
mod stream;
pub mod tar;

use cf_vmm::{Channel, ExitCell, Network, Owner, Vm, VmExit, VmSpec, Vmm, VmmError};
use http::{Body, Engine, escape};
use serde_json::json;
use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The label every container of the Runner's carries, with its machine's name.
pub const LABEL_MACHINE: &str = "cc.codefusion.runner.machine";
/// The label with the vm-host that made the container: its process id, start and boot ([`cf_vmm::Owner::label`]).
pub const LABEL_OWNER: &str = "cc.codefusion.runner.vm-host";
/// The repository of the job images in the engine.
pub const IMAGE_REPOSITORY: &str = "codefusion-runner-guest";
/// Where the supervisor is in the container.
pub const SUPERVISOR: &str = "/run/codefusion/supervisor";
/// The most processes a container holds.
const PIDS: u64 = 4096;
/// How long a call to the engine may take before it counts as a failure, but for a container's end.
pub(crate) const CALL: Duration = Duration::from_secs(60);
/// How long a network still in use is asked for again: Podman takes a container's network down in a process of its
/// own once the container has gone, so the network may still be busy just after.
const NETWORK_IN_USE: Duration = Duration::from_secs(10);

/// The engine image of an installed image version.
pub fn image_tag(version: &str) -> String {
    format!("{IMAGE_REPOSITORY}:{version}")
}

/// What the monitor is given.
#[derive(Debug, Clone)]
pub struct Config {
    /// The engine's Unix socket.
    pub socket: PathBuf,
    /// The machine's name, which every container it makes carries ([`LABEL_MACHINE`]).
    pub machine: String,
    /// Takes an engine whose containers' root is the host's ([`isolation`]): for the real-engine tests on a
    /// throwaway host only. The Runner never sets it.
    pub rootful_for_tests: bool,
}

/// Containers of one engine as machines.
pub struct OciVmm {
    engine: Engine,
    machine: String,
    /// This process, as what it makes is labelled.
    owner: Owner,
    next: AtomicU64,
    /// Whether the engine's storage bounds a container's layer ([`layer_bound`]); off for good once it refuses.
    bounds_layer: AtomicBool,
}

fn engine_error(what: &str, error: impl std::fmt::Display) -> String {
    format!("{what}: {error}")
}

/// What `check` says of the engine at `socket` for jobs of the image `version`, changing nothing: that it answers,
/// what keeps its containers' root from the host's ([`isolation`]), and that it holds the image.
pub fn probe(socket: &std::path::Path, version: &str) -> Result<String, String> {
    let engine = Engine::new(socket);
    let call = |path: &str| {
        engine
            .request("GET", path, Body::Empty, Some(Duration::from_secs(10)))
            .map_err(|error| format!("the engine at {} does not answer: {error}", socket.display()))
    };
    let info = call("/info")?;
    if !info.ok() {
        return Err(format!(
            "the engine at {} answers {}: {}",
            socket.display(),
            info.status,
            info.message()
        ));
    }
    let info = info
        .json()
        .map_err(|error| format!("the engine's /info: {error}"))?;
    let isolated = isolation(&info)?;
    let tag = image_tag(version);
    let image = call(&format!("/images/{}/json", escape(&tag)))?;
    if !image.ok() {
        return Err(format!(
            "the engine at {} holds no {tag}; run `image install`",
            socket.display()
        ));
    }
    Ok(format!(
        "the engine at {} ({isolated}) holds {tag}",
        socket.display()
    ))
}

/// Throws away what vm-hosts of `machine` that are gone left in the engine at `socket`, as [`OciVmm::new`] does
/// first: the daemon's own look, now and then, so that a container whose vm-host died goes while no vm-host starts.
/// An engine that does not answer is left for the next look.
pub fn sweep(socket: &Path, machine: &str) -> Result<(), String> {
    OciVmm::of(socket, machine)?.sweep(Owner::alive);
    Ok(())
}

impl OciVmm {
    /// The engine at `config.socket`, once it answers. Containers of this machine whose vm-host is gone are thrown
    /// away first, with what they hold.
    pub fn new(config: Config) -> Result<Self, VmmError> {
        let vmm = Self::of(&config.socket, &config.machine).map_err(VmmError::Unsupported)?;
        let engine = vmm.engine.clone();
        let ping = engine
            .request("GET", "/_ping", Body::Empty, Some(Duration::from_secs(10)))
            .map_err(|error| {
                VmmError::Unsupported(format!(
                    "the engine at {} does not answer: {error}",
                    config.socket.display()
                ))
            })?;
        if !ping.ok() {
            return Err(VmmError::Unsupported(format!(
                "the engine at {} answers {}: {}",
                config.socket.display(),
                ping.status,
                ping.message()
            )));
        }
        let info = engine
            .request("GET", "/info", Body::Empty, Some(CALL))
            .ok()
            .filter(http::Response::ok)
            .and_then(|answer| answer.json().ok())
            .ok_or_else(|| VmmError::Unsupported("the engine does not say what it is (/info)".into()))?;
        if !config.rootful_for_tests {
            isolation(&info).map_err(VmmError::Unsupported)?;
        }
        vmm.bounds_layer.store(layer_bound(&info), Ordering::Relaxed);
        vmm.sweep(Owner::alive);
        Ok(vmm)
    }

    /// The engine at `socket` for the machine `machine`, asked nothing yet.
    fn of(socket: &Path, machine: &str) -> Result<Self, String> {
        if !plain(machine) {
            return Err(format!("{machine:?} is no machine name"));
        }
        Ok(Self {
            engine: Engine::new(socket),
            machine: machine.to_string(),
            owner: Owner::this().ok_or("the system does not say when this process started")?,
            next: AtomicU64::new(0),
            bounds_layer: AtomicBool::new(false),
        })
    }

    /// Throws away every container and network of this machine whose vm-host is not `alive`, the containers first,
    /// as a network a container is on cannot go. What names no owner as [`Owner::label`] writes it is nobody's.
    pub fn sweep(&self, alive: impl Fn(&Owner) -> bool) {
        for (id, owner) in self.containers() {
            if !owner.as_ref().is_some_and(&alive) {
                remove(&self.engine, &id);
            }
        }
        for (id, owner) in self.networks() {
            if !owner.as_ref().is_some_and(&alive) {
                remove_network(&self.engine, &id);
            }
        }
    }

    /// The containers of this machine, each with the vm-host that made it, when its label names one.
    pub fn containers(&self) -> Vec<(String, Option<Owner>)> {
        self.labelled("/containers/json?all=1&filters=")
    }

    /// The networks of this machine, each with the vm-host that made it, when its label names one.
    pub fn networks(&self) -> Vec<(String, Option<Owner>)> {
        self.labelled("/networks?filters=")
    }

    /// What a list call of the engine names of this machine's, with each one's vm-host.
    fn labelled(&self, list: &str) -> Vec<(String, Option<Owner>)> {
        let filters = json!({ "label": [format!("{LABEL_MACHINE}={}", self.machine)] }).to_string();
        let path = format!("{list}{}", escape(&filters));
        let Ok(answer) = self.engine.request("GET", &path, Body::Empty, Some(CALL)) else {
            return Vec::new();
        };
        let Ok(serde_json::Value::Array(list)) = answer.json() else {
            return Vec::new();
        };
        list.iter()
            .filter_map(|item| {
                let id = item.get("Id")?.as_str()?.to_string();
                let owner = item
                    .get("Labels")
                    .and_then(|labels| labels.get(LABEL_OWNER))
                    .and_then(|owner| owner.as_str())
                    .and_then(Owner::parse);
                Some((id, owner))
            })
            .collect()
    }

    /// The labels of what this vm-host makes.
    fn labels(&self) -> serde_json::Value {
        json!({
            LABEL_MACHINE: self.machine,
            LABEL_OWNER: self.owner.label(),
        })
    }

    /// A network for one container, named as it is: it reaches the internet, as a machine's NAT does, and no other
    /// job's container, which is on a network of its own.
    fn network(&self, name: &str) -> Result<OwnNetwork, String> {
        let body = json!({
            "Name": name,
            "Driver": "bridge",
            "CheckDuplicate": true,
            "Internal": false,
            "Attachable": false,
            "Options": {
                "com.docker.network.bridge.enable_icc": "false",
                // Podman's netavark: the same, said its way.
                "isolate": "true",
            },
            "Labels": self.labels(),
        });
        let answer = self
            .engine
            .request("POST", "/networks/create", Body::Json(&body), Some(CALL))
            .map_err(|error| engine_error("the network", error))?;
        if answer.ok() {
            Ok(OwnNetwork {
                engine: self.engine.clone(),
                name: name.to_string(),
            })
        } else {
            Err(format!(
                "the network was refused ({}): {}",
                answer.status,
                answer.message()
            ))
        }
    }
}

/// What keeps a container's root from the host's, as the engine's `/info` tells it, or why nothing does. A job
/// runs as an unprivileged user with no capabilities, but a kernel flaw that makes it root in its container must
/// not make it the host's root, so the engine must be one of:
///
/// - rootless (Docker's or Podman's `name=rootless`): the engine and every container are a user's;
/// - with user namespaces (Docker's `userns-remap`, `name=userns`): a container's root is an unprivileged user of
///   the host;
/// - in a virtual machine of its own (OrbStack, Docker Desktop), whose root is not the host's.
pub fn isolation(info: &serde_json::Value) -> Result<&'static str, String> {
    let options: Vec<&str> = info
        .get("SecurityOptions")
        .and_then(serde_json::Value::as_array)
        .map(|options| options.iter().filter_map(serde_json::Value::as_str).collect())
        .unwrap_or_default();
    let has = |name: &str| {
        options
            .iter()
            .any(|option| option.split(',').any(|part| part == format!("name={name}")))
    };
    if has("rootless") {
        return Ok("rootless");
    }
    if has("userns") {
        return Ok("user namespaces");
    }
    let system = info
        .get("OperatingSystem")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if system == "OrbStack" || system == "Docker Desktop" {
        return Ok("a virtual machine of its own");
    }
    Err(format!(
        "the engine runs containers as the host's root ({}): run it rootless, or with user namespaces \
         (`userns-remap`)",
        if system.is_empty() {
            "it names no system"
        } else {
            system
        }
    ))
}

/// Whether the engine's storage, as its `/info` tells it, can hold a container's writable layer to a size
/// (`StorageOpt.size`): overlay on xfs, by the file system's project quotas, which only root sets, so never a
/// rootless engine. An xfs mounted without project quotas says so only when a container asks: the first refusal
/// turns the bound off ([`OciVmm::create`]).
pub fn layer_bound(info: &serde_json::Value) -> bool {
    let driver = info.get("Driver").and_then(serde_json::Value::as_str);
    let backing = info
        .get("DriverStatus")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|pair| match pair.as_array()?.as_slice() {
            [name, value] if name.as_str()? == "Backing Filesystem" => value.as_str(),
            _ => None,
        });
    let rootless = info
        .get("SecurityOptions")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .any(|option| option.split(',').any(|part| part == "name=rootless"));
    matches!(driver, Some("overlay" | "overlay2")) && backing == Some("xfs") && !rootless
}

/// Whether a refused container was refused for the bound of its layer, which this engine's storage cannot give.
fn layer_bound_refused(answer: &http::Response) -> bool {
    let message = answer.message().to_ascii_lowercase();
    matches!(answer.status, 400 | 500)
        && ["storage-opt", "storage opt", "storageopt", "quota"]
            .iter()
            .any(|word| message.contains(word))
}

/// What a container holds in its writable layer, from the engine's look at it with its size (`SizeRw`); an engine
/// that could not tell says -1, or nothing.
pub fn layer_of(inspect: &serde_json::Value) -> Option<u64> {
    u64::try_from(inspect.get("SizeRw")?.as_i64()?).ok()
}

/// Lowercase letters, digits and dashes: what goes into a container's name and a label.
fn plain(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// The version of the image a machine of this specification runs: the folder its image disk is installed in.
fn image_version(spec: &VmSpec) -> Result<String, VmmError> {
    let disk = spec
        .disks
        .iter()
        .find(|disk| disk.id == cf_guest_proto::IMAGE_DISK)
        .ok_or_else(|| VmmError::Spec("no image disk".into()))?;
    let version = disk
        .path
        .parent()
        .and_then(|folder| folder.file_name())
        .and_then(|name| name.to_str())
        .filter(|version| {
            !version.is_empty()
                && version.len() <= 128
                && version
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
                && !version.starts_with(['-', '.'])
        })
        .ok_or_else(|| VmmError::Spec(format!("{} names no image version", disk.path.display())))?;
    Ok(version.to_string())
}

/// The image's user `runner`: its id and group, from the image's `/etc/passwd`.
fn runner_of(passwd: &str) -> Option<(u32, u32)> {
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        (fields.next()? == "runner").then_some(())?;
        let _password = fields.next()?;
        Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
    })
}

/// Stops a container at once and removes it with its anonymous volumes; what is not there is not an error.
fn remove(engine: &Engine, id: &str) {
    let _ = engine.request(
        "DELETE",
        &format!("/containers/{}?force=1&v=1", escape(id)),
        Body::Empty,
        Some(CALL),
    );
}

/// Removes a network; what is not there is not an error.
fn remove_network(engine: &Engine, id: &str) {
    remove_network_within(engine, id, NETWORK_IN_USE);
}

/// Removes a network, asked again while the engine says it is in use (Docker 403, Podman 409 or 500), for `limit`.
fn remove_network_within(engine: &Engine, id: &str, limit: Duration) {
    let began = Instant::now();
    let mut wait = Duration::from_millis(50);
    loop {
        let Ok(answer) = engine.request(
            "DELETE",
            &format!("/networks/{}", escape(id)),
            Body::Empty,
            Some(CALL),
        ) else {
            return;
        };
        if !matches!(answer.status, 403 | 409 | 500) || began.elapsed() + wait > limit {
            return;
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(Duration::from_secs(1));
    }
}

/// A container's own network, removed when this goes out of scope: after the container, which is dropped first.
struct OwnNetwork {
    engine: Engine,
    name: String,
}

impl Drop for OwnNetwork {
    fn drop(&mut self) {
        remove_network(&self.engine, &self.name);
    }
}

/// The container and its network, removed when this goes out of scope, however its making ends; taken apart
/// once the machine stands.
struct Made {
    engine: Engine,
    /// Empty once taken.
    id: String,
    network: Option<OwnNetwork>,
}

impl Drop for Made {
    fn drop(&mut self) {
        if !self.id.is_empty() {
            remove(&self.engine, &self.id);
        }
        // The network goes after, with the fields.
    }
}

impl OciVmm {
    /// The container's settings: see the crate's documentation for each.
    fn create_body(&self, spec: &VmSpec, version: &str, network: Option<&str>) -> serde_json::Value {
        let memory = spec.memory_bytes;
        let mut body = json!({
            "Image": image_tag(version),
            "User": "runner",
            "Entrypoint": [SUPERVISOR],
            "Cmd": ["container-guest"],
            "Env": [],
            "WorkingDir": "/",
            "Hostname": self.machine,
            "AttachStdin": true,
            "AttachStdout": true,
            "AttachStderr": true,
            "OpenStdin": true,
            // The input ends with the one attach, the vm-host's: a vm-host that dies ends its job.
            "StdinOnce": true,
            "Tty": false,
            "NetworkDisabled": network.is_none(),
            "Labels": self.labels(),
            "HostConfig": {
                "Memory": memory,
                // Memory and swap alike: no swap. No swappiness, which crun refuses under cgroup v2 (Podman).
                "MemorySwap": memory,
                "NanoCpus": spec.cpus as u64 * 1_000_000_000,
                "PidsLimit": PIDS,
                "CapDrop": ["ALL"],
                "CapAdd": [],
                "SecurityOpt": ["no-new-privileges"],
                "Privileged": false,
                "ReadonlyRootfs": false,
                "Init": false,
                "AutoRemove": false,
                "Binds": [],
                "Mounts": [],
                "Devices": [],
                "VolumesFrom": [],
                "PublishAllPorts": false,
                "IpcMode": "private",
                "PidMode": "",
                "CgroupnsMode": "private",
                "ShmSize": (memory / 2).min(1 << 30),
                "NetworkMode": network.unwrap_or("none"),
                // The vm-host keeps what the container says; the engine keeps nothing that could fill its disk.
                "LogConfig": { "Type": "none", "Config": {} },
                "RestartPolicy": { "Name": "no" },
            },
        });
        if let Some(bytes) = spec.layer_bytes
            && self.bounds_layer.load(Ordering::Relaxed)
        {
            body["HostConfig"]["StorageOpt"] = json!({ "size": bytes.to_string() });
        }
        body
    }

    /// Makes the container `name` and returns its id. A bound of its layer the engine's storage cannot give is
    /// dropped, for this container and every later one, which the vm-host's looks hold to their size alone.
    fn create(
        &self,
        name: &str,
        spec: &VmSpec,
        version: &str,
        network: Option<&str>,
    ) -> Result<String, String> {
        let answer = loop {
            let bounded = spec.layer_bytes.is_some() && self.bounds_layer.load(Ordering::Relaxed);
            let answer = self
                .engine
                .request(
                    "POST",
                    &format!("/containers/create?name={}", escape(name)),
                    Body::Json(&self.create_body(spec, version, network)),
                    Some(CALL),
                )
                .map_err(|error| engine_error("the container", error))?;
            if bounded && layer_bound_refused(&answer) {
                self.bounds_layer.store(false, Ordering::Relaxed);
                continue;
            }
            break answer;
        };
        if answer.status == 404 {
            return Err(format!(
                "the engine has no image {}: install it first ({})",
                image_tag(version),
                answer.message()
            ));
        }
        if !answer.ok() {
            return Err(format!(
                "the container was refused ({}): {}",
                answer.status,
                answer.message()
            ));
        }
        let made = answer.json().unwrap_or_default();
        let id = made
            .get("Id")
            .and_then(serde_json::Value::as_str)
            .ok_or("the engine named no container")?
            .to_string();
        // An engine that cannot give a limit makes the container without it and warns: a job is not run without
        // its limits.
        let warnings: Vec<String> = made
            .get("Warnings")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .map(|warning| {
                warning
                    .as_str()
                    .map_or_else(|| warning.to_string(), str::to_string)
            })
            .collect();
        if !warnings.is_empty() {
            remove(&self.engine, &id);
            return Err(format!(
                "the engine made the container with a warning, so not as asked: {}",
                warnings.join("; ")
            ));
        }
        Ok(id)
    }

    fn make(&self, spec: &VmSpec) -> Result<OciVm, String> {
        let version = image_version(spec).map_err(|error| error.to_string())?;
        let initramfs = fs::read(&spec.initramfs).map_err(|error| engine_error("the initramfs", error))?;
        let supervisor = initramfs::file(&initramfs, "init")?;
        drop(initramfs);
        let console = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&spec.console)
            .map_err(|error| engine_error(&spec.console.display().to_string(), error))?;
        let name = format!(
            "codefusion-{}-vm-{}-{}",
            self.machine,
            std::process::id(),
            self.next.fetch_add(1, Ordering::Relaxed)
        );
        let network = match spec.network {
            Network::Nat => Some(self.network(&name)?),
            Network::None => None,
        };
        let mut made = Made {
            engine: self.engine.clone(),
            id: String::new(),
            network,
        };
        let id = self.create(
            &name,
            spec,
            &version,
            made.network.as_ref().map(|network| network.name.as_str()),
        )?;
        made.id.clone_from(&id);
        let passwd = self
            .engine
            .request(
                "GET",
                &format!("/containers/{}/archive?path=%2Fetc%2Fpasswd", escape(&id)),
                Body::Empty,
                Some(CALL),
            )
            .map_err(|error| engine_error("the image's users", error))?;
        let (uid, gid) = tar::read_file(&passwd.body, "passwd")
            .and_then(|text| runner_of(&String::from_utf8_lossy(&text)))
            .filter(|(uid, _)| *uid != 0)
            .ok_or("the image has no user runner other than root")?;
        let archive = tar::write(&[
            tar::Entry {
                path: "run/codefusion",
                data: None,
                mode: 0o755,
                uid,
                gid,
            },
            tar::Entry {
                path: &SUPERVISOR[1..],
                data: Some(&supervisor),
                mode: 0o755,
                uid: 0,
                gid: 0,
            },
        ])?;
        let put = self
            .engine
            .request(
                "PUT",
                &format!(
                    "/containers/{}/archive?path=%2F&noOverwriteDirNonDir=true",
                    escape(&id)
                ),
                Body::Tar(&archive),
                Some(CALL),
            )
            .map_err(|error| engine_error("the supervisor", error))?;
        if !put.ok() {
            return Err(format!(
                "the supervisor was refused ({}): {}",
                put.status,
                put.message()
            ));
        }
        let (attached, early) = self
            .engine
            .attach(&format!(
                "/containers/{}/attach?stream=1&stdin=1&stdout=1&stderr=1",
                escape(&id)
            ))
            .map_err(|error| engine_error("the container's streams", error))?;
        let channel =
            stream::split(attached, early, console).map_err(|error| engine_error("the streams", error))?;
        let started = self
            .engine
            .request(
                "POST",
                &format!("/containers/{}/start", escape(&id)),
                Body::Empty,
                Some(CALL),
            )
            .map_err(|error| engine_error("the start", error))?;
        if !started.ok() && started.status != 304 {
            channel.shutdown();
            return Err(format!(
                "the container did not start ({}): {}",
                started.status,
                started.message()
            ));
        }
        let exit = Arc::new(ExitCell::default());
        watch_end(self.engine.clone(), id.clone(), exit.clone());
        // From here the machine's own value removes them.
        made.id.clear();
        Ok(OciVm {
            engine: self.engine.clone(),
            id,
            exit,
            channel: Mutex::new(Some(channel)),
            _network: made.network.take(),
        })
    }
}

impl Vmm for OciVmm {
    fn start(&self, spec: &VmSpec) -> Result<Box<dyn Vm>, VmmError> {
        spec.validate()?;
        self.make(spec)
            .map(|vm| Box::new(vm) as Box<dyn Vm>)
            .map_err(VmmError::Start)
    }

    fn containers(&self) -> bool {
        true
    }
}

/// Waits for the container's end on a thread of its own, and says how it came.
fn watch_end(engine: Engine, id: String, exit: Arc<ExitCell>) {
    std::thread::spawn(move || {
        let waited = engine.request(
            "POST",
            &format!("/containers/{}/wait?condition=not-running", escape(&id)),
            Body::Empty,
            None,
        );
        let code = match waited {
            Ok(answer) if answer.ok() => answer
                .json()
                .ok()
                .and_then(|value| value.get("StatusCode")?.as_i64()),
            // A container the host stopped or removed may end the wait this way too: then it was the host's stop.
            Ok(answer) => {
                exit.found_failed(format!(
                    "the engine's wait answered {}: {}",
                    answer.status,
                    answer.message()
                ));
                return;
            }
            Err(error) => {
                exit.found_failed(format!("the engine was lost: {error}"));
                return;
            }
        };
        // A clean end needs no look at the container: only a failure may be the kernel's doing.
        let oom = code != Some(0)
            && engine
                .request(
                    "GET",
                    &format!("/containers/{}/json", escape(&id)),
                    Body::Empty,
                    Some(CALL),
                )
                .ok()
                .and_then(|answer| answer.json().ok())
                .and_then(|value| value.get("State")?.get("OOMKilled")?.as_bool())
                .unwrap_or(false);
        match code {
            // The supervisor ends with 0 after it told the host how the job ended; a kill ends it with 137.
            Some(0) => exit.found_stopped(),
            _ if oom => exit.end(VmExit::Error(
                "the kernel stopped the container's first process for memory".into(),
            )),
            Some(code) => exit.found_failed(format!("the container ended with {code}")),
            None => exit.end(VmExit::Error(
                "the engine did not say how the container ended".into(),
            )),
        }
    });
}

/// One container as a machine.
pub struct OciVm {
    engine: Engine,
    id: String,
    exit: Arc<ExitCell>,
    channel: Mutex<Option<stream::AttachChannel>>,
    /// Removed after the container, when this is dropped.
    _network: Option<OwnNetwork>,
}

impl Vm for OciVm {
    fn channel(&self, _port: u32) -> Result<Box<dyn Channel>, VmmError> {
        let taken = self.channel.lock().ok().and_then(|mut slot| slot.take());
        taken
            .map(|channel| Box::new(channel) as Box<dyn Channel>)
            .ok_or(VmmError::Connect {
                port: cf_guest_proto::PORT,
                reason: "the container's streams are taken".into(),
            })
    }

    fn wait(&self, timeout: Duration) -> Option<VmExit> {
        self.exit.wait(timeout)
    }

    fn stop(&self) -> Result<(), VmmError> {
        self.exit.stop_asked();
        let answer = self
            .engine
            .request(
                "POST",
                &format!("/containers/{}/kill?signal=SIGKILL", escape(&self.id)),
                Body::Empty,
                Some(CALL),
            )
            .map_err(|error| VmmError::Stop(error.to_string()))?;
        if answer.ok() {
            Ok(())
        } else {
            Err(VmmError::Stop(format!("{}: {}", answer.status, answer.message())))
        }
    }

    fn kill(&self) {
        self.exit.stop_asked();
        remove(&self.engine, &self.id);
        self.exit.end(VmExit::Stopped);
    }

    fn resident(&self) -> Option<u64> {
        let answer = self
            .engine
            .request(
                "GET",
                &format!(
                    "/containers/{}/stats?stream=false&one-shot=true",
                    escape(&self.id)
                ),
                Body::Empty,
                Some(Duration::from_secs(10)),
            )
            .ok()?;
        resident_of(&answer.json().ok()?)
    }

    fn layer_bytes(&self) -> Option<u64> {
        let answer = self
            .engine
            .request(
                "GET",
                &format!("/containers/{}/json?size=1", escape(&self.id)),
                Body::Empty,
                Some(CALL),
            )
            .ok()
            .filter(http::Response::ok)?;
        layer_of(&answer.json().ok()?)
    }
}

/// What of the host's memory a container holds, from the engine's statistics: its usage less the page cache the
/// kernel takes back at will (`inactive_file`, cgroup v2; `total_inactive_file`, v1).
pub fn resident_of(stats: &serde_json::Value) -> Option<u64> {
    let memory = stats.get("memory_stats")?;
    let usage = memory.get("usage")?.as_u64()?;
    let inactive = memory
        .get("stats")
        .and_then(|stats| {
            stats
                .get("inactive_file")
                .or_else(|| stats.get("total_inactive_file"))
        })
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    Some(usage.saturating_sub(inactive))
}

impl Drop for OciVm {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.channel.lock()
            && let Some(channel) = slot.take()
        {
            channel.shutdown();
        }
        remove(&self.engine, &self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An engine that answers each request with the next status of `statuses` (the last again once they run out),
    /// and counts them.
    fn scripted_engine(statuses: Vec<u16>) -> (tempfile::TempDir, Engine, Arc<AtomicU64>) {
        use std::io::{BufRead as _, BufReader, Write as _};
        let folder = tempfile::tempdir().unwrap();
        let socket = folder.path().join("engine.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let asked = Arc::new(AtomicU64::new(0));
        let counted = asked.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|read| read > 0) && line != "\r\n" {
                    line.clear();
                }
                let n = counted.fetch_add(1, Ordering::SeqCst) as usize;
                let status = statuses[n.min(statuses.len() - 1)];
                let _ = write!(
                    reader.get_mut(),
                    "HTTP/1.1 {status} X\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                );
            }
        });
        (folder, Engine::new(&socket), asked)
    }

    #[test]
    fn a_network_still_in_use_is_asked_for_again_until_it_goes_or_the_wait_ends() {
        let (_folder, engine, asked) = scripted_engine(vec![500, 409, 403, 204]);
        remove_network_within(&engine, "codefusion-host-1-vm-7-0", Duration::from_secs(10));
        assert_eq!(asked.load(Ordering::SeqCst), 4, "asked until it was removed");

        let (_folder, engine, asked) = scripted_engine(vec![404]);
        remove_network_within(&engine, "gone", Duration::from_secs(10));
        assert_eq!(
            asked.load(Ordering::SeqCst),
            1,
            "one that is not there is not asked for again"
        );

        let (_folder, engine, asked) = scripted_engine(vec![500]);
        let began = Instant::now();
        remove_network_within(&engine, "busy", Duration::from_millis(300));
        assert!(began.elapsed() < Duration::from_secs(1), "the wait ends");
        assert!(asked.load(Ordering::SeqCst) >= 2, "asked again within it");
    }

    #[test]
    fn a_wait_that_fails_after_the_host_asked_for_a_stop_is_the_hosts_stop() {
        let ended = |engine: Engine, asked: bool| {
            let exit = Arc::new(ExitCell::default());
            if asked {
                exit.stop_asked();
            }
            watch_end(engine, "c1".into(), exit.clone());
            exit.wait(Duration::from_secs(10))
        };
        let (_folder, engine, _) = scripted_engine(vec![404]);
        assert_eq!(
            ended(engine, true),
            Some(VmExit::Stopped),
            "the removed container's 404"
        );
        assert_eq!(
            ended(Engine::new("/nowhere.sock"), true),
            Some(VmExit::Stopped),
            "the engine lost"
        );

        let (_folder, engine, _) = scripted_engine(vec![404]);
        assert!(matches!(ended(engine, false), Some(VmExit::Error(error)) if error.contains("404")));
        assert!(matches!(
            ended(Engine::new("/nowhere.sock"), false),
            Some(VmExit::Error(error)) if error.contains("lost")
        ));
    }

    #[test]
    fn only_an_engine_whose_root_is_not_the_hosts_is_taken() {
        let info =
            |options: &[&str], system: &str| json!({ "SecurityOptions": options, "OperatingSystem": system });
        let seccomp = "name=seccomp,profile=builtin";
        assert_eq!(
            isolation(&info(
                &[seccomp, "name=rootless", "name=cgroupns"],
                "Ubuntu 26.04 LTS"
            )),
            Ok("rootless")
        );
        assert_eq!(
            isolation(&info(&["name=userns", seccomp], "Debian GNU/Linux 13")),
            Ok("user namespaces")
        );
        assert_eq!(
            isolation(&info(&[seccomp], "OrbStack")),
            Ok("a virtual machine of its own")
        );
        assert_eq!(
            isolation(&info(&[], "Docker Desktop")),
            Ok("a virtual machine of its own")
        );
        for refused in [
            info(&[seccomp, "name=cgroupns"], "Ubuntu 26.04 LTS"),
            info(&["name=rootlessish", "rootless"], ""),
            json!({ "SecurityOptions": "name=rootless" }),
            json!({}),
        ] {
            let said = isolation(&refused).unwrap_err();
            assert!(said.contains("as the host's root"), "{said}");
        }
    }
    use cf_vmm::Disk;

    fn spec() -> VmSpec {
        VmSpec {
            kernel: "/state/run/job/kernel".into(),
            initramfs: "/state/run/job/initramfs".into(),
            memory_bytes: 4 << 30,
            cpus: 3,
            disks: vec![Disk {
                path: "/state/images/image-20261112-7/rootfs.img".into(),
                id: cf_guest_proto::IMAGE_DISK.into(),
                read_only: true,
            }],
            network: Network::Nat,
            console: "/state/logs/job.console.log".into(),
            layer_bytes: None,
        }
    }

    fn vmm() -> OciVmm {
        OciVmm::of(Path::new("/nowhere.sock"), "host-1").unwrap()
    }

    #[test]
    fn the_image_is_the_installed_version_the_image_disk_names() {
        assert_eq!(image_version(&spec()).unwrap(), "image-20261112-7");
        for path in [
            "/rootfs.img",
            "/state/images/../rootfs.img",
            "/state/images/a b/rootfs.img",
            "/s/.x/r",
        ] {
            let mut spec = spec();
            spec.disks[0].path = path.into();
            assert!(image_version(&spec).is_err(), "{path}");
        }
        let mut none = spec();
        none.disks.clear();
        assert!(image_version(&none).is_err());
    }

    #[test]
    fn a_container_is_held_in_as_a_machine_is() {
        let body = vmm().create_body(&spec(), "image-20261112-7", Some("codefusion-host-1-vm-7-0"));
        let host = &body["HostConfig"];
        assert_eq!(body["Image"], "codefusion-runner-guest:image-20261112-7");
        assert_eq!(body["User"], "runner", "never root");
        assert_eq!(host["Privileged"], false);
        assert_eq!(host["CapDrop"], json!(["ALL"]));
        assert_eq!(host["CapAdd"], json!([]));
        assert_eq!(host["SecurityOpt"], json!(["no-new-privileges"]));
        // Nothing of the host's: no folder, no device, no volume of another container, so never the engine's socket.
        for empty in ["Binds", "Mounts", "Devices", "VolumesFrom"] {
            assert_eq!(host[empty], json!([]), "{empty}");
        }
        assert_eq!(host["Memory"], 4u64 << 30);
        assert_eq!(host["MemorySwap"], host["Memory"], "no swap past the memory");
        assert!(
            host.get("MemorySwappiness").is_none(),
            "crun under cgroup v2 refuses any swappiness"
        );
        assert_eq!(host["NanoCpus"], 3_000_000_000u64);
        assert_eq!(host["PidsLimit"], PIDS);
        assert_eq!(host["PidMode"], "", "a process namespace of its own");
        assert_eq!(host["IpcMode"], "private");
        assert_eq!(host["CgroupnsMode"], "private");
        assert_eq!(host["NetworkMode"], "codefusion-host-1-vm-7-0");
        assert_eq!(host["LogConfig"]["Type"], "none");
        assert_eq!(body["Labels"][LABEL_MACHINE], "host-1");
        assert_eq!(body["Labels"][LABEL_OWNER], Owner::this().unwrap().label());
        assert_eq!(body["Entrypoint"], json!([SUPERVISOR]));
        assert_eq!(body["Cmd"], json!(["container-guest"]));
        let offline = vmm().create_body(
            &VmSpec {
                network: Network::None,
                ..spec()
            },
            "v",
            None,
        );
        assert_eq!(offline["HostConfig"]["NetworkMode"], "none");
        assert_eq!(offline["NetworkDisabled"], true);
    }

    /// The supervisor ends the job when its standard input ends. With `StdinOnce: false` the engine keeps the
    /// container's input open after the vm-host's attach goes, so a vm-host that dies (SIGKILL, OOM) would leave the
    /// job running past its timeout.
    #[test]
    fn the_containers_input_ends_with_its_vm_hosts_attach() {
        let body = vmm().create_body(&spec(), "image-20261112-7", Some("n"));
        assert_eq!(body["OpenStdin"], true);
        assert_eq!(
            body["StdinOnce"], true,
            "stdin stays open after the vm-host dies: the job outlives it"
        );
    }

    /// A machine has a scratch disk of the job's size; a container writes into its layer on the engine's own file
    /// system. Where the engine's storage can bound a layer, the container is made with the job's size as its bound.
    #[test]
    fn a_containers_writable_layer_is_bounded_where_the_engines_storage_can() {
        let sized = VmSpec {
            layer_bytes: Some(1 << 30),
            ..spec()
        };
        let bounding = vmm();
        bounding.bounds_layer.store(true, Ordering::Relaxed);
        let body = bounding.create_body(&sized, "image-20261112-7", Some("n"));
        assert_eq!(body["HostConfig"]["StorageOpt"], json!({ "size": "1073741824" }));
        let no_size = bounding.create_body(&spec(), "image-20261112-7", Some("n"));
        assert!(
            no_size["HostConfig"].get("StorageOpt").is_none(),
            "no size, no bound"
        );
        let body = vmm().create_body(&sized, "image-20261112-7", Some("n"));
        assert!(
            body["HostConfig"].get("StorageOpt").is_none(),
            "an engine whose storage cannot bound a layer refuses the container that asks"
        );
    }

    #[test]
    fn only_overlay_on_xfs_of_an_engine_with_root_bounds_a_layer() {
        let info = |driver: &str, backing: &str, options: &[&str]| {
            json!({
                "Driver": driver,
                "DriverStatus": [["Backing Filesystem", backing], ["Supports d_type", "true"]],
                "SecurityOptions": options,
            })
        };
        assert!(layer_bound(&info("overlay2", "xfs", &["name=userns"])));
        assert!(layer_bound(&info(
            "overlay",
            "xfs",
            &["name=seccomp,profile=default"]
        )));
        // Project quotas are root's: a rootless engine's overlay has no bound to give.
        assert!(!layer_bound(&info("overlay", "xfs", &["name=rootless"])));
        assert!(!layer_bound(&info("overlay2", "extfs", &["name=userns"])));
        assert!(!layer_bound(&info("vfs", "xfs", &[])));
        assert!(!layer_bound(&json!({ "Driver": "overlay2" })));
        assert!(!layer_bound(&json!({})));
    }

    /// An xfs mounted without project quotas is found out by the first container that asks for a bound: the engine
    /// refuses it, and that one and every later container is made without, held to its size by the vm-host's looks.
    #[test]
    fn a_layer_bound_the_engine_refuses_is_dropped_once_and_for_all() {
        let engine = fake::FakeEngine::new(|asked| {
            let body: serde_json::Value = serde_json::from_slice(&asked.body).unwrap_or_default();
            if body["HostConfig"].get("StorageOpt").is_some() {
                (
                    400,
                    json!({ "message": "--storage-opt is supported only for overlay over xfs with 'pquota' mount option" })
                        .to_string(),
                )
            } else {
                (201, json!({ "Id": "c1", "Warnings": [] }).to_string())
            }
        });
        let vmm = OciVmm::of(&engine.socket, "host-1").unwrap();
        vmm.bounds_layer.store(true, Ordering::Relaxed);
        let sized = VmSpec {
            layer_bytes: Some(1 << 30),
            ..spec()
        };
        assert_eq!(vmm.create("c-0", &sized, "v", None).unwrap(), "c1");
        assert_eq!(vmm.create("c-1", &sized, "v", None).unwrap(), "c1");
        let bounds: Vec<bool> = engine
            .asked()
            .iter()
            .map(|asked| {
                let body: serde_json::Value = serde_json::from_slice(&asked.body).unwrap();
                body["HostConfig"].get("StorageOpt").is_some()
            })
            .collect();
        assert_eq!(bounds, [true, false, false]);

        // Any other refusal stays an error, and asks once.
        let engine = fake::FakeEngine::new(|_| (500, json!({ "message": "no space left" }).to_string()));
        let vmm = OciVmm::of(&engine.socket, "host-1").unwrap();
        vmm.bounds_layer.store(true, Ordering::Relaxed);
        let refused = vmm.create("c-0", &sized, "v", None).unwrap_err();
        assert!(refused.contains("no space left"), "{refused}");
        assert_eq!(engine.asked().len(), 1);
    }

    /// An engine that cannot hold a container to a limit (a kernel without the memory controller, a cgroup v1 host
    /// with no swap account) makes the container anyway and only warns. A job without its limits is not run, and
    /// the container that would have held it goes.
    #[test]
    fn a_container_the_engine_warns_of_is_removed_and_refused() {
        let warned = "Your kernel does not support memory limit capabilities or the cgroup is not mounted. Limitation discarded.";
        let engine = fake::FakeEngine::new(move |asked| match asked.method.as_str() {
            "POST" => (201, json!({ "Id": "c1", "Warnings": [warned] }).to_string()),
            _ => (204, String::new()),
        });
        let vmm = OciVmm::of(&engine.socket, "host-1").unwrap();
        let refused = vmm.create("c-0", &spec(), "v", None).unwrap_err();
        assert!(refused.contains("Limitation discarded"), "{refused}");
        assert_eq!(engine.lines("DELETE"), ["DELETE /containers/c1?force=1&v=1"]);

        for quiet in [
            json!({ "Id": "c1", "Warnings": [] }),
            json!({ "Id": "c1", "Warnings": null }),
            json!({ "Id": "c1" }),
        ] {
            let engine = fake::FakeEngine::new(move |_| (201, quiet.to_string()));
            let vmm = OciVmm::of(&engine.socket, "host-1").unwrap();
            assert_eq!(vmm.create("c-0", &spec(), "v", None).unwrap(), "c1");
            assert!(engine.lines("DELETE").is_empty());
        }
    }

    #[test]
    fn a_layers_size_is_what_the_engine_says_it_holds() {
        let engine = fake::FakeEngine::new(|asked| match asked.path.as_str() {
            "/containers/c1/json?size=1" => {
                (200, json!({ "Id": "c1", "SizeRw": 734_003_200u64 }).to_string())
            }
            _ => (404, json!({ "message": "no such container" }).to_string()),
        });
        let vm = |id: &str| OciVm {
            engine: Engine::new(&engine.socket),
            id: id.into(),
            exit: Arc::default(),
            channel: Mutex::new(None),
            _network: None,
        };
        assert_eq!(vm("c1").layer_bytes(), Some(734_003_200));
        assert_eq!(
            vm("c2").layer_bytes(),
            None,
            "a container that is gone holds nothing to tell"
        );
        assert_eq!(
            layer_of(&json!({ "SizeRw": -1 })),
            None,
            "an engine that could not tell"
        );
        assert_eq!(layer_of(&json!({ "SizeRw": "1" })), None);
        assert_eq!(layer_of(&json!({})), None);
        assert_eq!(layer_of(&json!({ "SizeRw": 0 })), Some(0));
    }

    /// An engine on a rootless host that holds one container and one network of `host-1` for each owner label.
    fn holding(owners: &[String]) -> fake::FakeEngine {
        let listed = |kind: &str| {
            serde_json::Value::Array(
                owners
                    .iter()
                    .enumerate()
                    .map(|(n, owner)| {
                        json!({ "Id": format!("{kind}{n}"), "Labels": { LABEL_MACHINE: "host-1", LABEL_OWNER: owner } })
                    })
                    .collect(),
            )
            .to_string()
        };
        let (containers, networks) = (listed("c"), listed("n"));
        fake::FakeEngine::new(move |asked| match (asked.method.as_str(), asked.path.as_str()) {
            ("GET", "/_ping") => (200, "\"OK\"".into()),
            ("GET", "/info") => (
                200,
                json!({ "SecurityOptions": ["name=rootless"], "OperatingSystem": "Ubuntu 26.04 LTS" })
                    .to_string(),
            ),
            ("GET", path) if path.starts_with("/containers/json") => (200, containers.clone()),
            ("GET", path) if path.starts_with("/networks") => (200, networks.clone()),
            ("DELETE", _) => (204, String::new()),
            _ => (404, "{\"message\":\"no such thing\"}".into()),
        })
    }

    /// After a reboot or a wrap the number a container's vm-host had is another process's. Pid 1 is init or
    /// launchd, never a vm-host: what a vm-host of that number left goes, with its writable layer and its network.
    #[test]
    fn a_pid_that_is_now_another_process_is_not_a_live_vm_host() {
        let this = Owner::this().unwrap();
        // macOS tells another user's process's start to root only.
        let init = Owner::of(1).unwrap_or(Owner {
            pid: 1,
            start: 1,
            boot: this.boot,
        });
        let engine = holding(&[
            "1".to_string(),
            Owner {
                start: init.start + 1,
                ..init
            }
            .label(),
            this.label(),
            Owner {
                boot: this.boot ^ 1,
                ..this
            }
            .label(),
            String::new(),
        ]);
        OciVmm::new(Config {
            socket: engine.socket.clone(),
            machine: "host-1".into(),
            rootful_for_tests: false,
        })
        .expect("the engine answers");
        let removed = |kind: &str, at: &str| {
            [0, 1, 3, 4]
                .map(|n| format!("DELETE /{kind}/{kind:.1}{n}{at}"))
                .to_vec()
        };
        let (containers, networks) = (removed("containers", "?force=1&v=1"), removed("networks", ""));
        assert_eq!(
            engine.lines("DELETE"),
            [containers, networks].concat(),
            "only what this live vm-host labelled stays; pid 1 (init) is never a vm-host that owns a container"
        );
    }

    #[test]
    fn the_runner_user_is_read_from_the_images_passwd() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\nrunner:x:1001:121::/home/runner:/bin/bash\n";
        assert_eq!(runner_of(passwd), Some((1001, 121)));
        assert_eq!(runner_of("root:x:0:0:root:/root:/bin/bash\n"), None);
        assert_eq!(runner_of("runner:x:abc:1::/:/\n"), None);
        assert_eq!(runner_of("runners:x:5:5::/:/\n"), None);
    }

    #[test]
    fn what_a_container_holds_leaves_out_the_cache_the_kernel_takes_back() {
        let v2 = json!({"memory_stats": {"usage": 900, "stats": {"inactive_file": 300}}});
        assert_eq!(resident_of(&v2), Some(600));
        let v1 = json!({"memory_stats": {"usage": 900, "stats": {"total_inactive_file": 100}}});
        assert_eq!(resident_of(&v1), Some(800));
        assert_eq!(resident_of(&json!({"memory_stats": {"usage": 5}})), Some(5));
        assert_eq!(
            resident_of(&json!({"memory_stats": {}})),
            None,
            "an ended container has none"
        );
        let odd = json!({"memory_stats": {"usage": 5, "stats": {"inactive_file": 9}}});
        assert_eq!(resident_of(&odd), Some(0));
    }

    #[test]
    fn names_that_go_into_containers_and_labels_are_plain() {
        assert!(plain("host-1"));
        assert!(plain("mac-mini-2"));
        for name in ["", "Dal1", "a b", "a/b", "a=b", &"a".repeat(65)] {
            assert!(!plain(name), "{name:?}");
        }
    }
}
