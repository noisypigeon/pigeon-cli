use crate::commands::job::cli::{JobCommands, JobType};
use crate::commands::job::{decrypt_files, email_sync};

pub fn dispatch(command: JobCommands) -> i32 {
    match command {
        JobCommands::Run(run_args) => match run_args.job_type {
            JobType::EmailSync {
                identities,
                local_output,
                remote_output,
                encryption_key,
                concurrency,
                yes,
            } => email_sync::wizard::dispatch(
                identities,
                local_output,
                remote_output,
                encryption_key,
                concurrency,
                yes,
            ),
            JobType::DecryptFiles {
                input_dir,
                output_dir,
                encryption_key,
                concurrency,
                yes,
            } => decrypt_files::wizard::dispatch(
                input_dir,
                output_dir,
                encryption_key,
                concurrency,
                yes,
            ),
        },
    }
}
