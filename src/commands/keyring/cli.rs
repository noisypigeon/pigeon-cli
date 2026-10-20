use clap::{Args, Subcommand};

use crate::commands::keyring::email::imap_client::ImapSecurity;
use crate::commands::keyring::email::provider::Provider;
use crate::core::observability::Observable;

#[derive(Args, Debug)]
pub struct KeyringArgs {
    #[command(subcommand)]
    pub command: KeyringCommands,
}

#[derive(Subcommand, Debug)]
pub enum KeyringCommands {
    /// Add a new email identity or bucket-config
    Add(AddArgs),
    /// Edit an existing entry's fields
    Modify {
        /// Alias of the entry to edit. Interactively selected from every
        /// configured entry (of any kind) when omitted.
        alias: Option<String>,
    },
    /// Remove a configured entry and its secret
    Delete {
        /// Alias of the entry to remove.
        alias: String,
    },
    /// List every configured entry
    List,
}

impl Observable for KeyringCommands {
    fn command_name(&self) -> &'static str {
        match self {
            KeyringCommands::Add(_) => "keyring.add",
            KeyringCommands::Modify { .. } => "keyring.modify",
            KeyringCommands::Delete { .. } => "keyring.delete",
            KeyringCommands::List => "keyring.list",
        }
    }
}

#[derive(Args, Debug)]
pub struct AddArgs {
    #[command(subcommand)]
    pub kind: Option<AddKind>,
}

#[derive(Subcommand, Debug)]
pub enum AddKind {
    /// Authenticate an email identity and register it under a local alias
    Email {
        /// Email address of the identity to authenticate, e.g. first.last@example.com
        email: String,

        /// Local alias to store this identity under (e.g. first-last).
        /// Defaults to a sanitized form of the email's local part.
        #[arg(long)]
        alias: Option<String>,

        /// Email provider. Auto-detected from the email's domain when
        /// omitted, falling back to an interactive prompt if detection fails.
        #[arg(long)]
        provider: Option<Provider>,

        /// IMAP host, e.g. imap.example.com. Required when --provider is
        /// "custom" (or resolves to it); defaults to the provider's own host
        /// otherwise (127.0.0.1 for proton's local Bridge).
        #[arg(long)]
        host: Option<String>,

        /// IMAP port. Defaults to the provider's own port (1143 for proton's
        /// Bridge), or 993 for a custom provider. Set it for a Bridge
        /// configured to listen elsewhere.
        #[arg(long)]
        port: Option<u16>,

        /// How the connection is secured: "tls" (implicit, port 993) or
        /// "starttls" (plaintext greeting, then STARTTLS). Defaults to
        /// starttls for proton's Bridge and for a custom server on port 143,
        /// tls otherwise. Set "tls" for a Bridge switched to SSL mode.
        /// There is no plaintext option (CLI-0162).
        #[arg(long, value_enum)]
        security: Option<ImapSecurity>,
    },

    /// Interactively configure a new S3-compatible bucket-config
    Bucket {
        /// Alias to register this bucket-config under (e.g. "backup").
        /// Prompted if omitted.
        alias: Option<String>,
    },
}
