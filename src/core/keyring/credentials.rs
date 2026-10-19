/// Env var name checked ahead of a stored secret (CLI-0087) -- `PIGEON_SECRET_`
/// plus `alias` uppercased, with every character outside `[A-Z0-9_]` replaced
/// by `_`. Lets a secret be injected directly, independent of wherever
/// `keyring.toml` itself stores its secret fields -- which is what makes a
/// non-interactive or restarted container usable without touching the config
/// file at all.
fn env_var_name(alias: &str) -> String {
    let mut name = String::from("PIGEON_SECRET_");
    for ch in alias.chars() {
        let upper = ch.to_ascii_uppercase();
        name.push(if upper.is_ascii_alphanumeric() || upper == '_' {
            upper
        } else {
            '_'
        });
    }
    name
}

/// Resolves the secret to use for `alias`: `PIGEON_SECRET_<ALIAS>` (CLI-0087)
/// wins outright if set, otherwise `stored` -- the plain secret field already
/// loaded from `keyring.toml` (CLI-0135) -- is returned as-is. Storage-backend
/// independent and infallible: there is no OS keychain left to fail to open.
pub(crate) fn resolve_secret(alias: &str, stored: &str) -> String {
    std::env::var(env_var_name(alias)).unwrap_or_else(|_| stored.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_name_uppercases_and_sanitizes_the_alias() {
        assert_eq!(env_var_name("gmail-work"), "PIGEON_SECRET_GMAIL_WORK");
        assert_eq!(env_var_name("My.Bucket"), "PIGEON_SECRET_MY_BUCKET");
    }

    #[test]
    fn resolve_secret_prefers_the_env_var_over_the_stored_value() {
        let alias = "cli-0135-test-alias";
        let var = env_var_name(alias);
        // SAFETY: tests run single-threaded within this process for this env var
        // (no other test reads/writes a PIGEON_SECRET_* var concurrently).
        unsafe {
            std::env::set_var(&var, "injected-secret");
        }
        let result = resolve_secret(alias, "stored-secret");
        unsafe {
            std::env::remove_var(&var);
        }
        assert_eq!(result, "injected-secret");
    }

    #[test]
    fn resolve_secret_falls_back_to_the_stored_value_when_no_env_var_is_set() {
        let alias = "cli-0135-test-alias-unset";
        assert_eq!(resolve_secret(alias, "stored-secret"), "stored-secret");
    }
}
