//! Cryptographically secure random tokens for local authentication.

const TOKEN_BYTES: usize = 32;
const LOWER_HEX: &[u8; 16] = b"0123456789abcdef";

/// Generates a 256-bit token encoded as 64 lowercase hexadecimal characters.
///
/// Returns the operating system entropy-source error without adding context so
/// each caller can describe which token it was trying to create.
pub fn secure_random_hex_token() -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::getrandom(&mut bytes)?;
    Ok(encode_lower_hex(&bytes))
}

fn encode_lower_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(LOWER_HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(LOWER_HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thirty_two_bytes_encode_to_sixty_four_lowercase_hex_characters() {
        let encoded = encode_lower_hex(&[0xabu8; TOKEN_BYTES]);

        assert_eq!(encoded.len(), 64);
        assert_eq!(encoded, "ab".repeat(TOKEN_BYTES));
    }

    #[test]
    fn lower_hex_encoding_covers_nibble_boundaries() {
        assert_eq!(
            encode_lower_hex(&[0x00, 0x09, 0x0a, 0x0f, 0x10, 0x90, 0xa0, 0xf0, 0xff]),
            "00090a0f1090a0f0ff"
        );
    }

    #[test]
    fn generated_token_has_the_wire_shape() {
        let token = secure_random_hex_token().expect("operating system entropy is available");

        assert_eq!(token.len(), 64);
        assert!(token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')));
    }
}
