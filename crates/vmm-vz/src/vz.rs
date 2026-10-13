use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use block2::RcBlock;
use cf_vmm::{Channel, ExitCell, Network, Vm, VmExit, VmSpec, Vmm, VmmError};

use crate::holder::Holder;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::{NSArray, NSError, NSString, NSURL};
use objc2_virtualization::{
    VZDiskImageCachingMode, VZDiskImageStorageDeviceAttachment, VZDiskImageSynchronizationMode,
    VZEntropyDeviceConfiguration, VZFileSerialPortAttachment, VZLinuxBootLoader, VZMACAddress,
    VZNATNetworkDeviceAttachment, VZNetworkDeviceConfiguration, VZSerialPortConfiguration,
    VZSocketDeviceConfiguration, VZStorageDeviceConfiguration, VZVirtioBlockDeviceConfiguration,
    VZVirtioConsoleDeviceSerialPortConfiguration, VZVirtioEntropyDeviceConfiguration,
    VZVirtioNetworkDeviceConfiguration, VZVirtioSocketConnection, VZVirtioSocketDevice,
    VZVirtioSocketDeviceConfiguration, VZVirtualMachine, VZVirtualMachineConfiguration,
    VZVirtualMachineDelegate, VZVirtualMachineState,
};

/// How long the framework gets to answer a start or a connect before the call gives up.
const START_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The monitor on Virtualization.framework.
#[derive(Debug, Clone, Copy)]
pub struct VzVmm(());

impl VzVmm {
    /// The monitor, when this Mac can run virtual machines at all.
    pub fn new() -> Result<Self, VmmError> {
        // SAFETY: a class method with no arguments, callable from any thread.
        if unsafe { VZVirtualMachine::isSupported() } {
            Ok(Self(()))
        } else {
            Err(VmmError::Unsupported(
                "Virtualization.framework says this Mac cannot".into(),
            ))
        }
    }
}

impl Vmm for VzVmm {
    fn start(&self, spec: &VmSpec) -> Result<Box<dyn Vm>, VmmError> {
        spec.validate()?;
        let queue = DispatchQueue::new("cc.codefusion.runner.vm", None);
        let exit = Arc::new(ExitCell::default());

        let mut built = None;
        queue.exec_sync(|| built = Some(build(spec, &queue, exit.clone()).map(OnQueue)));
        let OnQueue((machine, delegate)) = built.expect("the queue ran the block")?;
        // The files only this machine opens; a read-only image may be open in every machine at once.
        let mut own: Vec<&Path> = vec![&spec.kernel, &spec.console];
        own.extend(
            spec.disks
                .iter()
                .filter(|disk| !disk.read_only)
                .map(|disk| disk.path.as_path()),
        );
        let vm = VzVm {
            queue,
            machine: Mutex::new(Some(Arc::new(OnQueue(machine)))),
            _delegate: OnQueue(delegate),
            exit,
            process: Holder::machine(&own),
        };

        let (started, result) = mpsc::channel();
        vm.on_queue(move |machine| {
            let handler = RcBlock::new(move |error: *mut NSError| {
                // SAFETY: the framework passes null or an error that is valid for the call.
                let _ = started.send(unsafe { error.as_ref() }.map(describe));
            });
            // SAFETY: on the machine's queue, with a block that is copied before the call returns.
            unsafe { machine.startWithCompletionHandler(&handler) };
        });
        match result.recv_timeout(START_TIMEOUT) {
            Ok(None) => Ok(Box::new(vm)),
            Ok(Some(message)) => Err(VmmError::Start(message)),
            Err(_) => Err(VmmError::Start(format!("no answer within {START_TIMEOUT:?}"))),
        }
    }
}

/// Something that is only ever used on its machine's queue, which is what makes it safe to hand to that queue
/// from another thread.
struct OnQueue<T>(T);
// SAFETY: the value is created and used only inside blocks that run on the machine's serial queue; other threads
// hold it without ever looking inside. The machine is dropped on the queue too ([`VzVm::release`]). The delegate
// is dropped with its `VzVm`, on whichever thread that is: all this does is release an Objective-C object, which
// any thread may, and its one instance variable is an `Arc` of state that is `Send` and `Sync`.
unsafe impl<T> Send for OnQueue<T> {}
// SAFETY: as above; a shared reference gives no access outside the queue either.
unsafe impl<T> Sync for OnQueue<T> {}

type Machine = Arc<OnQueue<Retained<VZVirtualMachine>>>;

struct VzVm {
    queue: DispatchRetained<DispatchQueue>,
    /// `None` once the machine was let go of.
    machine: Mutex<Option<Machine>>,
    /// The machine holds its delegate weakly.
    _delegate: OnQueue<Retained<Delegate>>,
    exit: Arc<ExitCell>,
    /// The framework's process that runs the machine.
    process: Holder,
}

impl VzVm {
    /// Runs `work` with the machine on its queue, later. Nothing runs when the machine was let go of.
    fn on_queue(&self, work: impl FnOnce(&VZVirtualMachine) + Send + 'static) {
        if let Some(machine) = self.machine.lock().unwrap().clone() {
            self.queue.exec_async(move || work(&machine.0));
        }
    }

    /// Lets go of the machine; the framework ends it when the last reference goes, which happens on the queue.
    fn release(&self) {
        if let Some(machine) = self.machine.lock().unwrap().take() {
            self.queue.exec_async(move || drop(machine));
        }
    }
}

impl Vm for VzVm {
    fn channel(&self, port: u32) -> Result<Box<dyn Channel>, VmmError> {
        let refuse = |reason: String| VmmError::Connect { port, reason };
        let (connected, result) = mpsc::channel::<Result<OwnedFd, String>>();
        let failed = connected.clone();
        self.on_queue(move |machine| {
            // SAFETY: on the machine's queue.
            let device = unsafe { machine.socketDevices() }
                .firstObject()
                .and_then(|device| device.downcast::<VZVirtioSocketDevice>().ok());
            let Some(device) = device else {
                let _ = failed.send(Err("the machine has no socket device".into()));
                return;
            };
            let handler = RcBlock::new(
                move |connection: *mut VZVirtioSocketConnection, error: *mut NSError| {
                    // SAFETY: the framework passes a connection or an error, each valid for the call or null.
                    let outcome = match unsafe { connection.as_ref() } {
                        // The connection closes its descriptor when it is released, so this end keeps a copy.
                        // SAFETY: the descriptor is open for the call; dup gives one nothing else owns.
                        Some(connection) => match unsafe { libc::dup(connection.fileDescriptor()) } {
                            -1 => Err(std::io::Error::last_os_error().to_string()),
                            // SAFETY: dup returned a new descriptor that is ours alone.
                            fd => Ok(unsafe { OwnedFd::from_raw_fd(fd) }),
                        },
                        // SAFETY: as above.
                        None => {
                            Err(unsafe { error.as_ref() }.map_or_else(|| "no reason given".into(), describe))
                        }
                    };
                    let _ = connected.send(outcome);
                },
            );
            // SAFETY: on the machine's queue, with a block that is copied before the call returns.
            unsafe { device.connectToPort_completionHandler(port, &handler) };
        });
        match result.recv_timeout(CONNECT_TIMEOUT) {
            Ok(Ok(fd)) => {
                patient_for(&fd, cf_vmm::SEND_TIMEOUT);
                Ok(Box::new(Stream(File::from(fd))))
            }
            Ok(Err(reason)) => Err(refuse(reason)),
            Err(RecvTimeoutError::Timeout) => Err(refuse(format!("no answer within {CONNECT_TIMEOUT:?}"))),
            Err(RecvTimeoutError::Disconnected) => Err(refuse("the machine is gone".into())),
        }
    }

    fn wait(&self, timeout: Duration) -> Option<VmExit> {
        if let Some(exit) = self.exit.wait(timeout) {
            return Some(exit);
        }
        // The delegate is the way an end is heard of; asking the machine as well means a missed call cannot leave
        // a stopped machine looking alive for ever.
        let (asked, state) = mpsc::channel();
        self.on_queue(move |machine| {
            // SAFETY: on the machine's queue.
            let _ = asked.send(unsafe { machine.state() });
        });
        match state.recv_timeout(CONNECT_TIMEOUT) {
            // A stop the host asked for may be seen here before its own answer comes.
            Ok(VZVirtualMachineState::Stopped) => self.exit.found_stopped(),
            Ok(VZVirtualMachineState::Error) => self
                .exit
                .end(VmExit::Error("the machine is in its error state".into())),
            _ => {}
        }
        self.exit.wait(Duration::ZERO)
    }

    fn stop(&self) -> Result<(), VmmError> {
        self.exit.stop_asked();
        let exit = self.exit.clone();
        let (answer, taken) = mpsc::channel();
        self.on_queue(move |machine| {
            // SAFETY: on the machine's queue.
            let can = unsafe { machine.canStop() };
            let _ = answer.send(can);
            if !can {
                return;
            }
            let handler = RcBlock::new(move |error: *mut NSError| {
                // SAFETY: the framework passes null or an error that is valid for the call.
                match unsafe { error.as_ref() } {
                    None => exit.end(VmExit::Stopped),
                    Some(error) => exit.end(VmExit::Error(format!("the stop failed: {}", describe(error)))),
                }
            });
            // SAFETY: on the machine's queue, with a block that is copied before the call returns.
            unsafe { machine.stopWithCompletionHandler(&handler) };
        });
        match taken.recv_timeout(CONNECT_TIMEOUT) {
            Ok(true) => Ok(()),
            // It has ended, which `wait` finds out, or is in a state the framework takes no stop in.
            Ok(false) => Err(VmmError::Stop("the machine takes no stop now".into())),
            Err(_) => Err(VmmError::Stop(format!("no answer within {CONNECT_TIMEOUT:?}"))),
        }
    }

    fn kill(&self) {
        self.release();
        self.exit.end(VmExit::Stopped);
    }

    fn resident(&self) -> Option<u64> {
        self.process.resident()
    }
}

impl Drop for VzVm {
    fn drop(&mut self) {
        self.release();
    }
}

struct DelegateIvars {
    exit: Arc<ExitCell>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and this class has no `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "CFRunnerMachineDelegate"]
    #[ivars = DelegateIvars]
    struct Delegate;

    // SAFETY: the protocol asks nothing beyond NSObject's own methods.
    unsafe impl NSObjectProtocol for Delegate {}

    // SAFETY: the two methods have the protocol's signatures.
    unsafe impl VZVirtualMachineDelegate for Delegate {
        #[unsafe(method(guestDidStopVirtualMachine:))]
        fn guest_did_stop(&self, _machine: &VZVirtualMachine) {
            self.ivars().exit.end(VmExit::Guest);
        }

        #[unsafe(method(virtualMachine:didStopWithError:))]
        fn did_stop_with_error(&self, _machine: &VZVirtualMachine, error: &NSError) {
            self.ivars().exit.end(VmExit::Error(describe(error)));
        }
    }
);

impl Delegate {
    fn new(exit: Arc<ExitCell>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars { exit });
        // SAFETY: NSObject's `init` on a freshly allocated object whose instance variables are set.
        unsafe { msg_send![super(this), init] }
    }
}

/// Makes the machine. Runs on its queue.
fn build(
    spec: &VmSpec,
    queue: &DispatchQueue,
    exit: Arc<ExitCell>,
) -> Result<(Retained<VZVirtualMachine>, Retained<Delegate>), VmmError> {
    let refuse = |what: &str, error: &NSError| VmmError::Spec(format!("{what}: {}", describe(error)));
    // SAFETY: every object below is made and used here, on the machine's queue, and every argument is a valid
    // object of the class the method takes.
    unsafe {
        let boot = VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &file_url(&spec.kernel));
        boot.setInitialRamdiskURL(Some(&file_url(&spec.initramfs)));
        boot.setCommandLine(&NSString::from_str(cf_vmm::COMMAND_LINE));

        let console = VZFileSerialPortAttachment::initWithURL_append_error(
            VZFileSerialPortAttachment::alloc(),
            &file_url(&spec.console),
            true,
        )
        .map_err(|error| refuse("the console file", &error))?;
        let port = VZVirtioConsoleDeviceSerialPortConfiguration::new();
        port.setAttachment(Some(&console));
        let ports: Retained<NSArray<VZSerialPortConfiguration>> =
            NSArray::from_retained_slice(&[Retained::into_super(port)]);

        let entropy: Retained<NSArray<VZEntropyDeviceConfiguration>> =
            NSArray::from_retained_slice(&[Retained::into_super(VZVirtioEntropyDeviceConfiguration::new())]);

        let mut disks: Vec<Retained<VZStorageDeviceConfiguration>> = Vec::new();
        for disk in &spec.disks {
            // The host's cache holds an image's pages once for every machine that reads it. Nothing on a disk of a
            // machine outlives the machine, so no write is ever forced to the Mac's disk.
            let attachment = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_cachingMode_synchronizationMode_error(
                VZDiskImageStorageDeviceAttachment::alloc(),
                &file_url(&disk.path),
                disk.read_only,
                VZDiskImageCachingMode::Cached,
                VZDiskImageSynchronizationMode::None,
            )
            .map_err(|error| refuse(&format!("the disk {}", disk.path.display()), &error))?;
            let device = VZVirtioBlockDeviceConfiguration::initWithAttachment(
                VZVirtioBlockDeviceConfiguration::alloc(),
                &attachment,
            );
            device.setBlockDeviceIdentifier(&NSString::from_str(&disk.id));
            disks.push(Retained::into_super(device));
        }
        let disks = NSArray::from_retained_slice(&disks);

        let mut network: Vec<Retained<VZNetworkDeviceConfiguration>> = Vec::new();
        if spec.network == Network::Nat {
            let device = VZVirtioNetworkDeviceConfiguration::new();
            device.setAttachment(Some(&VZNATNetworkDeviceAttachment::new()));
            device.setMACAddress(&VZMACAddress::randomLocallyAdministeredAddress());
            network.push(Retained::into_super(device));
        }
        let network = NSArray::from_retained_slice(&network);

        let sockets: Retained<NSArray<VZSocketDeviceConfiguration>> =
            NSArray::from_retained_slice(&[Retained::into_super(VZVirtioSocketDeviceConfiguration::new())]);

        let configuration = VZVirtualMachineConfiguration::new();
        configuration.setBootLoader(Some(&boot));
        configuration.setCPUCount(spec.cpus);
        configuration.setMemorySize(spec.memory_bytes);
        configuration.setSerialPorts(&ports);
        configuration.setEntropyDevices(&entropy);
        configuration.setStorageDevices(&disks);
        configuration.setNetworkDevices(&network);
        configuration.setSocketDevices(&sockets);
        configuration
            .validateWithError()
            .map_err(|error| refuse("the configuration", &error))?;

        let machine =
            VZVirtualMachine::initWithConfiguration_queue(VZVirtualMachine::alloc(), &configuration, queue);
        let delegate = Delegate::new(exit);
        machine.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        Ok((machine, delegate))
    }
}

fn file_url(path: &Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
}

fn describe(error: &NSError) -> String {
    format!("{} (code {})", error.localizedDescription(), error.code())
}

/// Gives a write to this socket no longer than `most` to go on: a guest that reads nothing fails the write, and
/// holds no thread of the host. Best effort: a descriptor that takes no such option stays as it was.
fn patient_for(socket: &OwnedFd, most: Duration) {
    let time = libc::timeval {
        tv_sec: most.as_secs() as libc::time_t,
        tv_usec: most.subsec_micros() as libc::suseconds_t,
    };
    // SAFETY: the descriptor is open for the call, and the option's value is a `timeval` of the size given that
    // lives through it.
    unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDTIMEO,
            (&raw const time).cast(),
            size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
}

/// One end of a connection to a port of the guest.
struct Stream(File);

impl Read for Stream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buffer)
    }
}

impl Write for Stream {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Channel for Stream {
    fn try_clone(&self) -> std::io::Result<Box<dyn Channel>> {
        Ok(Box::new(Stream(self.0.try_clone()?)))
    }

    fn shutdown(&self) {
        // SAFETY: shutdown takes a descriptor this stream owns and no pointer.
        unsafe { libc::shutdown(self.0.as_raw_fd(), libc::SHUT_RDWR) };
    }
}

#[cfg(test)]
mod tests {
    use super::patient_for;
    use std::io::{ErrorKind, Write};
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn a_write_nobody_reads_fails_in_its_time_and_holds_no_thread() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let ours = OwnedFd::from(ours);
        patient_for(&ours, Duration::from_millis(200));
        let (said, heard) = mpsc::channel();
        std::thread::spawn(move || {
            let mut ours = std::fs::File::from(ours);
            // More than any socket buffers, to a peer that reads none of it.
            let outcome = ours.write_all(&vec![0u8; 16 << 20]);
            let _ = said.send(outcome.map_err(|error| error.kind()));
        });
        let outcome = heard
            .recv_timeout(Duration::from_secs(10))
            .expect("the write is still waiting");
        assert_eq!(outcome, Err(ErrorKind::WouldBlock));
        drop(theirs);
    }
}
