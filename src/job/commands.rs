use crate::job::cli::{JobCommands, JobType};
use crate::job::email_sync;

pub fn dispatch(command: JobCommands) -> i32 {
    match command {
        JobCommands::Run(run_args) => match run_args.job_type {
            JobType::EmailSync {
                identities,
                local_output,
                remote_output,
                concurrency,
                yes,
            } => email_sync::dispatch(identities, local_output, remote_output, concurrency, yes),
        },
    }
}
