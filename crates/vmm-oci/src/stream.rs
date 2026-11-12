//! A container's attached streams as the guest's channel. With no terminal, the engine sends standard output and
//! standard error on one connection, each piece behind 8 bytes: the stream (1 output, 2 error), three zeros, and the
//! length (big-endian). Output is the guest's protocol: it goes to the channel's reader. Error is the console: it
//! is appended to the console file. What the host writes goes to the container's standard input as it is.

use cf_vmm::{Channel, SEND_TIMEOUT};
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;

/// The largest piece the engine sends: it cuts what the container writes into pieces of its buffer's size.
const PIECE_MAX: usize = 1 << 20;

/// The channel to a container's first process.
pub struct AttachChannel {
    /// The engine's connection: the container's input is written here.
    attached: UnixStream,
    /// The near end of a pair whose far end the reading thread writes the container's output to.
    output: UnixStream,
}

/// Starts reading the attached connection, `early` being what came with the attach's answer, and returns the
/// channel. The console takes what the container says on its standard error.
pub fn split(attached: UnixStream, early: Vec<u8>, console: File) -> io::Result<AttachChannel> {
    attached.set_write_timeout(Some(SEND_TIMEOUT))?;
    let (output, far) = UnixStream::pair()?;
    let reader = attached.try_clone()?;
    std::thread::spawn(move || {
        let mut source = io::Cursor::new(early).chain(reader);
        let _ = demultiplex(&mut source, far, console);
    });
    Ok(AttachChannel { attached, output })
}

/// Copies each piece to where its stream goes until the connection ends, the guest's reader goes, or a piece is
/// no piece; then ends the guest's output, which wakes its reader.
pub fn demultiplex(source: &mut impl Read, mut output: UnixStream, mut console: File) -> io::Result<()> {
    let mut piece = Vec::new();
    let result = loop {
        let mut header = [0u8; 8];
        if let Err(error) = source.read_exact(&mut header) {
            break if error.kind() == io::ErrorKind::UnexpectedEof {
                Ok(())
            } else {
                Err(error)
            };
        }
        let length = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
        if header[1..4] != [0, 0, 0] || header[0] > 2 || length > PIECE_MAX {
            break Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the engine's stream is not multiplexed",
            ));
        }
        piece.resize(length, 0);
        if let Err(error) = source.read_exact(&mut piece) {
            break Err(error);
        }
        match header[0] {
            1 => {
                if let Err(error) = output.write_all(&piece) {
                    break Err(error);
                }
            }
            // A full or failing console loses lines, not the job.
            2 => drop(console.write_all(&piece)),
            _ => {}
        }
    };
    let _ = output.shutdown(Shutdown::Both);
    result
}

impl Read for AttachChannel {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.output.read(buffer)
    }
}

impl Write for AttachChannel {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.attached.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.attached.flush()
    }
}

impl Channel for AttachChannel {
    fn try_clone(&self) -> io::Result<Box<dyn Channel>> {
        Ok(Box::new(AttachChannel {
            attached: self.attached.try_clone()?,
            output: self.output.try_clone()?,
        }))
    }

    fn shutdown(&self) {
        // The container's input ends, so its supervisor hears the host is gone; a blocked reader wakes.
        let _ = self.attached.shutdown(Shutdown::Both);
        let _ = self.output.shutdown(Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(stream: u8, data: &[u8]) -> Vec<u8> {
        let mut out = vec![stream, 0, 0, 0];
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(data);
        out
    }

    fn run(bytes: Vec<u8>) -> (io::Result<()>, Vec<u8>, String) {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("console.log");
        let console = File::create(&path).unwrap();
        let (mut near, far) = UnixStream::pair().unwrap();
        let result = demultiplex(&mut io::Cursor::new(bytes), far, console);
        let mut output = Vec::new();
        near.read_to_end(&mut output).unwrap();
        (result, output, std::fs::read_to_string(&path).unwrap())
    }

    #[test]
    fn output_goes_to_the_channel_and_errors_to_the_console_in_order() {
        let mut bytes = frame(1, b"\x00\x00\x00\x02{}");
        bytes.extend(frame(2, b"cf-supervisor: kernel 6.12\n"));
        bytes.extend(frame(1, b""));
        bytes.extend(frame(1, b"more"));
        bytes.extend(frame(0, b"stdin echo"));
        bytes.extend(frame(2, b"workload line\n"));
        let (result, output, console) = run(bytes);
        assert!(result.is_ok());
        assert_eq!(output, b"\x00\x00\x00\x02{}more");
        assert_eq!(console, "cf-supervisor: kernel 6.12\nworkload line\n");
    }

    #[test]
    fn a_stream_that_is_not_multiplexed_ends_the_channel_and_says_so() {
        for bytes in [
            b"plain text from a terminal".to_vec(),
            [vec![3, 0, 0, 0, 0, 0, 0, 1], b"x".to_vec()].concat(),
            vec![1, 0, 0, 0, 0xff, 0xff, 0xff, 0xff],
            [frame(1, b"ok"), vec![1, 0, 0, 0, 0, 0, 0, 9, b'c', b'u']].concat(),
        ] {
            let (result, output, _) = run(bytes.clone());
            assert!(result.is_err(), "{bytes:?}");
            assert!(
                output.len() <= 2,
                "nothing past the bad piece reaches the guest's reader"
            );
        }
        let (result, output, console) = run(Vec::new());
        assert!(result.is_ok(), "an empty stream is the container's end");
        assert!(output.is_empty() && console.is_empty());
    }

    #[test]
    fn a_shutdown_wakes_a_reader_blocked_on_the_channel() {
        let (attached, engine) = UnixStream::pair().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let console = File::create(folder.path().join("c")).unwrap();
        let mut channel = split(attached, frame(1, b"hi"), console).unwrap();
        let mut first = [0u8; 2];
        channel.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"hi");
        let mut reader = channel.try_clone().unwrap();
        let blocked = std::thread::spawn(move || {
            let mut rest = Vec::new();
            reader.read_to_end(&mut rest).map(|_| rest)
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        channel.shutdown();
        assert_eq!(blocked.join().unwrap().unwrap(), b"");
        // And the engine sees the container's input end.
        let mut engine = engine;
        let mut nothing = Vec::new();
        engine.read_to_end(&mut nothing).unwrap();
        assert!(nothing.is_empty());
    }
}
