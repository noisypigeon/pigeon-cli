use clap::{Args, Subcommand};

#[derive(Args, Debug)]
pub struct DataopsArgs {
    #[command(subcommand)]
    pub command: DataopsCommands,
}

#[derive(Subcommand, Debug)]
pub enum DataopsCommands {
    /// Manage configured bucket-configs (create, edit, remove)
    BucketConfig(BucketConfigArgs),
}

#[derive(Args, Debug)]
pub struct BucketConfigArgs {
    #[command(subcommand)]
    pub command: BucketConfigCommands,
}

#[derive(Subcommand, Debug)]
pub enum BucketConfigCommands {
    /// Interactively configure a new S3-compatible bucket-config
    New {
        /// Alias to register this bucket-config under (e.g. "email"). Prompted if omitted.
        alias: Option<String>,
    },

    /// Interactively edit an existing bucket-config's bucket, endpoint,
    /// access key, or secret key. The alias itself can't be changed this
    /// way -- remove and reconfigure under a new alias instead.
    Edit {
        /// Alias of the bucket-config to edit. Interactively selected when omitted.
        alias: Option<String>,
    },

    /// Remove a configured bucket-config and its stored secret
    Remove {
        /// Alias of the bucket-config to remove. Interactively selected when omitted.
        alias: Option<String>,
    },
}
