use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{AnyThread, MainThreadMarker};
use objc2_foundation::{NSArray, NSDate, NSError, NSFileHandle, NSRunLoop, NSString, NSURL};
use objc2_virtualization::{
    VZEntropyDeviceConfiguration, VZFileHandleSerialPortAttachment, VZLinuxBootLoader,
    VZSerialPortConfiguration, VZVirtioConsoleDeviceSerialPortConfiguration,
    VZVirtioEntropyDeviceConfiguration, VZVirtualMachine, VZVirtualMachineConfiguration,
    VZVirtualMachineState,
};

/// The line the supervisor prints when the guest is up.
const READY_LINE: &str = "cf-supervisor: ready";

/// What to boot.
#[derive(Debug, Clone)]
pub struct BootSpec {
    /// The uncompressed arm64 kernel `Image`.
    pub kernel: PathBuf,
    /// The initramfs.
    pub initramfs: PathBuf,
    /// The guest's memory in bytes.
    pub memory_bytes: u64,
    /// The guest's CPU count.
    pub cpus: usize,
    /// How long the guest may take to say ready, and then to power off.
    pub timeout: Duration,
}

/// What the proof saw.
#[derive(Debug, Clone)]
pub struct BootReport {
    /// Everything the guest wrote to its console.
    pub console: String,
    /// From the start request to the ready line.
    pub ready_after: Duration,
    /// From the poweroff request to the machine stopped.
    pub stopped_after: Duration,
    /// Whether the guest powered itself off (`false`: the host had to stop it).
    pub clean_poweroff: bool,
}

/// Why the proof failed.
#[derive(Debug, thiserror::Error)]
pub enum BootError {
    /// Not on the main thread.
    #[error("the boot must run on the main thread")]
    NotMainThread,
    /// Virtualization.framework is not usable (no entitlement or no hardware support).
    #[error("virtualization is not supported here")]
    Unsupported,
    /// The configuration was refused (often a missing entitlement).
    #[error("the configuration is invalid: {0}")]
    InvalidConfiguration(String),
    /// The framework could not start the machine.
    #[error("the machine did not start: {0}")]
    StartFailed(String),
    /// No ready line within the timeout.
    #[error("the guest was not ready after {0:?}; console so far:\n{1}")]
    NotReady(Duration, String),
    /// The machine stopped before it said ready.
    #[error("the guest stopped before it was ready; console:\n{0}")]
    StoppedEarly(String),
    /// A pipe could not be made.
    #[error("system call failed: {0}")]
    System(String),
}

/// Boots the guest, waits for `cf-supervisor: ready` on its console, asks it to power off and waits for it to stop.
/// The guest has one virtio console, no network and no disk. Must run on the main thread.
pub fn boot_to_ready(spec: &BootSpec) -> Result<BootReport, BootError> {
    MainThreadMarker::new().ok_or(BootError::NotMainThread)?;
    // SAFETY: plain class method with no arguments.
    if !unsafe { VZVirtualMachine::isSupported() } {
        return Err(BootError::Unsupported);
    }

    let (to_guest_read, to_guest_write) = pipe()?;
    let (from_guest_read, from_guest_write) = pipe()?;
    let console: Console = Arc::default();
    spawn_reader(from_guest_read, console.clone());

    let reading = file_handle(to_guest_read);
    let writing = file_handle(from_guest_write);
    let mut host_input = std::fs::File::from(to_guest_write);

    // SAFETY: all objects below are created and used on the main thread, and every argument is a valid object.
    let machine = unsafe {
        let boot = VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &file_url(&spec.kernel));
        boot.setInitialRamdiskURL(Some(&file_url(&spec.initramfs)));
        boot.setCommandLine(&NSString::from_str("console=hvc0 quiet panic=-1"));

        let attachment = VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
            VZFileHandleSerialPortAttachment::alloc(),
            Some(&reading),
            Some(&writing),
        );
        let port = VZVirtioConsoleDeviceSerialPortConfiguration::new();
        port.setAttachment(Some(&attachment));
        let ports: Retained<NSArray<VZSerialPortConfiguration>> =
            NSArray::from_retained_slice(&[Retained::into_super(port)]);
        let entropy: Retained<NSArray<VZEntropyDeviceConfiguration>> =
            NSArray::from_retained_slice(&[Retained::into_super(VZVirtioEntropyDeviceConfiguration::new())]);

        let configuration = VZVirtualMachineConfiguration::new();
        configuration.setBootLoader(Some(&*Retained::into_super(boot)));
        configuration.setCPUCount(spec.cpus);
        configuration.setMemorySize(spec.memory_bytes);
        configuration.setSerialPorts(&ports);
        configuration.setEntropyDevices(&entropy);
        configuration
            .validateWithError()
            .map_err(|error| BootError::InvalidConfiguration(describe(&error)))?;
        VZVirtualMachine::initWithConfiguration(VZVirtualMachine::alloc(), &configuration)
    };

    let started = Instant::now();
    let start_result: Arc<Mutex<Option<Result<(), String>>>> = Arc::default();
    let slot = start_result.clone();
    let handler = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: the framework passes either null or a valid NSError for the duration of the call.
        let outcome = match unsafe { error.as_ref() } {
            Some(error) => Err(describe(error)),
            None => Ok(()),
        };
        *slot.lock().unwrap() = Some(outcome);
    });
    // SAFETY: called on the main thread (the machine's queue) with a valid block.
    unsafe { machine.startWithCompletionHandler(&handler) };

    let deadline = started + spec.timeout;
    let ready_after = loop {
        spin();
        if let Some(Err(message)) = start_result.lock().unwrap().clone() {
            return Err(BootError::StartFailed(message));
        }
        if text(&console).contains(READY_LINE) {
            break started.elapsed();
        }
        // SAFETY: reading the state on the main queue.
        if unsafe { machine.state() } == VZVirtualMachineState::Stopped {
            // The reader thread may still hold the last lines, which say why.
            spin_for(Duration::from_millis(100));
            return Err(BootError::StoppedEarly(text(&console)));
        }
        if Instant::now() >= deadline {
            // SAFETY: stopping on the main queue; the result is not needed.
            unsafe { machine.stopWithCompletionHandler(&RcBlock::new(|_: *mut NSError| {})) };
            spin_for(Duration::from_millis(500));
            return Err(BootError::NotReady(spec.timeout, text(&console)));
        }
    };

    use std::io::Write as _;
    let stop_requested = Instant::now();
    let _ = host_input.write_all(b"poweroff\n");
    let stop_deadline = stop_requested + spec.timeout;
    let mut clean_poweroff = true;
    // SAFETY: reading the state on the main queue.
    while unsafe { machine.state() } != VZVirtualMachineState::Stopped {
        spin();
        if Instant::now() >= stop_deadline {
            clean_poweroff = false;
            // SAFETY: stopping on the main queue.
            unsafe { machine.stopWithCompletionHandler(&RcBlock::new(|_: *mut NSError| {})) };
            spin_for(Duration::from_secs(1));
            break;
        }
    }
    let stopped_after = stop_requested.elapsed();
    // The reader thread may still hold the last bytes for a moment.
    spin_for(Duration::from_millis(100));
    Ok(BootReport {
        console: text(&console),
        ready_after,
        stopped_after,
        clean_poweroff,
    })
}

fn spin() {
    spin_for(Duration::from_millis(20));
}

fn spin_for(duration: Duration) {
    let until = NSDate::dateWithTimeIntervalSinceNow(duration.as_secs_f64());
    // Runs the current (main) thread's run loop, which is what the framework's queue needs.
    NSRunLoop::currentRunLoop().runUntilDate(&until);
}

fn file_url(path: &std::path::Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
}

fn file_handle(fd: OwnedFd) -> Retained<NSFileHandle> {
    use std::os::fd::IntoRawFd as _;
    // The descriptor is open and now owned by the handle, which closes it when released.
    NSFileHandle::initWithFileDescriptor_closeOnDealloc(NSFileHandle::alloc(), fd.into_raw_fd(), true)
}

fn describe(error: &NSError) -> String {
    format!("{} (code {})", error.localizedDescription(), error.code())
}

fn pipe() -> Result<(OwnedFd, OwnedFd), BootError> {
    let mut fds: [RawFd; 2] = [0; 2];
    // SAFETY: fds is a two element array, as pipe(2) needs.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(BootError::System(std::io::Error::last_os_error().to_string()));
    }
    // SAFETY: pipe(2) returned two open descriptors that nothing else owns.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Everything the guest wrote, as bytes: a multi-byte character may arrive in two reads.
type Console = Arc<Mutex<Vec<u8>>>;

fn text(console: &Console) -> String {
    String::from_utf8_lossy(&console.lock().unwrap()).into_owned()
}

fn spawn_reader(fd: OwnedFd, console: Console) {
    std::thread::spawn(move || {
        use std::io::Read as _;
        let mut file = std::fs::File::from(fd);
        let mut buffer = [0u8; 4096];
        while let Ok(read) = file.read(&mut buffer) {
            if read == 0 {
                break;
            }
            console.lock().unwrap().extend_from_slice(&buffer[..read]);
        }
    });
}
