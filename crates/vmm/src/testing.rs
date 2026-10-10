//! A monitor whose machines are scripted, for tests of everything above [`Vmm`](crate::Vmm).
//!
//! A [`FakeVmm`] is given one [`Script`] per machine it will be asked to start, in order. A script's guest is a
//! closure that gets the guest's end of the channel and plays the supervisor on a thread of its own.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{Channel, ExitCell, Vm, VmExit, VmSpec, Vmm, VmmError};

/// What the guest of one scripted machine does with its end of the channel. When it returns, the machine has
/// powered itself off.
pub type Guest = Box<dyn FnOnce(UnixStream, PowerSwitch) + Send>;

/// How one scripted machine behaves.
pub enum Script {
    /// [`Vmm::start`] fails with this error.
    FailToStart(VmmError),
    /// The machine starts; `guest` runs when the host opens the channel.
    Boot {
        /// How many attempts to open the channel fail first, as while a guest is still booting.
        refuse_channel: usize,
        /// The guest.
        guest: Guest,
        /// Whether [`Vm::stop`] ends the machine (`false`: a machine that hangs in its stop).
        obeys_stop: bool,
    },
}

impl Script {
    /// A machine whose guest is `guest`, reachable at the first attempt, that obeys a stop.
    pub fn boot(guest: impl FnOnce(UnixStream, PowerSwitch) + Send + 'static) -> Self {
        Script::Boot {
            refuse_channel: 0,
            guest: Box::new(guest),
            obeys_stop: true,
        }
    }
}

/// Lets a scripted guest end its machine in other ways than returning.
#[derive(Clone)]
pub struct PowerSwitch(Arc<ExitCell>);

impl PowerSwitch {
    /// The monitor loses the machine, as when its process dies.
    pub fn crash(&self, reason: &str) {
        self.0.end(VmExit::Error(reason.into()));
    }

    /// Blocks until the host has ended the machine (stop, kill or drop).
    pub fn wait_for_the_end(&self) {
        self.0.wait(Duration::MAX);
    }
}

/// A monitor that starts scripted machines and remembers what it was asked for.
#[derive(Default)]
pub struct FakeVmm {
    scripts: Mutex<VecDeque<Script>>,
    started: Mutex<Vec<VmSpec>>,
    stops: Arc<Mutex<usize>>,
}

impl FakeVmm {
    /// A monitor that will start these machines, in this order.
    pub fn new(scripts: impl IntoIterator<Item = Script>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into_iter().collect()),
            ..Self::default()
        }
    }

    /// The specifications of every start asked for so far, failed ones included.
    pub fn started(&self) -> Vec<VmSpec> {
        self.started.lock().unwrap().clone()
    }

    /// How often a machine of this monitor was told to stop.
    pub fn stops(&self) -> usize {
        *self.stops.lock().unwrap()
    }
}

impl Vmm for FakeVmm {
    fn start(&self, spec: &VmSpec) -> Result<Box<dyn Vm>, VmmError> {
        spec.validate()?;
        self.started.lock().unwrap().push(spec.clone());
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| VmmError::Start("the test scripted no further machine".into()))?;
        match script {
            Script::FailToStart(error) => Err(error),
            Script::Boot {
                refuse_channel,
                guest,
                obeys_stop,
            } => Ok(Box::new(FakeVm {
                power: Arc::default(),
                guest: Mutex::new(Some(guest)),
                refuse_channel: Mutex::new(refuse_channel),
                obeys_stop,
                stops: self.stops.clone(),
            })),
        }
    }
}

struct FakeVm {
    power: Arc<ExitCell>,
    guest: Mutex<Option<Guest>>,
    refuse_channel: Mutex<usize>,
    obeys_stop: bool,
    stops: Arc<Mutex<usize>>,
}

impl Vm for FakeVm {
    fn channel(&self, port: u32) -> Result<Box<dyn Channel>, VmmError> {
        let refuse = |reason: &str| VmmError::Connect {
            port,
            reason: reason.into(),
        };
        if self.power.wait(Duration::ZERO).is_some() {
            return Err(refuse("the machine has ended"));
        }
        let mut refusals = self.refuse_channel.lock().unwrap();
        if *refusals > 0 {
            *refusals -= 1;
            return Err(refuse("nothing listens yet"));
        }
        let guest = self
            .guest
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| refuse("the guest accepts one connection"))?;
        let (host, guest_end) = UnixStream::pair().map_err(|error| refuse(&error.to_string()))?;
        let power = self.power.clone();
        std::thread::spawn(move || {
            guest(guest_end, PowerSwitch(power.clone()));
            power.end(VmExit::Guest);
        });
        Ok(Box::new(Pipe(host)))
    }

    fn wait(&self, timeout: Duration) -> Option<VmExit> {
        self.power.wait(timeout)
    }

    fn stop(&self) -> Result<(), VmmError> {
        *self.stops.lock().unwrap() += 1;
        if self.obeys_stop {
            self.power.end(VmExit::Stopped);
        }
        Ok(())
    }

    fn kill(&self) {
        self.power.end(VmExit::Stopped);
    }
}

impl Drop for FakeVm {
    fn drop(&mut self) {
        self.power.end(VmExit::Stopped);
    }
}

struct Pipe(UnixStream);

impl Read for Pipe {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buffer)
    }
}

impl Write for Pipe {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl Channel for Pipe {
    fn try_clone(&self) -> std::io::Result<Box<dyn Channel>> {
        Ok(Box::new(Pipe(self.0.try_clone()?)))
    }

    fn shutdown(&self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}
