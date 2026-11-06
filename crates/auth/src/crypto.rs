use std::collections::HashMap;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use rand::Rng;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use url::form_urlencoded;

pub(crate) fn random_bytes() -> [u8; 32] {
    let mut bytes = [0; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes
}

pub(crate) fn generate_token(prefix: &str) -> String {
    format!("{prefix}_{}", hex::encode(random_bytes()))
}

pub(crate) fn sha256_hex(data: &str) -> String {
    hex::encode(Sha256::digest(data))
}

pub(crate) fn sha256_base64url(data: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(data))
}

/// Compares secrets without an early exit on the first differing byte.
pub(crate) fn secret_eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// `purpose` separates uses of the one key, so a token minted for one form is useless for another.
pub(crate) fn sign(key: &[u8], purpose: &str, payload: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(purpose.as_bytes());
    mac.update(&[0]);
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

pub(crate) fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Parses a query string or form body. The first value of a repeated key wins, like
/// `URLSearchParams.get`, so all checks of one request see the same value.
pub(crate) fn parse_params(input: &[u8]) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for (key, value) in form_urlencoded::parse(input) {
        params
            .entry(key.into_owned())
            .or_insert_with(|| value.into_owned());
    }
    params
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_digest_matches_the_rfc_7636_vector() {
        assert_eq!(
            sha256_base64url("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn secret_eq_handles_length_and_content() {
        assert!(secret_eq("abc", "abc"));
        assert!(!secret_eq("abc", "abd"));
        assert!(!secret_eq("abc", "abcd"));
    }

    #[test]
    fn signatures_depend_on_key_purpose_and_payload() {
        let base = sign(b"key", "a", "payload");
        assert_eq!(base, sign(b"key", "a", "payload"));
        assert_ne!(base, sign(b"other", "a", "payload"));
        assert_ne!(base, sign(b"key", "b", "payload"));
        assert_ne!(base, sign(b"key", "a", "other"));
    }

    #[test]
    fn escape_html_covers_every_attribute_and_element_breakout() {
        assert_eq!(
            escape_html(r#"<a href="x" o='y'>&"#),
            "&lt;a href=&quot;x&quot; o=&#39;y&#39;&gt;&amp;"
        );
    }

    #[test]
    fn repeated_params_keep_the_first_value() {
        let params = parse_params(b"a=1&a=2&b=x+y%21");
        assert_eq!(params["a"], "1");
        assert_eq!(params["b"], "x y!");
    }
}
