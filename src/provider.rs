use std::fmt;

use clap::ValueEnum;
use dialoguer::{Select, theme::ColorfulTheme};
use serde::{Deserialize, Serialize};

/// A known email provider, or a manually configured IMAP server.
///
/// Per ADR-0003, authentication is always via an app-specific (or, for
/// Proton, Bridge-issued) password over plain IMAP `LOGIN` -- never OAuth2.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    Gmail,
    Fastmail,
    Icloud,
    Proton,
    Custom,
}

/// Domains recognized as belonging to a known provider, used to suggest a
/// default for `pigeon email authenticate` when `--provider` is omitted.
const DOMAIN_TABLE: &[(&str, Provider)] = &[
    ("gmail.com", Provider::Gmail),
    ("googlemail.com", Provider::Gmail),
    ("fastmail.com", Provider::Fastmail),
    ("fastmail.fm", Provider::Fastmail),
    ("icloud.com", Provider::Icloud),
    ("me.com", Provider::Icloud),
    ("mac.com", Provider::Icloud),
];

impl Provider {
    /// Suggests a provider from the domain part of an email address.
    /// Returns `None` when the domain isn't recognized (e.g. Google
    /// Workspace on a custom domain, or Proton, which has no fixed domain).
    pub fn detect(email: &str) -> Option<Provider> {
        let domain = email.rsplit('@').next()?.to_ascii_lowercase();
        DOMAIN_TABLE
            .iter()
            .find(|(known, _)| *known == domain)
            .map(|(_, provider)| *provider)
    }

    /// The default IMAP host/port for this provider, or `None` for `Custom`
    /// (the caller must supply `--host`/`--port` explicitly).
    pub fn default_host_port(&self) -> Option<(&'static str, u16)> {
        match self {
            Provider::Gmail => Some(("imap.gmail.com", 993)),
            Provider::Fastmail => Some(("imap.fastmail.com", 993)),
            Provider::Icloud => Some(("imap.mail.me.com", 993)),
            // Proton Mail Bridge's default local IMAP endpoint.
            Provider::Proton => Some(("127.0.0.1", 1143)),
            Provider::Custom => None,
        }
    }

    /// Whether the TLS handshake should accept an invalid/self-signed
    /// certificate. Only Proton Bridge needs this: it terminates TLS
    /// locally with a self-signed cert, unlike the public-CA-backed
    /// servers of the other providers.
    pub fn accepts_invalid_certs(&self) -> bool {
        matches!(self, Provider::Proton)
    }

    /// Interactively prompts the user to pick a provider, used when
    /// `--provider` is omitted and the email's domain isn't recognized.
    pub fn prompt_select() -> std::io::Result<Provider> {
        let options = [
            Provider::Gmail,
            Provider::Fastmail,
            Provider::Icloud,
            Provider::Proton,
            Provider::Custom,
        ];
        let labels: Vec<String> = options.iter().map(|p| p.to_string()).collect();
        let selection = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Select a provider")
            .items(&labels)
            .default(0)
            .interact()?;
        Ok(options[selection])
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Provider::Gmail => "gmail",
            Provider::Fastmail => "fastmail",
            Provider::Icloud => "icloud",
            Provider::Proton => "proton",
            Provider::Custom => "custom",
        };
        f.write_str(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_known_domains() {
        assert_eq!(
            Provider::detect("first.last@gmail.com"),
            Some(Provider::Gmail)
        );
        assert_eq!(
            Provider::detect("first.last@fastmail.fm"),
            Some(Provider::Fastmail)
        );
        assert_eq!(
            Provider::detect("first.last@me.com"),
            Some(Provider::Icloud)
        );
    }

    #[test]
    fn unrecognized_domain_is_none() {
        assert_eq!(Provider::detect("first.last@example.com"), None);
    }

    #[test]
    fn custom_has_no_default_host_port() {
        assert_eq!(Provider::Custom.default_host_port(), None);
    }

    #[test]
    fn only_proton_accepts_invalid_certs() {
        assert!(Provider::Proton.accepts_invalid_certs());
        assert!(!Provider::Gmail.accepts_invalid_certs());
        assert!(!Provider::Custom.accepts_invalid_certs());
    }
}
