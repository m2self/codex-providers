use anyhow::{Context as _, Result};

pub fn validate_provider_id(id: &str) -> Result<()> {
    if id.trim().is_empty() {
        anyhow::bail!("provider id cannot be empty");
    }

    let mut has_alnum = false;
    for ch in id.chars() {
        if ch.is_ascii_alphanumeric() {
            has_alnum = true;
            continue;
        }
        if ch == '_' || ch == '-' {
            continue;
        }
        anyhow::bail!(
            "provider id '{}' contains invalid character {:?}; allowed: [A-Za-z0-9_-]",
            id,
            ch
        );
    }

    if !has_alnum {
        anyhow::bail!(
            "provider id '{}' must include at least one alphanumeric character",
            id
        );
    }

    Ok(())
}

pub fn generate_env_key(id: &str) -> String {
    let mut sanitized = String::new();
    let mut last_was_underscore = false;

    for ch in id.chars() {
        let upper = ch.to_ascii_uppercase();
        if upper.is_ascii_alphanumeric() {
            sanitized.push(upper);
            last_was_underscore = false;
            continue;
        }

        if !last_was_underscore {
            sanitized.push('_');
            last_was_underscore = true;
        }
    }

    while sanitized.ends_with('_') {
        sanitized.pop();
    }

    let sanitized = sanitized.trim_matches('_');
    format!("CODEX_{sanitized}_KEY")
}

pub fn validate_base_url(base_url: &str) -> Result<()> {
    let url = url::Url::parse(base_url).with_context(|| format!("invalid base_url '{base_url}'"))?;
    match url.scheme() {
        "http" | "https" => {}
        s => anyhow::bail!("base_url scheme must be http or https (got '{s}')"),
    }
    if url.host_str().is_none() {
        anyhow::bail!("base_url must include a host");
    }
    Ok(())
}

#[cfg(any(test, all(unix, target_os = "linux")))]
pub fn bash_single_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Parse values produced by `bash_single_quote` (including the `'abc'\\''def'` concatenation).
#[cfg(any(test, all(unix, target_os = "linux")))]
pub fn bash_unquote_single_quoted_concatenation(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut i = 0;
    let mut out = String::new();

    while i < bytes.len() {
        if bytes[i] != b'\'' {
            return None;
        }

        // opening quote
        i += 1;
        while i < bytes.len() && bytes[i] != b'\'' {
            out.push(bytes[i] as char);
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        // closing quote
        i += 1;
        if i >= bytes.len() {
            break;
        }

        // Expect escaped single quote: \'
        if i + 1 < bytes.len() && bytes[i] == b'\\' && bytes[i + 1] == b'\'' {
            out.push('\'');
            i += 2;
            continue;
        }

        return None;
    }

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_key_generation() {
        assert_eq!(generate_env_key("zapi"), "CODEX_ZAPI_KEY");
        assert_eq!(generate_env_key("my-provider"), "CODEX_MY_PROVIDER_KEY");
        assert_eq!(generate_env_key("my__provider"), "CODEX_MY_PROVIDER_KEY");
        assert_eq!(generate_env_key("my.provider"), "CODEX_MY_PROVIDER_KEY");
        assert_eq!(generate_env_key("_zapi"), "CODEX_ZAPI_KEY");
        assert_eq!(generate_env_key("zapi_"), "CODEX_ZAPI_KEY");
    }

    #[test]
    fn bash_quote_roundtrip() {
        let v = "abc'def";
        let quoted = bash_single_quote(v);
        assert_eq!(bash_unquote_single_quoted_concatenation(&quoted), Some(v.to_string()));

        let v2 = "no-quotes";
        let quoted2 = bash_single_quote(v2);
        assert_eq!(
            bash_unquote_single_quoted_concatenation(&quoted2),
            Some(v2.to_string())
        );
    }

    #[test]
    fn provider_id_validation() {
        assert!(validate_provider_id("zapi").is_ok());
        assert!(validate_provider_id("my-provider_1").is_ok());
        assert!(validate_provider_id("").is_err());
        assert!(validate_provider_id("---").is_err());
        assert!(validate_provider_id("bad id").is_err());
        assert!(validate_provider_id("bad.id").is_err());
    }
}
