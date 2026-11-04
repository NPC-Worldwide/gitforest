use std::collections::HashMap;

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn cache_restore_step_script(with: &HashMap<String, String>) -> String {
    let path = with.get("path").map(String::as_str).unwrap_or(".");
    let key = with.get("key").map(String::as_str).unwrap_or("");
    if key.is_empty() {
        return "echo 'gitforest/cache: key is required' >&2; exit 1".to_string();
    }
    let mut args = format!(
        "cache restore --key {} --path {}",
        shell_quote(key),
        shell_quote(path)
    );
    if let Some(restore_keys) = with.get("restore-keys").filter(|s| !s.trim().is_empty()) {
        args.push_str(&format!(" --restore-keys {}", shell_quote(restore_keys)));
    }
    format!(
        "_gf_hit=$(gitforest {args}) || exit 1\nprintf '%s\\n' \"$_gf_hit\" >> \"$GF_OUTPUTS\"\necho \"$_gf_hit\""
    )
}

pub fn cache_save_step_script(with: &HashMap<String, String>) -> String {
    let path = with.get("path").map(String::as_str).unwrap_or(".");
    let key = with.get("key").map(String::as_str).unwrap_or("");
    if key.is_empty() {
        return "echo 'gitforest/cache-save: key is required' >&2; exit 1".to_string();
    }
    format!(
        "gitforest cache save --key {} --path {}",
        shell_quote(key),
        shell_quote(path)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_script_quotes_and_sets_output() {
        let with = HashMap::from([
            ("path".to_string(), "target stuff".to_string()),
            ("key".to_string(), "linux-cargo-abc".to_string()),
            (
                "restore-keys".to_string(),
                "linux-cargo-,linux-".to_string(),
            ),
        ]);
        let script = cache_restore_step_script(&with);
        assert!(script.contains("--key 'linux-cargo-abc'"), "{script}");
        assert!(script.contains("--path 'target stuff'"), "{script}");
        assert!(
            script.contains("--restore-keys 'linux-cargo-,linux-'"),
            "{script}"
        );
        assert!(script.contains("$GF_OUTPUTS"), "{script}");
    }

    #[test]
    fn restore_script_needs_a_key() {
        let with = HashMap::from([("path".to_string(), ".".to_string())]);
        let script = cache_restore_step_script(&with);
        assert!(script.contains("key is required"), "{script}");
        assert!(script.ends_with("exit 1"), "{script}");
    }

    #[test]
    fn save_script_form() {
        let with = HashMap::from([
            ("path".to_string(), "target".to_string()),
            ("key".to_string(), "k".to_string()),
        ]);
        let script = cache_save_step_script(&with);
        assert!(
            script.contains("gitforest cache save --key 'k' --path 'target'"),
            "{script}"
        );
    }
}
