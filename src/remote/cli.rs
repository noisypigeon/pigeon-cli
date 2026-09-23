use clap::{Args, Subcommand};

#[derive(Args, Debug)]
pub struct RemoteArgs {
    #[command(subcommand)]
    pub command: RemoteCommands,
}

#[derive(Subcommand, Debug)]
pub enum RemoteCommands {
    /// Interactively configure a new S3-compatible remote
    Configure {
        /// Alias to register this remote under (e.g. "email"). Prompted if omitted.
        alias: Option<String>,
    },

    /// List configured remotes
    List,

    /// Interactively edit an existing remote's bucket, endpoint, access key,
    /// or secret key. The alias itself can't be changed this way -- remove
    /// and reconfigure under a new alias instead.
    Edit {
        /// Alias of the remote to edit. Interactively selected when omitted.
        alias: Option<String>,
    },

    /// Remove a configured remote and its stored secret
    Remove {
        /// Alias of the remote to remove. Interactively selected when omitted.
        alias: Option<String>,
    },

    /// List buckets reachable with a configured remote's credentials
    ListBuckets {
        /// Alias of the remote to use. Interactively selected when omitted.
        alias: Option<String>,
    },

    /// List files recursively under a remote location
    Ls {
        /// Location to list, e.g. `email:` or `email:archive/2020`
        location: String,
    },

    /// List directories (one level, non-recursive) under a remote location
    Lsd {
        /// Location to list, e.g. `email:` or `email:archive/2020`
        location: String,
    },

    /// Copy between a local path and a remote (or vice versa)
    Copy {
        /// Source: a local filesystem path, or `alias:path`
        source: String,

        /// Destination: a local filesystem path, or `alias:path`
        dest: String,
    },
}
