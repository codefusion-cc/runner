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
//! - on a network of the Runner's whose containers cannot reach each other ([`NETWORK`]), or none;
//! - with the guest's own supervisor, taken from the verified initramfs, put in at `/run/codefusion/supervisor`
//!   before the container starts, as its first process (`container-guest`);
//! - talked to over its standard input and output, attached before it starts; its standard error is the console;
//! - thrown away, with its anonymous volumes, when the vm-host lets go of it, and found and thrown away by the next
//!   vm-host when the one that made it is gone ([`OciVmm::new`]).

pub mod http;
pub mod initramfs;
mod stream;
pub mod tar;

use cf_vmm::{Channel, ExitCell, Network, Vm, VmExit, Vmm, VmmError, VmSpec};
use http::{Body, Engine, escape};
use serde_json::json;
use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The label every container of the Runner's carries, with its machine's name.
pub const LABEL_MACHINE: &str = "cc.codefusion.runner.machine";
/// The label with the process id of the vm-host that made the container.
pub const LABEL_OWNER: &str = "cc.codefusion.runner.vm-host";
/// The repository of the job images in the engine.
pub const IMAGE_REPOSITORY: &str = "codefusion-runner-guest";
/// Where the supervisor is in the container.
pub const SUPERVISOR: &str = "/run/codefusion/supervisor";
/// The prefix of the network the containers of a machine share, which lets none reach another.
pub const NETWORK: &str = "codefusion-runner-";
/// The most processes a container holds.
const PIDS: u64 = 4096;
/// How long a call to the engine may take before it counts as a failure, but for a container's end.
const CALL: Duration = Duration::from_secs(60);

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
}

/// Containers of one engine as machines.
pub struct OciVmm {
    engine: Engine,
    machine: String,
    next: AtomicU64,
}

fn engine_error(what: &str, error: impl std::fmt::Display) -> String {
    format!("{what}: {error}")
}

impl OciVmm {
    /// The engine at `config.socket`, once it answers. Containers of this machine whose vm-host is gone are thrown
    /// away first, with what they hold.
    pub fn new(config: Config) -> Result<Self, VmmError> {
        if !plain(&config.machine) {
            return Err(VmmError::Unsupported(format!("{:?} is no machine name", config.machine)));
        }
        let engine = Engine::new(&config.socket);
        let ping = engine
            .request("GET", "/_ping", Body::Empty, Some(Duration::from_secs(10)))
            .map_err(|error| {
                VmmError::Unsupported(format!("the engine at {} does not answer: {error}", config.socket.display()))
            })?;
        if !ping.ok() {
            return Err(VmmError::Unsupported(format!(
                "the engine at {} answers {}: {}",
                config.socket.display(),
                ping.status,
                ping.message()
            )));
        }
        let vmm = Self {
            engine,
            machine: config.machine,
            next: AtomicU64::new(0),
        };
        vmm.sweep(alive);
        Ok(vmm)
    }

    /// Throws away every container of this machine whose vm-host is not `alive`, with its anonymous volumes.
    pub fn sweep(&self, alive: impl Fn(u32) -> bool) {
        for (id, owner) in self.containers() {
            if !owner.is_some_and(&alive) {
                remove(&self.engine, &id);
            }
        }
    }

    /// The containers of this machine, each with the vm-host that made it, when its label names one.
    pub fn containers(&self) -> Vec<(String, Option<u32>)> {
        let filters = json!({ "label": [format!("{LABEL_MACHINE}={}", self.machine)] }).to_string();
        let path = format!("/containers/json?all=1&filters={}", escape(&filters));
        let Ok(answer) = self.engine.request("GET", &path, Body::Empty, Some(CALL)) else {
            return Vec::new();
        };
        let Ok(serde_json::Value::Array(list)) = answer.json() else {
            return Vec::new();
        };
        list.iter()
            .filter_map(|container| {
                let id = container.get("Id")?.as_str()?.to_string();
                let owner = container
                    .get("Labels")
                    .and_then(|labels| labels.get(LABEL_OWNER))
                    .and_then(|owner| owner.as_str())
                    .and_then(|owner| owner.parse().ok());
                Some((id, owner))
            })
            .collect()
    }

    /// The network this machine's containers share, made when it is not there. Docker's bridge option keeps the
    /// containers on it from reaching one another; they reach the internet, as a machine's NAT does.
    fn network(&self) -> Result<String, String> {
        let name = format!("{NETWORK}{}", self.machine);
        let path = format!("/networks/{}", escape(&name));
        if self.engine.request("GET", &path, Body::Empty, Some(CALL)).is_ok_and(|answer| answer.ok()) {
            return Ok(name);
        }
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
            "Labels": { LABEL_MACHINE: self.machine },
        });
        let answer = self
            .engine
            .request("POST", "/networks/create", Body::Json(&body), Some(CALL))
            .map_err(|error| engine_error("the network", error))?;
        // Another vm-host made it in between.
        if answer.ok() || answer.status == 409 {
            Ok(name)
        } else {
            Err(format!("the network was refused ({}): {}", answer.status, answer.message()))
        }
    }
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

/// Whether a process of this id is running.
fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: kill with signal 0 takes no pointers and sends nothing.
    pid > 0
        && (unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

/// The container, removed when this goes out of scope, however its making ends.
struct Made {
    engine: Engine,
    id: String,
}

impl Drop for Made {
    fn drop(&mut self) {
        remove(&self.engine, &self.id);
    }
}

impl OciVmm {
    /// The container's settings: see the crate's documentation for each.
    fn create_body(&self, spec: &VmSpec, version: &str, network: Option<&str>) -> serde_json::Value {
        let memory = spec.memory_bytes;
        json!({
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
            "StdinOnce": false,
            "Tty": false,
            "NetworkDisabled": network.is_none(),
            "Labels": {
                LABEL_MACHINE: self.machine,
                LABEL_OWNER: std::process::id().to_string(),
            },
            "HostConfig": {
                "Memory": memory,
                "MemorySwap": memory,
                "MemorySwappiness": 0,
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
        })
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
        let network = match spec.network {
            Network::Nat => Some(self.network()?),
            Network::None => None,
        };
        let name = format!(
            "codefusion-{}-vm-{}-{}",
            self.machine,
            std::process::id(),
            self.next.fetch_add(1, Ordering::Relaxed)
        );
        let body = self.create_body(spec, &version, network.as_deref());
        let answer = self
            .engine
            .request(
                "POST",
                &format!("/containers/create?name={}", escape(&name)),
                Body::Json(&body),
                Some(CALL),
            )
            .map_err(|error| engine_error("the container", error))?;
        if answer.status == 404 {
            return Err(format!(
                "the engine has no image {}: install it first ({})",
                image_tag(&version),
                answer.message()
            ));
        }
        if !answer.ok() {
            return Err(format!("the container was refused ({}): {}", answer.status, answer.message()));
        }
        let id = answer
            .json()
            .ok()
            .and_then(|value| Some(value.get("Id")?.as_str()?.to_string()))
            .ok_or("the engine named no container")?;
        let made = Made {
            engine: self.engine.clone(),
            id: id.clone(),
        };
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
                &format!("/containers/{}/archive?path=%2F&noOverwriteDirNonDir=true", escape(&id)),
                Body::Tar(&archive),
                Some(CALL),
            )
            .map_err(|error| engine_error("the supervisor", error))?;
        if !put.ok() {
            return Err(format!("the supervisor was refused ({}): {}", put.status, put.message()));
        }
        let (attached, early) = self
            .engine
            .attach(&format!(
                "/containers/{}/attach?stream=1&stdin=1&stdout=1&stderr=1",
                escape(&id)
            ))
            .map_err(|error| engine_error("the container's streams", error))?;
        let channel = stream::split(attached, early, console).map_err(|error| engine_error("the streams", error))?;
        let started = self
            .engine
            .request("POST", &format!("/containers/{}/start", escape(&id)), Body::Empty, Some(CALL))
            .map_err(|error| engine_error("the start", error))?;
        if !started.ok() && started.status != 304 {
            channel.shutdown();
            return Err(format!("the container did not start ({}): {}", started.status, started.message()));
        }
        let exit = Arc::new(ExitCell::default());
        watch_end(self.engine.clone(), id.clone(), exit.clone());
        // From here the machine's own value removes it.
        std::mem::forget(made);
        Ok(OciVm {
            engine: self.engine.clone(),
            id,
            exit,
            channel: Mutex::new(Some(channel)),
        })
    }
}

impl Vmm for OciVmm {
    fn start(&self, spec: &VmSpec) -> Result<Box<dyn Vm>, VmmError> {
        spec.validate()?;
        self.make(spec).map(|vm| Box::new(vm) as Box<dyn Vm>).map_err(VmmError::Start)
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
            Ok(answer) => {
                exit.end(VmExit::Error(format!("the engine's wait answered {}: {}", answer.status, answer.message())));
                return;
            }
            Err(error) => {
                exit.end(VmExit::Error(format!("the engine was lost: {error}")));
                return;
            }
        };
        let oom = engine
            .request("GET", &format!("/containers/{}/json", escape(&id)), Body::Empty, Some(CALL))
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
            None => exit.end(VmExit::Error("the engine did not say how the container ended".into())),
        }
    });
}

/// One container as a machine.
pub struct OciVm {
    engine: Engine,
    id: String,
    exit: Arc<ExitCell>,
    channel: Mutex<Option<stream::AttachChannel>>,
}

impl OciVm {
    /// The engine's id of the container.
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Vm for OciVm {
    fn channel(&self, _port: u32) -> Result<Box<dyn Channel>, VmmError> {
        let taken = self.channel.lock().ok().and_then(|mut slot| slot.take());
        taken.map(|channel| Box::new(channel) as Box<dyn Channel>).ok_or(VmmError::Connect {
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
                &format!("/containers/{}/stats?stream=false&one-shot=true", escape(&self.id)),
                Body::Empty,
                Some(Duration::from_secs(10)),
            )
            .ok()?;
        resident_of(&answer.json().ok()?)
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
        }
    }

    fn vmm() -> OciVmm {
        OciVmm {
            engine: Engine::new("/nowhere.sock"),
            machine: "dal1".into(),
            next: AtomicU64::new(0),
        }
    }

    #[test]
    fn the_image_is_the_installed_version_the_image_disk_names() {
        assert_eq!(image_version(&spec()).unwrap(), "image-20261112-7");
        for path in ["/rootfs.img", "/state/images/../rootfs.img", "/state/images/a b/rootfs.img", "/s/.x/r"] {
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
        let body = vmm().create_body(&spec(), "image-20261112-7", Some("codefusion-runner-dal1"));
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
        assert_eq!(host["NanoCpus"], 3_000_000_000u64);
        assert_eq!(host["PidsLimit"], PIDS);
        assert_eq!(host["PidMode"], "", "a process namespace of its own");
        assert_eq!(host["IpcMode"], "private");
        assert_eq!(host["CgroupnsMode"], "private");
        assert_eq!(host["NetworkMode"], "codefusion-runner-dal1");
        assert_eq!(host["LogConfig"]["Type"], "none");
        assert_eq!(body["Labels"][LABEL_MACHINE], "dal1");
        assert_eq!(body["Labels"][LABEL_OWNER], std::process::id().to_string());
        assert_eq!(body["Entrypoint"], json!([SUPERVISOR]));
        assert_eq!(body["Cmd"], json!(["container-guest"]));
        let offline = vmm().create_body(&VmSpec { network: Network::None, ..spec() }, "v", None);
        assert_eq!(offline["HostConfig"]["NetworkMode"], "none");
        assert_eq!(offline["NetworkDisabled"], true);
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
        assert_eq!(resident_of(&json!({"memory_stats": {}})), None, "an ended container has none");
        let odd = json!({"memory_stats": {"usage": 5, "stats": {"inactive_file": 9}}});
        assert_eq!(resident_of(&odd), Some(0));
    }

    #[test]
    fn names_that_go_into_containers_and_labels_are_plain() {
        assert!(plain("dal1"));
        assert!(plain("mac-mini-2"));
        for name in ["", "Dal1", "a b", "a/b", "a=b", &"a".repeat(65)] {
            assert!(!plain(name), "{name:?}");
        }
    }
}
