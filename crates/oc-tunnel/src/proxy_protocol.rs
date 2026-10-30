//! PROXY protocol headers a route can write to its target (`@opentunnel/protocol/proxy-protocol`), carrying the
//! visitor's address.

/// The PROXY protocol versions a route can ask for.
pub const VERSIONS: [&str; 2] = ["v1", "v2"];

pub fn is_version(value: &str) -> bool {
    value == "v1" || value == "v2"
}

/// The public port every visitor connected to.
pub const PUBLIC_PORT: u16 = 443;

/// The 12-byte signature that starts every v2 header.
pub const V2_SIGNATURE: [u8; 12] = [0x0d, 0x0a, 0x0d, 0x0a, 0x00, 0x0d, 0x0a, 0x51, 0x55, 0x49, 0x54, 0x0a];
const V2_PROXY: u8 = 0x21;
const V2_LOCAL: u8 = 0x20;
const V2_TCP4: u8 = 0x11;
const V2_TCP6: u8 = 0x21;
const V2_UNSPEC: u8 = 0x00;
const PP2_TYPE_AUTHORITY: u8 = 0x02;

/// A visitor address: 4 or 16 bytes and a port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub family: u8,
    pub bytes: Vec<u8>,
    pub port: u16,
}

/// `^(?:0|[1-9][0-9]{0,2})$`, at most 255.
fn parse_octet(part: &str) -> Option<u8> {
    let b = part.as_bytes();
    let ok = match b.len() {
        1 => b[0].is_ascii_digit(),
        2 | 3 => b[0] != b'0' && b.iter().all(u8::is_ascii_digit),
        _ => false,
    };
    if !ok {
        return None;
    }
    part.parse::<u16>().ok().filter(|n| *n <= 255).map(|n| n as u8)
}

fn parse_ipv4(value: &str) -> Option<[u8; 4]> {
    let parts: Vec<&str> = value.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut bytes = [0u8; 4];
    for (index, part) in parts.iter().enumerate() {
        bytes[index] = parse_octet(part)?;
    }
    Some(bytes)
}

fn parse_groups(value: &str, allow_ipv4: bool) -> Option<Vec<u16>> {
    if value.is_empty() {
        return Some(Vec::new());
    }
    let parts: Vec<&str> = value.split(':').collect();
    let mut groups = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        if allow_ipv4 && index == parts.len() - 1 && part.contains('.') {
            let ipv4 = parse_ipv4(part)?;
            groups.push(u16::from(ipv4[0]) << 8 | u16::from(ipv4[1]));
            groups.push(u16::from(ipv4[2]) << 8 | u16::from(ipv4[3]));
            continue;
        }
        if part.is_empty() || part.len() > 4 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        groups.push(u16::from_str_radix(part, 16).ok()?);
    }
    Some(groups)
}

fn parse_ipv6(value: &str) -> Option<[u8; 16]> {
    let groups = match value.find("::") {
        None => {
            let groups = parse_groups(value, true)?;
            if groups.len() != 8 {
                return None;
            }
            groups
        }
        Some(gap) => {
            if value[gap + 1..].contains("::") {
                return None;
            }
            let head = parse_groups(&value[..gap], false)?;
            let tail = parse_groups(&value[gap + 2..], true)?;
            // `::` stands for at least one zero group.
            if head.len() + tail.len() > 7 {
                return None;
            }
            let mut groups = head.clone();
            groups.resize(8 - tail.len(), 0);
            groups.extend(tail);
            groups
        }
    };
    let mut bytes = [0u8; 16];
    for (index, group) in groups.iter().enumerate() {
        bytes[index * 2] = (group >> 8) as u8;
        bytes[index * 2 + 1] = (group & 0xff) as u8;
    }
    Some(bytes)
}

/// `^[0-9]{1,5}$`, at most 65535.
fn parse_port(value: &str) -> Option<u16> {
    if value.is_empty() || value.len() > 5 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse::<u32>().ok().filter(|p| *p <= 65535).map(|p| p as u16)
}

fn address(bytes: &[u8], port: u16) -> Address {
    if bytes.len() == 16 && bytes[..10].iter().all(|b| *b == 0) && bytes[10] == 0xff && bytes[11] == 0xff {
        return Address { family: 4, bytes: bytes[12..].to_vec(), port };
    }
    Address { family: if bytes.len() == 4 { 4 } else { 6 }, bytes: bytes.to_vec(), port }
}

/// `parsePeer`: the `peer` of an `open` frame: `ipv4:port`, `[ipv6]:port`, or a bare address (port 0). IPv4-mapped
/// IPv6 addresses become IPv4.
pub fn parse_peer(peer: &str) -> Option<Address> {
    if let Some(bytes) = parse_ipv4(peer) {
        return Some(address(&bytes, 0));
    }
    if let Some(bytes) = parse_ipv6(peer) {
        return Some(address(&bytes, 0));
    }
    if peer.starts_with('[') {
        let close = peer.find("]:")?;
        let bytes = parse_ipv6(&peer[1..close]);
        let port = parse_port(&peer[close + 2..]);
        return match (bytes, port) {
            (Some(bytes), Some(port)) => Some(address(&bytes, port)),
            _ => None,
        };
    }
    let separator = peer.rfind(':')?;
    let bytes = parse_ipv4(&peer[..separator]);
    let port = parse_port(&peer[separator + 1..]);
    match (bytes, port) {
        (Some(bytes), Some(port)) => Some(address(&bytes, port)),
        _ => None,
    }
}

/// `formatAddress`: the text form PROXY v1 uses (RFC 5952 for IPv6).
pub fn format_address(value: &Address) -> String {
    if value.family == 4 {
        return value.bytes.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(".");
    }
    let groups: Vec<u16> =
        (0..8).map(|i| u16::from(value.bytes[i * 2]) << 8 | u16::from(value.bytes[i * 2 + 1])).collect();
    // Compress the first longest run of two or more zero groups.
    let (mut best_start, mut best_length) = (None, 1);
    let mut start = 0;
    while start < 8 {
        if groups[start] != 0 {
            start += 1;
            continue;
        }
        let mut end = start;
        while end < 8 && groups[end] == 0 {
            end += 1;
        }
        if end - start > best_length {
            best_start = Some(start);
            best_length = end - start;
        }
        start = end;
    }
    let hex = |list: &[u16]| list.iter().map(|g| format!("{g:x}")).collect::<Vec<_>>().join(":");
    match best_start {
        None => hex(&groups),
        Some(s) => format!("{}::{}", hex(&groups[..s]), hex(&groups[s + best_length..])),
    }
}

/// `header`: the header to write to a route target before any payload. The source is the visitor (`peer`); the
/// destination is the unspecified address of the same family on port 443. v2 carries `sni` in a
/// `PP2_TYPE_AUTHORITY` TLV.
pub fn header(version: &str, peer: &str, sni: &str) -> Vec<u8> {
    let source = parse_peer(peer);
    if version == "v1" { v1(source.as_ref()) } else { v2(source.as_ref(), sni) }
}

fn v1(source: Option<&Address>) -> Vec<u8> {
    let Some(source) = source else {
        return b"PROXY UNKNOWN\r\n".to_vec();
    };
    let (protocol, destination) = if source.family == 4 { ("TCP4", "0.0.0.0") } else { ("TCP6", "::") };
    format!("PROXY {protocol} {} {destination} {} {PUBLIC_PORT}\r\n", format_address(source), source.port).into_bytes()
}

fn v2(source: Option<&Address>, sni: &str) -> Vec<u8> {
    let authority = sni.as_bytes();
    // A TLS server name is at most 255 bytes; anything longer is not one.
    let tlv = if !authority.is_empty() && authority.len() <= 255 { 3 + authority.len() } else { 0 };
    let addresses = match source {
        None => 0,
        Some(s) if s.family == 4 => 12,
        Some(_) => 36,
    };
    let length = addresses + tlv;
    let mut out = vec![0u8; 16 + length];
    out[..12].copy_from_slice(&V2_SIGNATURE);
    out[12] = if source.is_some() { V2_PROXY } else { V2_LOCAL };
    out[13] = match source {
        None => V2_UNSPEC,
        Some(s) if s.family == 4 => V2_TCP4,
        Some(_) => V2_TCP6,
    };
    out[14..16].copy_from_slice(&(length as u16).to_be_bytes());
    let mut offset = 16;
    if let Some(source) = source {
        let size = source.bytes.len();
        out[offset..offset + size].copy_from_slice(&source.bytes);
        // The destination is the unspecified address: already zero.
        offset += size * 2;
        out[offset..offset + 2].copy_from_slice(&source.port.to_be_bytes());
        out[offset + 2..offset + 4].copy_from_slice(&PUBLIC_PORT.to_be_bytes());
        offset += 4;
    }
    if tlv > 0 {
        out[offset] = PP2_TYPE_AUTHORITY;
        out[offset + 1..offset + 3].copy_from_slice(&(authority.len() as u16).to_be_bytes());
        out[offset + 3..offset + 3 + authority.len()].copy_from_slice(authority);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peers() {
        assert_eq!(parse_peer("1.2.3.4:5"), Some(Address { family: 4, bytes: vec![1, 2, 3, 4], port: 5 }));
        assert_eq!(parse_peer("1.2.3.4"), Some(Address { family: 4, bytes: vec![1, 2, 3, 4], port: 0 }));
        assert_eq!(parse_peer("01.2.3.4:5"), None);
        assert_eq!(parse_peer("1.2.3.256:5"), None);
        assert_eq!(parse_peer("1.2.3.4:65536"), None);
        assert_eq!(parse_peer("[::ffff:1.2.3.4]:80"), Some(Address { family: 4, bytes: vec![1, 2, 3, 4], port: 80 }));
        let v6 = parse_peer("[2001:db8::1]:443").unwrap();
        assert_eq!((v6.family, v6.port), (6, 443));
        assert_eq!(format_address(&v6), "2001:db8::1");
        assert_eq!(format_address(&parse_peer("::").unwrap()), "::");
        assert_eq!(format_address(&parse_peer("1:0:0:2:0:0:0:3").unwrap()), "1:0:0:2::3");
        assert_eq!(format_address(&parse_peer("1:0:2:3:4:5:6:7").unwrap()), "1:0:2:3:4:5:6:7");
        assert_eq!(parse_peer("1::2::3"), None);
        assert_eq!(parse_peer("[::1]"), None);
        assert_eq!(parse_peer("nope"), None);
    }

    #[test]
    fn headers() {
        assert_eq!(header("v1", "1.2.3.4:5678", "a.b"), b"PROXY TCP4 1.2.3.4 0.0.0.0 5678 443\r\n");
        assert_eq!(header("v1", "[2001:db8::1]:5", "a.b"), b"PROXY TCP6 2001:db8::1 :: 5 443\r\n");
        assert_eq!(header("v1", "bogus", "a.b"), b"PROXY UNKNOWN\r\n");
        let v2 = header("v2", "1.2.3.4:5678", "a.b");
        let mut expected = V2_SIGNATURE.to_vec();
        expected
            .extend([0x21, 0x11, 0, 18, 1, 2, 3, 4, 0, 0, 0, 0, 0x16, 0x2e, 0x01, 0xbb, 0x02, 0, 3, b'a', b'.', b'b']);
        assert_eq!(v2, expected);
        let local = header("v2", "bogus", "");
        let mut expected = V2_SIGNATURE.to_vec();
        expected.extend([0x20, 0x00, 0, 0]);
        assert_eq!(local, expected);
        assert_eq!(header("v2", "[::1]:1", &"x".repeat(256)).len(), 16 + 36);
    }
}
