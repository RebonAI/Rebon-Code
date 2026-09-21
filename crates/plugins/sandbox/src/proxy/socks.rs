//! SOCKS5, enough of it to make a domain decision — RFC 1928.
//!
//! Present because the sandbox sets `all_proxy`, and a good deal of tooling
//! prefers SOCKS when it sees one. It is worth having for the same reason the
//! HTTP side is: a SOCKS5 `CONNECT` with `ATYP = 3` carries the **hostname**,
//! not a resolved address, so the decision is made on the same name the user
//! wrote in their settings.
//!
//! `ATYP = 1` / `ATYP = 4` carry a raw address instead. Those are passed to
//! the same policy as a literal, where an allowlist refuses them — a client
//! that resolves the name itself must not thereby skip the check.
//!
//! Only the no-authentication method is offered. The listener is loopback (or
//! a unix socket) and reachable only by processes that already have the
//! sandbox's environment; a password on top would be a password every client
//! would have to be taught, protecting a door that leads only to the
//! allowlist.

/// Parsed pieces of a SOCKS5 `CONNECT` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocksRequest {
    pub host: String,
    pub port: u16,
    /// The address bytes as received, so the reply can echo them.
    pub raw_address: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SocksError {
    #[error("not a SOCKS5 greeting")]
    BadVersion,
    #[error("the client offered no authentication method this proxy supports")]
    NoAcceptableMethod,
    #[error("only CONNECT is supported")]
    UnsupportedCommand,
    #[error("unknown address type")]
    UnknownAddressType,
    #[error("the request is truncated")]
    Truncated,
}

pub const VERSION: u8 = 0x05;
pub const METHOD_NONE: u8 = 0x00;
pub const METHOD_UNACCEPTABLE: u8 = 0xFF;
pub const CMD_CONNECT: u8 = 0x01;

pub const REPLY_SUCCESS: u8 = 0x00;
pub const REPLY_GENERAL_FAILURE: u8 = 0x01;
/// What a policy refusal returns. RFC 1928's "connection not allowed by
/// ruleset" is exactly this case, and a client that renders reply codes will
/// say so rather than reporting a generic failure.
pub const REPLY_NOT_ALLOWED: u8 = 0x02;
pub const REPLY_HOST_UNREACHABLE: u8 = 0x04;
pub const REPLY_COMMAND_NOT_SUPPORTED: u8 = 0x07;

/// Check a greeting (`VER NMETHODS METHODS...`) and pick a method.
pub fn parse_greeting(bytes: &[u8]) -> Result<u8, SocksError> {
    if bytes.len() < 2 {
        return Err(SocksError::Truncated);
    }
    if bytes[0] != VERSION {
        return Err(SocksError::BadVersion);
    }
    let count = bytes[1] as usize;
    let methods = bytes.get(2..2 + count).ok_or(SocksError::Truncated)?;
    if methods.contains(&METHOD_NONE) {
        Ok(METHOD_NONE)
    } else {
        Err(SocksError::NoAcceptableMethod)
    }
}

/// How many bytes the address of the given type occupies, including the
/// length prefix for a domain name and the two port bytes.
pub fn address_length(address_type: u8, first_byte: Option<u8>) -> Result<usize, SocksError> {
    match address_type {
        0x01 => Ok(4 + 2),
        0x03 => first_byte
            .map(|length| 1 + length as usize + 2)
            .ok_or(SocksError::Truncated),
        0x04 => Ok(16 + 2),
        _ => Err(SocksError::UnknownAddressType),
    }
}

/// Parse a `CONNECT` request (`VER CMD RSV ATYP ADDR PORT`).
pub fn parse_request(bytes: &[u8]) -> Result<SocksRequest, SocksError> {
    if bytes.len() < 4 {
        return Err(SocksError::Truncated);
    }
    if bytes[0] != VERSION {
        return Err(SocksError::BadVersion);
    }
    if bytes[1] != CMD_CONNECT {
        return Err(SocksError::UnsupportedCommand);
    }

    let address_type = bytes[3];
    let rest = &bytes[4..];
    let (host, consumed) = match address_type {
        0x01 => {
            let octets = rest.get(..4).ok_or(SocksError::Truncated)?;
            (
                format!("{}.{}.{}.{}", octets[0], octets[1], octets[2], octets[3]),
                4,
            )
        }
        0x03 => {
            let length = *rest.first().ok_or(SocksError::Truncated)? as usize;
            let name = rest.get(1..1 + length).ok_or(SocksError::Truncated)?;
            (
                String::from_utf8(name.to_vec()).map_err(|_| SocksError::Truncated)?,
                1 + length,
            )
        }
        0x04 => {
            let octets = rest.get(..16).ok_or(SocksError::Truncated)?;
            let groups: Vec<String> = octets
                .chunks(2)
                .map(|pair| format!("{:x}", u16::from_be_bytes([pair[0], pair[1]])))
                .collect();
            (groups.join(":"), 16)
        }
        _ => return Err(SocksError::UnknownAddressType),
    };

    let port_bytes = rest
        .get(consumed..consumed + 2)
        .ok_or(SocksError::Truncated)?;
    Ok(SocksRequest {
        host,
        port: u16::from_be_bytes([port_bytes[0], port_bytes[1]]),
        raw_address: bytes[3..4 + consumed + 2].to_vec(),
    })
}

/// A reply. The bound-address fields are zeroed: the client does not need
/// them for a `CONNECT`, and reporting the proxy's own address would be
/// telling the sandbox something about the host it has no use for.
pub fn reply(code: u8) -> Vec<u8> {
    vec![VERSION, code, 0x00, 0x01, 0, 0, 0, 0, 0, 0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_no_auth_greeting_is_accepted() {
        assert_eq!(parse_greeting(&[0x05, 0x01, 0x00]).unwrap(), METHOD_NONE);
        assert_eq!(
            parse_greeting(&[0x05, 0x02, 0x02, 0x00]).unwrap(),
            METHOD_NONE
        );
    }

    #[test]
    fn a_greeting_without_no_auth_is_refused() {
        assert_eq!(
            parse_greeting(&[0x05, 0x01, 0x02]).unwrap_err(),
            SocksError::NoAcceptableMethod
        );
    }

    #[test]
    fn socks4_is_not_socks5() {
        assert_eq!(
            parse_greeting(&[0x04, 0x01, 0x00]).unwrap_err(),
            SocksError::BadVersion
        );
    }

    #[test]
    fn a_truncated_greeting_is_refused_rather_than_indexed_past() {
        // The length byte is attacker-controlled; slicing on it without a
        // check is how a proxy panics on its first malformed client.
        assert_eq!(parse_greeting(&[0x05]).unwrap_err(), SocksError::Truncated);
        assert_eq!(
            parse_greeting(&[0x05, 0x04, 0x00]).unwrap_err(),
            SocksError::Truncated
        );
    }

    #[test]
    fn a_domain_request_carries_the_name_the_user_wrote() {
        // The reason SOCKS is worth supporting at all: the decision is made
        // on the hostname, not on whatever it resolved to.
        let mut bytes = vec![0x05, 0x01, 0x00, 0x03, 11];
        bytes.extend_from_slice(b"example.com");
        bytes.extend_from_slice(&443u16.to_be_bytes());

        let request = parse_request(&bytes).unwrap();

        assert_eq!(request.host, "example.com");
        assert_eq!(request.port, 443);
    }

    #[test]
    fn an_ipv4_request_becomes_a_dotted_literal() {
        let mut bytes = vec![0x05, 0x01, 0x00, 0x01, 93, 184, 216, 34];
        bytes.extend_from_slice(&443u16.to_be_bytes());
        assert_eq!(parse_request(&bytes).unwrap().host, "93.184.216.34");
    }

    #[test]
    fn an_ipv6_request_becomes_a_colon_literal() {
        let mut bytes = vec![0x05, 0x01, 0x00, 0x04];
        bytes.extend_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        bytes.extend_from_slice(&80u16.to_be_bytes());
        assert_eq!(parse_request(&bytes).unwrap().host, "2001:db8:0:0:0:0:0:1");
    }

    #[test]
    fn bind_and_associate_are_refused() {
        for command in [0x02u8, 0x03] {
            let bytes = vec![0x05, command, 0x00, 0x01, 1, 2, 3, 4, 0, 80];
            assert_eq!(
                parse_request(&bytes).unwrap_err(),
                SocksError::UnsupportedCommand
            );
        }
    }

    #[test]
    fn an_unknown_address_type_is_refused() {
        let bytes = vec![0x05, 0x01, 0x00, 0x09, 1, 2];
        assert_eq!(
            parse_request(&bytes).unwrap_err(),
            SocksError::UnknownAddressType
        );
    }

    #[test]
    fn a_truncated_request_is_refused_at_every_stage() {
        assert_eq!(parse_request(&[0x05]).unwrap_err(), SocksError::Truncated);
        // A domain length longer than the bytes that follow it.
        let mut bytes = vec![0x05, 0x01, 0x00, 0x03, 50];
        bytes.extend_from_slice(b"short");
        assert_eq!(parse_request(&bytes).unwrap_err(), SocksError::Truncated);
        // A complete domain with no port after it.
        let mut bytes = vec![0x05, 0x01, 0x00, 0x03, 3];
        bytes.extend_from_slice(b"abc");
        assert_eq!(parse_request(&bytes).unwrap_err(), SocksError::Truncated);
    }

    #[test]
    fn a_non_utf8_domain_is_refused() {
        let bytes = vec![0x05, 0x01, 0x00, 0x03, 2, 0xff, 0xfe, 0, 80];
        assert_eq!(parse_request(&bytes).unwrap_err(), SocksError::Truncated);
    }

    #[test]
    fn address_lengths_cover_every_type() {
        assert_eq!(address_length(0x01, None).unwrap(), 6);
        assert_eq!(address_length(0x03, Some(11)).unwrap(), 14);
        assert_eq!(address_length(0x04, None).unwrap(), 18);
        assert!(address_length(0x03, None).is_err());
        assert!(address_length(0x09, None).is_err());
    }

    #[test]
    fn a_policy_refusal_uses_the_ruleset_reply_code() {
        // RFC 1928 has a code for exactly this, and a client that renders
        // reply codes will say "not allowed" rather than "failed".
        let bytes = reply(REPLY_NOT_ALLOWED);
        assert_eq!(bytes[0], VERSION);
        assert_eq!(bytes[1], REPLY_NOT_ALLOWED);
        assert_eq!(bytes.len(), 10);
    }
}
