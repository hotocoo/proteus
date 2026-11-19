//! A small `adb` client: just enough of the protocol to run a command in the guest, feed it input
//! and read what it prints.
//!
//! It talks to the guest's `adbd` through the loopback port the NAT forwards to it. The protocol is
//! the one in AOSP's `packages/modules/adb/protocol.txt`: 24-byte message headers, a `CNXN`
//! handshake, then one stream opened with `OPEN "exec:<command>"`. `exec:` streams are raw in both
//! directions, so binary input and output pass unchanged.
//!
//! shortcut: no `AUTH`, so it only reaches an `adbd` that does not ask for a key, as on the
//! debuggable images run today; signing with the project key comes with the project's own image.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use crate::config::{ADB_CHUNK, ADB_CONNECT_TIMEOUT, ADB_IDLE_TIMEOUT, ADB_WRITE_TIMEOUT};
use crate::copy;

const A_CNXN: u32 = u32::from_le_bytes(*b"CNXN");
const A_OPEN: u32 = u32::from_le_bytes(*b"OPEN");
const A_OKAY: u32 = u32::from_le_bytes(*b"OKAY");
const A_WRTE: u32 = u32::from_le_bytes(*b"WRTE");
const A_CLSE: u32 = u32::from_le_bytes(*b"CLSE");
const A_AUTH: u32 = u32::from_le_bytes(*b"AUTH");
/// The protocol version that lets the data checksum be left out; it is still sent, for older
/// devices that check it.
const A_VERSION: u32 = 0x0100_0001;
const HEADER_LEN: usize = 24;
/// Who this end of the connection is, in the form `adbd` expects.
const BANNER: &[u8] = b"host::\0";
/// Our end of the one stream each connection opens.
const LOCAL_ID: u32 = 1;
/// The most the guest may send in one message; `adbd` itself sends far less.
const MAX_PAYLOAD: u32 = 1 << 20;

struct Message {
    command: u32,
    arg0: u32,
    arg1: u32,
    data: Vec<u8>,
}

fn write_message<W: Write>(
    to: &mut W,
    command: u32,
    arg0: u32,
    arg1: u32,
    data: &[u8],
) -> io::Result<()> {
    let check = data
        .iter()
        .fold(0u32, |sum, byte| sum.wrapping_add(u32::from(*byte)));
    let length = u32::try_from(data.len()).map_err(|_| io::Error::other(copy::ADB_TOO_LARGE))?;
    let mut header = [0u8; HEADER_LEN];
    for (slot, word) in header
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip([command, arg0, arg1, length, check, !command])
    {
        slot.copy_from_slice(&word.to_le_bytes());
    }
    to.write_all(&header)?;
    to.write_all(data)?;
    to.flush()
}

fn read_message<R: Read>(from: &mut R) -> io::Result<Message> {
    let mut header = [0u8; HEADER_LEN];
    from.read_exact(&mut header)?;
    let word = |index: usize| {
        let bytes = &header[index * 4..index * 4 + 4];
        u32::from_le_bytes(bytes.try_into().unwrap_or_default())
    };
    let (command, length) = (word(0), word(3));
    if word(5) != !command || length > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            copy::ADB_GARBLED,
        ));
    }
    let mut data = vec![0; length as usize];
    from.read_exact(&mut data)?;
    Ok(Message {
        command,
        arg0: word(1),
        arg1: word(2),
        data,
    })
}

/// Input for a command: exactly `length` bytes read from `reader`.
pub struct Input<'a> {
    pub reader: &'a mut dyn Read,
    pub length: u64,
}

/// Runs `command` in the guest over `stream`, which is connected to `adbd`. Sends it `input`, if
/// any, and writes what it prints to `output`. Returns when the guest closes the stream, which it
/// does when the command has ended.
pub fn exec_on<S: Read + Write>(
    stream: &mut S,
    command: &str,
    input: Option<Input<'_>>,
    output: &mut dyn Write,
) -> io::Result<()> {
    let refused = |why: &str| io::Error::new(io::ErrorKind::ConnectionRefused, why.to_owned());
    write_message(stream, A_CNXN, A_VERSION, MAX_PAYLOAD, BANNER)?;
    let max_data = loop {
        let message = read_message(stream)?;
        match message.command {
            A_CNXN => break message.arg1.clamp(1, MAX_PAYLOAD),
            A_AUTH => return Err(refused(copy::ADB_WANTS_KEY)),
            // Anything before the handshake ends is of no interest.
            _ => {}
        }
    };
    let mut service = format!("exec:{command}").into_bytes();
    service.push(0);
    write_message(stream, A_OPEN, LOCAL_ID, 0, &service)?;
    let remote = loop {
        let message = read_message(stream)?;
        match message.command {
            A_OKAY if message.arg1 == LOCAL_ID => break message.arg0,
            A_CLSE => return Err(refused(copy::ADB_COMMAND_REFUSED)),
            _ => {}
        }
    };
    // The guest takes one write at a time and says OKAY when it is ready for the next. Whatever it
    // prints meanwhile is taken and acknowledged.
    let mut pending = input.map(|input| input.reader.take(input.length));
    let mut chunk = vec![0; (max_data as usize).min(ADB_CHUNK)];
    let mut ready = true;
    loop {
        if ready && let Some(reader) = pending.as_mut() {
            let read = reader.read(&mut chunk)?;
            if read == 0 {
                // The source ended before the length it was declared to have.
                if reader.limit() > 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        copy::ADB_INPUT_SHORT,
                    ));
                }
                pending = None;
            } else {
                write_message(stream, A_WRTE, LOCAL_ID, remote, &chunk[..read])?;
                ready = false;
            }
        }
        let message = read_message(stream)?;
        match message.command {
            A_WRTE => {
                output.write_all(&message.data)?;
                write_message(stream, A_OKAY, LOCAL_ID, remote, &[])?;
            }
            A_OKAY => ready = true,
            A_CLSE => {
                // Our half closes too, as the protocol asks.
                let _ = write_message(stream, A_CLSE, LOCAL_ID, remote, &[]);
                return Ok(());
            }
            _ => {}
        }
    }
}

/// Runs `command` in the guest through the `adb` forward on loopback `port`.
pub fn exec(
    port: u16,
    command: &str,
    input: Option<Input<'_>>,
    output: &mut dyn Write,
) -> io::Result<()> {
    exec_within(port, command, input, output, ADB_IDLE_TIMEOUT)
}

/// As `exec`, giving up if the guest is silent for `idle`.
fn exec_within(
    port: u16,
    command: &str,
    input: Option<Input<'_>>,
    output: &mut dyn Write,
    idle: Duration,
) -> io::Result<()> {
    if port == 0 {
        return Err(io::Error::new(io::ErrorKind::NotConnected, copy::ADB_NONE));
    }
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&address, ADB_CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(idle))?;
    stream.set_write_timeout(Some(ADB_WRITE_TIMEOUT))?;
    stream.set_nodelay(true)?;
    let ran = exec_on(&mut stream, command, input, output);
    // The forward accepts at once and only then reaches the guest, so a guest whose adbd is not up
    // yet shows as a connection closed before the handshake.
    ran.map_err(|err| match err.kind() {
        // Not one of ours, which carry their own message.
        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
            if err.get_ref().is_none() =>
        {
            io::Error::new(err.kind(), copy::ADB_NOT_ANSWERING)
        }
        _ => err,
    })
}

/// Runs `command` in the guest and returns what it printed, as text.
pub fn output(port: u16, command: &str) -> io::Result<String> {
    output_within(port, command, ADB_IDLE_TIMEOUT)
}

/// As `output`, giving up if the guest is silent for `idle`.
pub fn output_within(port: u16, command: &str, idle: Duration) -> io::Result<String> {
    let mut printed = Vec::new();
    exec_within(port, command, None, &mut printed, idle)?;
    Ok(String::from_utf8_lossy(&printed).into_owned())
}

/// Puts `text` in single quotes for the guest's shell, so that it is one word whatever is in it.
pub fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;
    use std::thread;

    use super::*;

    /// A pretend `adbd` that accepts one `exec:` stream, takes `expect_input` bytes of input in
    /// small writes, and answers with `reply` in two writes.
    fn device(
        mut stream: UnixStream,
        expect_input: usize,
        reply: &'static [u8],
    ) -> (String, Vec<u8>) {
        const REMOTE: u32 = 77;
        const DEVICE_MAX: u32 = 4;
        let hello = read_message(&mut stream).unwrap();
        assert_eq!((hello.command, hello.data.as_slice()), (A_CNXN, BANNER));
        write_message(&mut stream, A_CNXN, A_VERSION, DEVICE_MAX, b"device::\0").unwrap();
        let open = read_message(&mut stream).unwrap();
        assert_eq!(open.command, A_OPEN);
        let service = String::from_utf8(open.data).unwrap();
        write_message(&mut stream, A_OKAY, REMOTE, open.arg0, &[]).unwrap();
        let mut input = Vec::new();
        while input.len() < expect_input {
            let message = read_message(&mut stream).unwrap();
            assert_eq!((message.command, message.arg1), (A_WRTE, REMOTE));
            assert!(message.data.len() <= DEVICE_MAX as usize);
            input.extend(message.data);
            write_message(&mut stream, A_OKAY, REMOTE, open.arg0, &[]).unwrap();
        }
        let (first, second) = reply.split_at(reply.len() / 2);
        for part in [first, second] {
            write_message(&mut stream, A_WRTE, REMOTE, open.arg0, part).unwrap();
            assert_eq!(read_message(&mut stream).unwrap().command, A_OKAY);
        }
        write_message(&mut stream, A_CLSE, REMOTE, open.arg0, &[]).unwrap();
        assert_eq!(read_message(&mut stream).unwrap().command, A_CLSE);
        (service, input)
    }

    #[test]
    fn a_command_gets_its_input_in_pieces_the_guest_can_take_and_its_output_back() {
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        let guest = thread::spawn(move || device(theirs, 10, b"Success\n"));
        let mut input: &[u8] = b"0123456789 and nothing more";
        let mut printed = Vec::new();
        let fed = Input {
            reader: &mut input,
            length: 10,
        };
        exec_on(
            &mut ours,
            "cmd package install -S 10",
            Some(fed),
            &mut printed,
        )
        .unwrap();
        let (service, received) = guest.join().unwrap();
        assert_eq!(service, "exec:cmd package install -S 10\0");
        assert_eq!(received, b"0123456789");
        assert_eq!(printed, b"Success\n");
    }

    #[test]
    fn a_guest_that_wants_a_key_or_refuses_the_command_says_why() {
        let (mut ours, mut theirs) = UnixStream::pair().unwrap();
        let guest = thread::spawn(move || {
            read_message(&mut theirs).unwrap();
            write_message(&mut theirs, A_AUTH, 1, 0, &[0; 20]).unwrap();
        });
        let err = exec_on(&mut ours, "true", None, &mut Vec::new()).unwrap_err();
        assert_eq!(err.to_string(), copy::ADB_WANTS_KEY);
        guest.join().unwrap();

        let (mut ours, mut theirs) = UnixStream::pair().unwrap();
        let guest = thread::spawn(move || {
            read_message(&mut theirs).unwrap();
            write_message(&mut theirs, A_CNXN, A_VERSION, MAX_PAYLOAD, b"device::\0").unwrap();
            let open = read_message(&mut theirs).unwrap();
            write_message(&mut theirs, A_CLSE, 0, open.arg0, &[]).unwrap();
        });
        let err = exec_on(&mut ours, "nope", None, &mut Vec::new()).unwrap_err();
        assert_eq!(err.to_string(), copy::ADB_COMMAND_REFUSED);
        guest.join().unwrap();
    }

    #[test]
    fn garbled_headers_are_refused() {
        let mut bytes = Vec::new();
        write_message(&mut bytes, A_OKAY, 1, 2, b"x").unwrap();
        assert!(read_message(&mut bytes.as_slice()).is_ok());
        bytes[20] ^= 1;
        assert!(read_message(&mut bytes.as_slice()).is_err());
    }

    #[test]
    fn quoting_keeps_a_path_one_word() {
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }
}
