use thiserror::Error;

pub const BRACKETED_PASTE_START: &str = "\x1b[200~";
pub const BRACKETED_PASTE_END: &str = "\x1b[201~";
pub const DEFAULT_MAX_INLINE_BYTES: usize = 64 * 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TransportError {
    #[error("payload contains a bracketed paste control sequence")]
    BracketedPasteControl,
    #[error("payload contains NUL")]
    Nul,
    #[error("payload contains carriage return")]
    CarriageReturn,
    #[error("payload contains unexpected control character 0x{0:02x}")]
    ControlCharacter(u8),
    #[error("payload exceeds inline byte limit: {actual} > {limit}")]
    TooLarge { actual: usize, limit: usize },
    #[error("invalid structured message kind")]
    InvalidKind,
}

pub fn bracketed_paste(payload: &str) -> Result<String, TransportError> {
    bracketed_paste_with_limit(payload, DEFAULT_MAX_INLINE_BYTES)
}

pub fn bracketed_paste_with_limit(
    payload: &str,
    max_inline_bytes: usize,
) -> Result<String, TransportError> {
    validate_payload(payload, max_inline_bytes)?;
    Ok(format!(
        "{BRACKETED_PASTE_START}{payload}{BRACKETED_PASTE_END}\r"
    ))
}

pub fn structured_block_paste(kind: &str, body: &str) -> Result<String, TransportError> {
    validate_kind(kind)?;
    let end = format!("[/{kind}]");
    let escaped_body = body.replace(&end, &format!("[/{kind}\\]"));
    let block = format!("[{kind}]\n{escaped_body}\n{end}");
    bracketed_paste(&block)
}

pub fn validate_payload(payload: &str, max_inline_bytes: usize) -> Result<(), TransportError> {
    if payload.contains(BRACKETED_PASTE_START) || payload.contains(BRACKETED_PASTE_END) {
        return Err(TransportError::BracketedPasteControl);
    }

    let byte_len = payload.len();
    if byte_len > max_inline_bytes {
        return Err(TransportError::TooLarge {
            actual: byte_len,
            limit: max_inline_bytes,
        });
    }

    for byte in payload.bytes() {
        match byte {
            0x00 => return Err(TransportError::Nul),
            b'\r' => return Err(TransportError::CarriageReturn),
            b'\n' | b'\t' => {}
            0x01..=0x08 | 0x0b..=0x0c | 0x0e..=0x1f | 0x7f => {
                return Err(TransportError::ControlCharacter(byte));
            }
            _ => {}
        }
    }

    Ok(())
}

fn validate_kind(kind: &str) -> Result<(), TransportError> {
    if kind.is_empty()
        || !kind
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte == b'_')
    {
        return Err(TransportError::InvalidKind);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        BRACKETED_PASTE_END, BRACKETED_PASTE_START, TransportError, bracketed_paste_with_limit,
        structured_block_paste,
    };

    #[test]
    fn wraps_payload_as_bracketed_paste() {
        let wrapped = bracketed_paste_with_limit("hello\nworld", 100).unwrap();
        assert_eq!(wrapped, "\x1b[200~hello\nworld\x1b[201~\r");
    }

    #[test]
    fn rejects_raw_bracketed_paste_controls() {
        let error = bracketed_paste_with_limit(BRACKETED_PASTE_START, 100).unwrap_err();
        assert_eq!(error, TransportError::BracketedPasteControl);

        let error = bracketed_paste_with_limit(BRACKETED_PASTE_END, 100).unwrap_err();
        assert_eq!(error, TransportError::BracketedPasteControl);
    }

    #[test]
    fn rejects_unexpected_control_characters() {
        assert_eq!(
            bracketed_paste_with_limit("bad\0", 100).unwrap_err(),
            TransportError::Nul
        );
        assert_eq!(
            bracketed_paste_with_limit("bad\r", 100).unwrap_err(),
            TransportError::CarriageReturn
        );
        assert_eq!(
            bracketed_paste_with_limit("bad\x07", 100).unwrap_err(),
            TransportError::ControlCharacter(0x07)
        );
    }

    #[test]
    fn enforces_inline_limit() {
        assert_eq!(
            bracketed_paste_with_limit("abcdef", 5).unwrap_err(),
            TransportError::TooLarge {
                actual: 6,
                limit: 5
            }
        );
    }

    #[test]
    fn escapes_closing_delimiter_inside_structured_body() {
        let wrapped =
            structured_block_paste("USER_REQUEST", "body\n[/USER_REQUEST]\nmore").unwrap();
        assert!(wrapped.contains("[/USER_REQUEST\\]"));
        assert!(wrapped.ends_with("\x1b[201~\r"));
    }
}
