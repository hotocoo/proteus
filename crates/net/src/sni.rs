//! Finds the server name in a TLS ClientHello, the one place a TLS connection names its destination
//! in the clear. Input is guest-controlled; every length is checked and nothing here can panic.

/// TLS record header: content type, legacy version (2 bytes), payload length (2 bytes).
const RECORD_HEADER_LEN: usize = 5;
const CONTENT_TYPE_HANDSHAKE: u8 = 0x16;
/// A record payload may not exceed 2^14 bytes, plus the allowance for a protected record.
const MAX_RECORD_PAYLOAD: usize = (1 << 14) + 256;
/// Most bytes worth buffering before giving up on finding a name.
pub const MAX_CLIENT_HELLO: usize = RECORD_HEADER_LEN + MAX_RECORD_PAYLOAD;
const HANDSHAKE_CLIENT_HELLO: u8 = 0x01;
/// ClientHello fields before the session ID: legacy version (2) and random (32).
const VERSION_AND_RANDOM_LEN: usize = 34;
const EXTENSION_SERVER_NAME: u16 = 0;
const NAME_TYPE_HOST: u8 = 0;
/// Longest DNS name in text form.
pub(crate) const MAX_NAME_LEN: usize = 253;

#[derive(Debug, PartialEq, Eq)]
pub enum Sni<'a> {
    /// The stream does not open with a TLS ClientHello.
    NotClientHello,
    /// It may be a ClientHello, but the record has not fully arrived.
    Incomplete,
    /// A ClientHello that names no server.
    Absent,
    Name(&'a str),
}

/// Reads fields off the front of a slice. Every read returns `None` rather than run past the end.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(len)?;
        self.0 = tail;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|bytes| bytes[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.take(2)
            .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u24(&mut self) -> Option<usize> {
        self.take(3).map(|bytes| {
            usize::from(bytes[0]) << 16 | usize::from(bytes[1]) << 8 | usize::from(bytes[2])
        })
    }

    /// A field preceded by its one-byte length.
    fn vec8(&mut self) -> Option<&'a [u8]> {
        let len = self.u8()?;
        self.take(usize::from(len))
    }

    /// A field preceded by its two-byte length.
    fn vec16(&mut self) -> Option<&'a [u8]> {
        let len = self.u16()?;
        self.take(usize::from(len))
    }
}

/// The host name in a `server_name` extension body, if it holds a usable one.
fn host_name(extension: &[u8]) -> Option<&str> {
    let mut list = Reader(Reader(extension).vec16()?);
    while !list.0.is_empty() {
        let name_type = list.u8()?;
        let name = list.vec16()?;
        if name_type == NAME_TYPE_HOST {
            let name = std::str::from_utf8(name).ok()?;
            let plain = !name.is_empty()
                && name.len() <= MAX_NAME_LEN
                && name.bytes().all(|b| b.is_ascii_graphic());
            return plain.then_some(name);
        }
    }
    None
}

/// The server name in a ClientHello handshake message body.
fn server_name(hello: &[u8]) -> Option<&str> {
    let mut hello = Reader(hello);
    hello.take(VERSION_AND_RANDOM_LEN)?;
    hello.vec8()?; // session ID
    hello.vec16()?; // cipher suites
    hello.vec8()?; // compression methods
    let mut extensions = Reader(hello.vec16()?);
    while !extensions.0.is_empty() {
        let kind = extensions.u16()?;
        let body = extensions.vec16()?;
        if kind == EXTENSION_SERVER_NAME {
            return host_name(body);
        }
    }
    None
}

/// Classifies the first bytes a client sent on a TCP connection.
pub fn client_hello_sni(stream: &[u8]) -> Sni<'_> {
    match stream.first() {
        None => return Sni::Incomplete,
        Some(&CONTENT_TYPE_HANDSHAKE) => {}
        Some(_) => return Sni::NotClientHello,
    }
    let Some((header, rest)) = stream.split_at_checked(RECORD_HEADER_LEN) else {
        return Sni::Incomplete;
    };
    let payload_len = usize::from(u16::from_be_bytes([header[3], header[4]]));
    if payload_len > MAX_RECORD_PAYLOAD {
        return Sni::NotClientHello;
    }
    let Some(payload) = rest.get(..payload_len) else {
        return Sni::Incomplete;
    };
    let mut record = Reader(payload);
    if record.u8() != Some(HANDSHAKE_CLIENT_HELLO) {
        return Sni::NotClientHello;
    }
    // A ClientHello larger than one record is not reassembled; what the first record holds is parsed.
    let hello = record.u24().map(|len| &record.0[..len.min(record.0.len())]);
    hello.and_then(server_name).map_or(Sni::Absent, Sni::Name)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn len16(body: &[u8]) -> Vec<u8> {
        [&(body.len() as u16).to_be_bytes()[..], body].concat()
    }

    fn extension(kind: u16, body: &[u8]) -> Vec<u8> {
        [&kind.to_be_bytes()[..], &len16(body)].concat()
    }

    fn sni_extension(name: &[u8]) -> Vec<u8> {
        let entry = [&[NAME_TYPE_HOST][..], &len16(name)].concat();
        extension(EXTENSION_SERVER_NAME, &len16(&entry))
    }

    /// A syntactically complete ClientHello record carrying the given extensions.
    pub(crate) fn client_hello(extensions: &[u8]) -> Vec<u8> {
        let mut hello = vec![0x03, 0x03];
        hello.extend([0xaa; 32]); // random
        hello.extend([4, 1, 2, 3, 4]); // session ID
        hello.extend(len16(&[0x13, 0x01, 0x13, 0x02])); // cipher suites
        hello.extend([1, 0]); // compression
        hello.extend(len16(extensions));
        let mut handshake = vec![HANDSHAKE_CLIENT_HELLO];
        handshake.extend(&(hello.len() as u32).to_be_bytes()[1..]);
        handshake.extend(hello);
        let mut record = vec![CONTENT_TYPE_HANDSHAKE, 0x03, 0x01];
        record.extend(len16(&handshake));
        record
    }

    pub(crate) fn client_hello_for(name: &str) -> Vec<u8> {
        // Other extensions before and after, as real clients send them in varying order.
        let extensions = [
            extension(0x002b, &[2, 3, 4]),
            sni_extension(name.as_bytes()),
            extension(0x000a, &[0, 2, 0, 0x1d]),
        ];
        client_hello(&extensions.concat())
    }

    #[test]
    fn name_is_found_among_other_extensions() {
        assert_eq!(
            client_hello_sni(&client_hello_for("game.example.com")),
            Sni::Name("game.example.com")
        );
    }

    #[test]
    fn trailing_bytes_after_the_record_do_not_matter() {
        let mut stream = client_hello_for("a.example");
        stream.extend([0x17, 3, 3, 0, 1, 0]);
        assert_eq!(client_hello_sni(&stream), Sni::Name("a.example"));
    }

    #[test]
    fn every_prefix_of_a_hello_is_incomplete_never_a_wrong_answer() {
        let stream = client_hello_for("prefix.example");
        for cut in 0..stream.len() {
            assert_eq!(
                client_hello_sni(&stream[..cut]),
                Sni::Incomplete,
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn non_tls_streams_are_recognised_from_the_first_byte() {
        assert_eq!(client_hello_sni(b"GET / HTTP/1.1\r\n"), Sni::NotClientHello);
        assert_eq!(client_hello_sni(&[0x17, 3, 3, 0, 0]), Sni::NotClientHello);
        // A handshake record that is not a ClientHello.
        assert_eq!(
            client_hello_sni(&[CONTENT_TYPE_HANDSHAKE, 3, 3, 0, 4, 2, 0, 0, 0]),
            Sni::NotClientHello
        );
        // A record length no TLS implementation sends.
        assert_eq!(
            client_hello_sni(&[CONTENT_TYPE_HANDSHAKE, 3, 3, 0xff, 0xff]),
            Sni::NotClientHello
        );
    }

    #[test]
    fn hello_without_a_usable_name_is_absent() {
        assert_eq!(client_hello_sni(&client_hello(&[])), Sni::Absent);
        assert_eq!(
            client_hello_sni(&client_hello(&extension(0x002b, &[2, 3, 4]))),
            Sni::Absent
        );
        for bad in [
            &b""[..],
            b"has space.example",
            b"nul\0.example",
            &[0xff, 0xfe],
            &[b'a'; MAX_NAME_LEN + 1],
        ] {
            assert_eq!(
                client_hello_sni(&client_hello(&sni_extension(bad))),
                Sni::Absent,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn lengths_that_overrun_their_container_are_absent_not_a_panic() {
        let good = client_hello_for("overrun.example");
        // Corrupt each byte in turn to every extreme value; the parser must always return.
        for at in RECORD_HEADER_LEN..good.len() {
            for value in [0x00, 0x7f, 0xff] {
                let mut bad = good.clone();
                bad[at] = value;
                let _ = client_hello_sni(&bad);
            }
        }
        // Extensions length larger than what follows.
        let mut truncated = client_hello(&sni_extension(b"x.example"));
        let len = truncated.len();
        truncated[len - 20] = 0xff;
        assert!(matches!(
            client_hello_sni(&truncated),
            Sni::Absent | Sni::Name(_) | Sni::NotClientHello
        ));
    }
}
