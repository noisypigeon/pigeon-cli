use clap::{Args, Subcommand};

use crate::commands::keyring::email::provider::Provider;

#[derive(Args, Debug)]
pub struct KeyringArgs {
    #[command(subcommand)]
    pub command: KeyringCommands,
}

#[derive(Subcommand, Debug)]
pub enum KeyringCommands {
    /// Add a new email identity or bucket-config
    Add(AddArgs),
    /// Edit an existing email identity's or bucket-config's fields
    Modify {
        /// Alias of the entry to edit. Interactively selected from every
        /// configured entry (of either kind) when omitted.
        alias: Option<String>,
    },
    /// Remove a configured email identity or bucket-config and its secret
    Delete {
        /// Alias of the entry to remove.
        alias: String,
    },
    /// List every configured email identity and bucket-config
    List,
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
        /// "custom" (or resolves to it); ignored for known providers, which
        /// use their own well-known host.
        #[arg(long)]
        host: Option<String>,

        /// IMAP port. Defaults to 993 for a custom provider when omitted;
        /// ignored for known providers, which use their own well-known port.
        #[arg(long)]
        port: Option<u16>,
    },

    /// Interactively configure a new S3-compatible bucket-config
    Bucket {
        /// Alias to register this bucket-config under (e.g. "backup").
        /// Prompted if omitted.
        alias: Option<String>,
    },
}
