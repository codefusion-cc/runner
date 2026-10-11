//! A line a person types that the terminal does not show: the console's one-time code. A pipe is read as it is.

use std::io::{self, Write};
use std::sync::atomic::{AtomicI32, Ordering};

/// The longest line taken. A one-time code is far shorter.
const LONGEST: usize = 4096;

/// The signals that end a program at a terminal. While the terminal shows nothing they are caught, so that it is
/// put back as it was before the program ends by them.
const ENDING: [libc::c_int; 4] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];

/// The signal that came while a line was read, or zero.
static CAME: AtomicI32 = AtomicI32::new(0);

extern "C" fn came(signal: libc::c_int) {
    CAME.store(signal, Ordering::Relaxed);
}

/// Says so when an ending signal came since the signals were caught.
fn came_signal() -> Result<(), NoLine> {
    match CAME.load(Ordering::Relaxed) {
        0 => Ok(()),
        signal => Err(NoLine::Signal(signal)),
    }
}

/// Why no line was read.
#[derive(Debug)]
pub enum NoLine {
    /// The descriptor or the terminal refused.
    Io(io::Error),
    /// The line is longer than any code, or is no text.
    NoCode,
    /// This signal asked the program to end. The terminal is as it was; [`end_by`] ends the program by it.
    Signal(libc::c_int),
}

/// The terminal with its echo off for as long as this lives.
struct Quiet {
    fd: libc::c_int,
    before: libc::termios,
}

impl Quiet {
    fn new(fd: libc::c_int) -> Result<Quiet, NoLine> {
        // SAFETY: zeroes are a termios, and `tcgetattr` fills the one it is given.
        let mut before: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `before` is a termios this call may write.
        if unsafe { libc::tcgetattr(fd, &mut before) } != 0 {
            return Err(NoLine::Io(io::Error::last_os_error()));
        }
        let mut quiet = before;
        quiet.c_lflag &= !libc::ECHO;
        // The Enter still moves to the next line, so what follows does not stand behind the question.
        quiet.c_lflag |= libc::ECHONL;
        // SAFETY: `quiet` is a whole termios. What was typed before the question is thrown away with the change:
        // it was typed while the terminal still showed it.
        while unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &quiet) } != 0 {
            // The change waits until the question is written out, and a signal ends that wait: an ending one ends
            // the question, any other is no reason to stop.
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(NoLine::Io(error));
            }
            came_signal()?;
        }
        Ok(Quiet { fd, before })
    }
}

impl Drop for Quiet {
    fn drop(&mut self) {
        // SAFETY: `before` is the termios this terminal had.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.before);
        }
    }
}

/// The ending signals caught for as long as this lives, without restarting the read they interrupt. One the
/// program was started to ignore (`nohup`, a shell's `trap '' INT`) stays ignored.
struct Caught([libc::sigaction; ENDING.len()]);

impl Caught {
    fn new() -> Caught {
        CAME.store(0, Ordering::Relaxed);
        // SAFETY: zeroes are a sigaction.
        let mut before: [libc::sigaction; ENDING.len()] = unsafe { std::mem::zeroed() };
        for (signal, before) in ENDING.into_iter().zip(&mut before) {
            // SAFETY: zeroes are a sigaction: no flags, so no SA_RESTART, and an empty mask.
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = came as extern "C" fn(libc::c_int) as libc::sighandler_t;
            // SAFETY: the handler only stores into an atomic, which a signal handler may do, and lives as long as
            // the program; `before` is a sigaction this call may write, and is what this signal had when it is
            // given back.
            unsafe {
                libc::sigaction(signal, &action, before);
                if before.sa_sigaction == libc::SIG_IGN {
                    libc::sigaction(signal, before, std::ptr::null_mut());
                }
            }
        }
        Caught(before)
    }
}

impl Drop for Caught {
    fn drop(&mut self) {
        for (signal, before) in ENDING.into_iter().zip(&self.0) {
            // SAFETY: `before` is what this signal had.
            unsafe {
                libc::sigaction(signal, before, std::ptr::null_mut());
            }
        }
    }
}

/// Reads one line from `fd`, without its end. At a terminal `question` goes to `ask` first and what is typed is not
/// shown; from anything else the line is read and nothing is asked. No more than the line is taken from `fd`. The
/// end of the input ends the line too.
pub fn read_line(fd: libc::c_int, question: &str, mut ask: impl Write) -> Result<String, NoLine> {
    // SAFETY: `isatty` only looks at the descriptor.
    let terminal = unsafe { libc::isatty(fd) } == 1;
    // In this order: the terminal is put back before the signals are let go.
    let (_caught, _quiet) = if terminal {
        let caught = Caught::new();
        let _ = ask.write_all(question.as_bytes()).and_then(|()| ask.flush());
        (Some(caught), Some(Quiet::new(fd)?))
    } else {
        (None, None)
    };
    // A signal ends a read only when it comes during one. One that came between two reads, or before the first,
    // is asked for here: none is lost, and no line is given after one.
    let signal = || if terminal { came_signal() } else { Ok(()) };
    let mut line = Vec::new();
    loop {
        signal()?;
        let mut byte = 0u8;
        // SAFETY: `byte` is one byte this call may write.
        match unsafe { libc::read(fd, (&raw mut byte).cast(), 1) } {
            1 if byte == b'\n' => break,
            1 if line.len() == LONGEST => return Err(NoLine::NoCode),
            1 => line.push(byte),
            0 => break,
            _ => {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(NoLine::Io(error));
                }
            }
        }
    }
    signal()?;
    String::from_utf8(line).map_err(|_| NoLine::NoCode)
}

/// Ends the program by `signal`, as if it had never been caught.
pub fn end_by(signal: libc::c_int) -> ! {
    // SAFETY: the default action of a signal, and the signal itself, sent to this program.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
    std::process::exit(128 + signal)
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::{Read as _, Write as _};
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::os::unix::thread::JoinHandleExt as _;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::{NoLine, read_line};

    /// The signals' handlers and the signal that came are the program's, so one test at a time asks at a terminal.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    /// A terminal: what a person types goes into the first, the program reads the second.
    fn terminal() -> (File, OwnedFd) {
        let (mut person, mut program) = (0, 0);
        // SAFETY: both numbers may be written; no name, settings or size is asked for.
        let made = unsafe {
            libc::openpty(
                &mut person,
                &mut program,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(made, 0, "a terminal");
        // SAFETY: both were just opened and belong to nobody else.
        unsafe { (File::from_raw_fd(person), OwnedFd::from_raw_fd(program)) }
    }

    fn echoes(terminal: &OwnedFd) -> bool {
        // SAFETY: zeroes are a termios, and `tcgetattr` fills it.
        let mut now: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `now` may be written.
        assert_eq!(unsafe { libc::tcgetattr(terminal.as_raw_fd(), &mut now) }, 0);
        now.c_lflag & libc::ECHO != 0
    }

    /// Waits until the terminal shows nothing any more, or a few seconds: what was typed before that is thrown
    /// away, as a real terminal's is.
    fn until_quiet(terminal: &OwnedFd) {
        let start = Instant::now();
        while echoes(terminal) && start.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// What the terminal has shown so far.
    fn shown(person: &mut File) -> String {
        // SAFETY: the flags of a descriptor this test owns.
        unsafe {
            libc::fcntl(person.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
        }
        let mut shown = Vec::new();
        let mut part = [0u8; 256];
        while let Ok(n @ 1..) = person.read(&mut part) {
            shown.extend_from_slice(&part[..n]);
        }
        String::from_utf8_lossy(&shown).into_owned()
    }

    #[derive(Clone, Default)]
    struct Asked(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Asked {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_code_typed_at_a_terminal_is_asked_for_and_never_shown() {
        let _one = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (mut person, program) = terminal();
        assert!(echoes(&program), "a new terminal shows what is typed");
        let asked = Asked::default();
        let reader = {
            let (fd, asked) = (program.as_raw_fd(), asked.clone());
            std::thread::spawn(move || read_line(fd, "The code: ", asked))
        };
        until_quiet(&program);
        person
            .write_all(b"s3cret-one-time-code\nwhat is typed next\n")
            .unwrap();
        let line = reader.join().unwrap().expect("a line");
        assert_eq!(line, "s3cret-one-time-code");
        assert_eq!(asked.0.lock().unwrap().as_slice(), b"The code: ");
        assert!(echoes(&program), "the terminal shows what is typed again");
        let shown = shown(&mut person);
        assert!(!shown.contains("s3cret"), "the terminal showed {shown:?}");
        // The Enter is shown, so the next line does not stand behind the question.
        assert!(shown.starts_with("\r\n") || shown.starts_with('\n'), "{shown:?}");
    }

    #[test]
    fn a_signal_at_the_question_gives_the_terminal_back_as_it_was() {
        let _one = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (mut person, program) = terminal();
        let reader = {
            let fd = program.as_raw_fd();
            std::thread::spawn(move || read_line(fd, "", std::io::sink()))
        };
        until_quiet(&program);
        let was_quiet = !echoes(&program);
        // SAFETY: the thread runs: it is joined only below.
        unsafe {
            libc::pthread_kill(reader.as_pthread_t(), libc::SIGHUP);
        }
        let ended = answer(reader, &mut person);
        assert!(matches!(ended, Err(NoLine::Signal(libc::SIGHUP))), "{ended:?}");
        assert!(was_quiet, "the terminal showed what was typed at the question");
        assert!(echoes(&program), "the terminal shows what is typed again");
    }

    /// Asks by sending its own thread a signal: one that comes while nothing is read.
    struct AskedWithASignal;

    impl std::io::Write for AskedWithASignal {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            // SAFETY: a signal to the thread that runs this.
            unsafe {
                libc::pthread_kill(libc::pthread_self(), libc::SIGTERM);
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The reader's answer. A read nothing ended is ended by a line after a few seconds, so that a test fails and
    /// does not wait for ever.
    fn answer(
        reader: std::thread::JoinHandle<Result<String, NoLine>>,
        person: &mut File,
    ) -> Result<String, NoLine> {
        let start = Instant::now();
        while !reader.is_finished() && start.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(5));
        }
        if !reader.is_finished() {
            person.write_all(b"typed-after-the-signal\n").unwrap();
        }
        reader.join().unwrap()
    }

    #[test]
    fn a_signal_that_comes_while_nothing_is_read_is_not_lost() {
        let _one = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (mut person, program) = terminal();
        let reader = {
            let fd = program.as_raw_fd();
            std::thread::spawn(move || read_line(fd, "The code: ", AskedWithASignal))
        };
        let ended = answer(reader, &mut person);
        assert!(matches!(ended, Err(NoLine::Signal(libc::SIGTERM))), "{ended:?}");
        assert!(echoes(&program), "the terminal shows what is typed again");
        // Whether the line was waited for: nothing had to be typed for the question to end.
        assert_eq!(shown(&mut person), "");
    }

    /// What a signal does in this program now.
    fn disposition(signal: libc::c_int) -> libc::sighandler_t {
        // SAFETY: zeroes are a sigaction, and with no new one given `sigaction` only fills it.
        let mut now: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: `now` may be written.
        assert_eq!(unsafe { libc::sigaction(signal, std::ptr::null(), &mut now) }, 0);
        now.sa_sigaction
    }

    #[test]
    fn a_signal_the_program_was_started_to_ignore_stays_ignored() {
        let _one = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // SAFETY: the signal is ignored in this program, as under `nohup`, and gets back what it had below.
        let before = unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
        let (mut person, program) = terminal();
        let reader = {
            let fd = program.as_raw_fd();
            std::thread::spawn(move || read_line(fd, "", std::io::sink()))
        };
        until_quiet(&program);
        // SAFETY: the thread runs: it is joined only below.
        unsafe {
            libc::pthread_kill(reader.as_pthread_t(), libc::SIGHUP);
        }
        person.write_all(b"code-under-nohup\n").unwrap();
        let ended = reader.join().unwrap();
        let after = disposition(libc::SIGHUP);
        // SAFETY: what the signal had before this test.
        unsafe {
            libc::signal(libc::SIGHUP, before);
        }
        assert_eq!(ended.expect("a line"), "code-under-nohup");
        assert_eq!(
            after,
            libc::SIG_IGN,
            "the signal is still ignored after the question"
        );
    }

    fn pipe(holds: &[u8]) -> OwnedFd {
        let mut ends = [0; 2];
        // SAFETY: two numbers this call may write.
        assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
        // SAFETY: both were just opened and belong to nobody else.
        let (read, mut write) = unsafe { (OwnedFd::from_raw_fd(ends[0]), File::from_raw_fd(ends[1])) };
        write.write_all(holds).unwrap();
        read
    }

    #[test]
    fn a_pipe_is_asked_nothing_and_gives_its_first_line_and_no_more() {
        let asked = Asked::default();
        let input = pipe(b"code-1\nand more\n");
        assert_eq!(
            read_line(input.as_raw_fd(), "The code: ", asked.clone()).unwrap(),
            "code-1"
        );
        assert_eq!(asked.0.lock().unwrap().as_slice(), b"");
        // The rest is still there for whoever reads next.
        assert_eq!(
            read_line(input.as_raw_fd(), "", asked.clone()).unwrap(),
            "and more"
        );
        // No line end before the end, nothing at all, and a line end alone.
        assert_eq!(
            read_line(pipe(b"code-2").as_raw_fd(), "", asked.clone()).unwrap(),
            "code-2"
        );
        assert_eq!(read_line(pipe(b"").as_raw_fd(), "", asked.clone()).unwrap(), "");
        assert_eq!(read_line(pipe(b"\n").as_raw_fd(), "", asked.clone()).unwrap(), "");
    }

    #[test]
    fn a_line_no_code_can_be_is_refused() {
        let longest = "c".repeat(super::LONGEST);
        let fits = read_line(
            pipe(format!("{longest}\n").as_bytes()).as_raw_fd(),
            "",
            std::io::sink(),
        );
        assert_eq!(fits.unwrap(), longest);
        let too_long = read_line(
            pipe(format!("{longest}c\n").as_bytes()).as_raw_fd(),
            "",
            std::io::sink(),
        );
        assert!(matches!(too_long, Err(NoLine::NoCode)), "{too_long:?}");
        let no_text = read_line(pipe(b"\xff\xfe\n").as_raw_fd(), "", std::io::sink());
        assert!(matches!(no_text, Err(NoLine::NoCode)), "{no_text:?}");
    }
}
