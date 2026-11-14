//! Credential redaction for logs and stored errors.
//!
//! The market feed receives its JWT as `?token=...`, so a failed WebSocket
//! handshake would otherwise print a live session token in the worker terminal
//! and in `/api/state`.

const SECRET_KEYS: [&str; 5] = ["token", "access_token", "refresh_token", "session", "key"];
const TOKEN_CHARS: &[u8] = b"._~+/-";

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_value_terminator(byte: u8) -> bool {
    matches!(byte, b'&' | b' ' | b'\t' | b'\r' | b'\n' | b'\'' | b'"')
}

/// Removes credentials from a URL/error string before it is logged or stored.
pub fn redact_secrets(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;

    while i < bytes.len() {
        // `?token=...` / `&key=...` query secrets.
        if (bytes[i] == b'?' || bytes[i] == b'&') && i + 1 < bytes.len() {
            let key_start = i + 1;
            let mut key_end = key_start;
            while key_end < bytes.len()
                && bytes[key_end] != b'='
                && !is_value_terminator(bytes[key_end])
            {
                key_end += 1;
            }
            let key = &value[key_start..key_end];
            let is_secret = SECRET_KEYS
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(key));
            if is_secret && key_end < bytes.len() && bytes[key_end] == b'=' {
                let value_start = key_end + 1;
                let mut value_end = value_start;
                while value_end < bytes.len() && !is_value_terminator(bytes[value_end]) {
                    value_end += 1;
                }
                out.push(bytes[i] as char);
                out.push_str(key);
                out.push_str("=<redacted>");
                i = value_end;
                continue;
            }
        }

        // `Bearer <token>` headers.
        if i + 6 <= bytes.len()
            && bytes[i..i + 6].eq_ignore_ascii_case(b"bearer")
            && (i == 0 || !is_word_byte(bytes[i - 1]))
        {
            let mut cursor = i + 6;
            if cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t') {
                while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t') {
                    cursor += 1;
                }
                let token_start = cursor;
                while cursor < bytes.len()
                    && (bytes[cursor].is_ascii_alphanumeric()
                        || TOKEN_CHARS.contains(&bytes[cursor])
                        || bytes[cursor] == b'=')
                {
                    cursor += 1;
                }
                if cursor > token_start {
                    out.push_str(&value[i..i + 6]);
                    out.push(' ');
                    out.push_str("<redacted>");
                    i = cursor;
                    continue;
                }
            }
        }

        // Copy one UTF-8 scalar, not one byte, so multi-byte text survives.
        let ch = value[i..].chars().next().unwrap_or('\u{0}');
        out.push(ch);
        i += ch.len_utf8();
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_query_tokens() {
        let redacted = redact_secrets("wss://nws.rollercoin.com/?token=abc.def&x=1");
        assert!(redacted.contains("token=<redacted>"));
        assert!(!redacted.contains("abc.def"));
    }

    #[test]
    fn strips_bearer_headers() {
        let redacted = redact_secrets("Authorization: Bearer eyJhbGciOi.abc.def");
        assert_eq!(redacted, "Authorization: Bearer <redacted>");
    }

    #[test]
    fn leaves_ordinary_urls_alone() {
        let url = "https://rollercoin.com/api/marketplace/purchase-item?page=2";
        assert_eq!(redact_secrets(url), url);
    }

    #[test]
    fn keeps_non_ascii_text_intact() {
        assert_eq!(redact_secrets("gagal — coba lagi"), "gagal — coba lagi");
    }
}
