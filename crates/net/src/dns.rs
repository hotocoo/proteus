//! The small part of DNS the built-in responder needs: read one question, write one answer.
//! Queries come from the guest and are parsed defensively.

use std::net::Ipv4Addr;

const HEADER_LEN: usize = 12;
const MAX_LABEL_LEN: usize = 63;
/// Longest name on the wire, length bytes and terminator included.
const MAX_WIRE_NAME_LEN: usize = 255;
/// The two top bits of a length byte mark a compression pointer, which a question never needs.
const LABEL_POINTER_BITS: u8 = 0xc0;

// Header flag bits.
const FLAG_RESPONSE: u16 = 1 << 15;
const OPCODE_MASK: u16 = 0xf << 11;
const FLAG_RECURSION_DESIRED: u16 = 1 << 8;
const FLAG_RECURSION_AVAILABLE: u16 = 1 << 7;
const RCODE_SERVER_FAILURE: u16 = 2;
const RCODE_NAME_ERROR: u16 = 3;

pub const TYPE_A: u16 = 1;
pub const CLASS_IN: u16 = 1;
/// A pointer to offset 12, where the question's name starts.
const POINTER_TO_QUESTION_NAME: [u8; 2] = [0xc0, HEADER_LEN as u8];

#[derive(Debug, PartialEq, Eq)]
pub struct Query {
    id: u16,
    recursion_desired: bool,
    /// The question exactly as received, echoed back in the response.
    question: Vec<u8>,
    pub name: String,
    pub qtype: u16,
    pub qclass: u16,
}

pub enum Answer<'a> {
    Addresses(&'a [Ipv4Addr]),
    /// The name exists as far as we know, but has no record of the type asked for.
    NoData,
    NameError,
    ServerFailure,
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    let pair = bytes.get(at..at + 2)?;
    Some(u16::from_be_bytes([pair[0], pair[1]]))
}

/// Parses a standard query with exactly one question. Anything else is `None` and gets no reply.
pub fn parse_query(packet: &[u8]) -> Option<Query> {
    let flags = u16_at(packet, 2)?;
    let question_count = u16_at(packet, 4)?;
    if flags & (FLAG_RESPONSE | OPCODE_MASK) != 0 || question_count != 1 {
        return None;
    }
    let mut at = HEADER_LEN;
    let mut name = String::new();
    loop {
        let len = *packet.get(at)?;
        at += 1;
        if len == 0 {
            break;
        }
        if len & LABEL_POINTER_BITS != 0 || usize::from(len) > MAX_LABEL_LEN {
            return None;
        }
        let label = packet.get(at..at + usize::from(len))?;
        at += usize::from(len);
        if at - HEADER_LEN >= MAX_WIRE_NAME_LEN
            || !label.iter().all(u8::is_ascii_graphic)
            || label.contains(&b'.')
        {
            return None;
        }
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(std::str::from_utf8(label).ok()?);
    }
    let qtype = u16_at(packet, at)?;
    let qclass = u16_at(packet, at + 2)?;
    Some(Query {
        id: u16_at(packet, 0)?,
        recursion_desired: flags & FLAG_RECURSION_DESIRED != 0,
        question: packet[HEADER_LEN..at + 4].to_vec(),
        name,
        qtype,
        qclass,
    })
}

pub fn build_response(query: &Query, answer: Answer<'_>, ttl_secs: u32) -> Vec<u8> {
    let (rcode, addresses) = match answer {
        Answer::Addresses(addresses) => (0, addresses),
        Answer::NoData => (0, &[][..]),
        Answer::NameError => (RCODE_NAME_ERROR, &[][..]),
        Answer::ServerFailure => (RCODE_SERVER_FAILURE, &[][..]),
    };
    let desired = if query.recursion_desired {
        FLAG_RECURSION_DESIRED
    } else {
        0
    };
    let flags = FLAG_RESPONSE | desired | FLAG_RECURSION_AVAILABLE | rcode;
    let answer_count = u16::try_from(addresses.len()).unwrap_or(u16::MAX);

    let mut out = Vec::with_capacity(HEADER_LEN + query.question.len());
    out.extend(query.id.to_be_bytes());
    out.extend(flags.to_be_bytes());
    out.extend(1u16.to_be_bytes());
    out.extend(answer_count.to_be_bytes());
    out.extend([0; 4]); // no authority or additional records
    out.extend(&query.question);
    for address in addresses.iter().take(usize::from(answer_count)) {
        out.extend(POINTER_TO_QUESTION_NAME);
        out.extend(TYPE_A.to_be_bytes());
        out.extend(CLASS_IN.to_be_bytes());
        out.extend(ttl_secs.to_be_bytes());
        out.extend((address.octets().len() as u16).to_be_bytes());
        out.extend(address.octets());
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const TYPE_AAAA: u16 = 28;

    pub(crate) fn query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
        let mut packet = Vec::new();
        packet.extend(id.to_be_bytes());
        packet.extend(FLAG_RECURSION_DESIRED.to_be_bytes());
        packet.extend([0, 1, 0, 0, 0, 0, 0, 0]);
        for label in name.split('.') {
            packet.push(label.len() as u8);
            packet.extend(label.as_bytes());
        }
        packet.push(0);
        packet.extend(qtype.to_be_bytes());
        packet.extend(CLASS_IN.to_be_bytes());
        packet
    }

    #[test]
    fn question_is_parsed() {
        let parsed = parse_query(&query(0x1234, "Play.Example.com", TYPE_A)).unwrap();
        assert_eq!(
            (parsed.name.as_str(), parsed.qtype, parsed.qclass),
            ("Play.Example.com", TYPE_A, CLASS_IN)
        );
    }

    #[test]
    fn trailing_additional_records_are_tolerated() {
        let mut packet = query(1, "edns.example", TYPE_AAAA);
        packet.extend([0, 0, 41, 0x10, 0, 0, 0, 0, 0, 0, 0]); // an OPT record
        assert_eq!(parse_query(&packet).unwrap().qtype, TYPE_AAAA);
    }

    #[test]
    fn response_echoes_the_question_and_carries_the_addresses() {
        let packet = query(0xbeef, "a.example", TYPE_A);
        let parsed = parse_query(&packet).unwrap();
        let addresses = [Ipv4Addr::new(192, 0, 2, 1), Ipv4Addr::new(192, 0, 2, 2)];
        let response = build_response(&parsed, Answer::Addresses(&addresses), 60);

        assert_eq!(&response[..2], &[0xbe, 0xef]);
        let flags = u16_at(&response, 2).unwrap();
        assert_eq!(
            flags,
            FLAG_RESPONSE | FLAG_RECURSION_DESIRED | FLAG_RECURSION_AVAILABLE
        );
        assert_eq!(&response[4..12], &[0, 1, 0, 2, 0, 0, 0, 0]);
        assert_eq!(&response[HEADER_LEN..packet.len()], &packet[HEADER_LEN..]);
        let first = &response[packet.len()..packet.len() + 16];
        assert_eq!(
            first,
            &[0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 192, 0, 2, 1]
        );
        assert_eq!(response.len(), packet.len() + 32);
    }

    #[test]
    fn failure_answers_set_the_response_code_and_carry_no_records() {
        let parsed = parse_query(&query(7, "gone.example", TYPE_A)).unwrap();
        for (answer, rcode) in [
            (Answer::NameError, 3),
            (Answer::ServerFailure, 2),
            (Answer::NoData, 0),
        ] {
            let response = build_response(&parsed, answer, 60);
            assert_eq!(u16_at(&response, 2).unwrap() & 0xf, rcode);
            assert_eq!(u16_at(&response, 6), Some(0));
        }
    }

    #[test]
    fn malformed_and_unsupported_packets_get_no_reply() {
        let good = query(1, "ok.example", TYPE_A);
        assert!(parse_query(&good).is_some());
        for cut in 0..good.len() {
            assert_eq!(parse_query(&good[..cut]), None, "cut at {cut}");
        }
        let mutate = |at: usize, value: u8| {
            let mut bad = good.clone();
            bad[at] = value;
            bad
        };
        assert_eq!(parse_query(&mutate(2, 0x80)), None, "a response");
        assert_eq!(parse_query(&mutate(2, 0x28)), None, "not a standard query");
        assert_eq!(parse_query(&mutate(5, 2)), None, "two questions");
        assert_eq!(
            parse_query(&mutate(HEADER_LEN, 0xc0)),
            None,
            "compression pointer"
        );
        assert_eq!(parse_query(&mutate(HEADER_LEN, 64)), None, "label too long");
        assert_eq!(
            parse_query(&mutate(HEADER_LEN + 1, b' ')),
            None,
            "space in a label"
        );
        assert_eq!(
            parse_query(&mutate(HEADER_LEN + 1, b'.')),
            None,
            "dot inside a label"
        );
        let long = vec!["a".repeat(63); 4].join(".");
        assert_eq!(
            parse_query(&query(1, &long, TYPE_A)),
            None,
            "name longer than 255 bytes"
        );
    }
}
