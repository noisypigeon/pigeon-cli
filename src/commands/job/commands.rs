use crate::commands::job::cli::{JobCommands, JobType};
use crate::commands::job::{decrypt_files, dedupe, email_pull, email_sync, pull_transform, sort};
use crate::core::observability::Observable as _;

pub fn dispatch(command: JobCommands) -> i32 {
    match command {
        JobCommands::Run(run_args) => {
            let name = run_args.job_type.command_name();
            crate::observability::run_instrumented(name, move || match run_args.job_type {
                JobType::EmailSync {
                    identities,
                    local_output,
                    remote_output,
                    encryption_key,
                    concurrency,
                    max_connections_per_identity,
                    yes,
                } => email_sync::wizard::dispatch(
                    identities,
                    local_output,
                    remote_output,
                    encryption_key,
                    concurrency,
                    max_connections_per_identity,
                    yes,
                ),
                JobType::EmailPull {
                    identities,
                    local_output,
                    remote_output,
                    concurrency,
                    max_connections_per_identity,
                    yes,
                } => email_pull::wizard::dispatch(
                    identities,
                    local_output,
                    remote_output,
                    concurrency,
                    max_connections_per_identity,
                    yes,
                ),
                JobType::Sort {
                    source_bucket,
                    local_output,
                    remote_output,
                    concurrency,
                    yes,
                } => sort::wizard::dispatch(
                    source_bucket,
                    local_output,
                    remote_output,
                    concurrency,
                    yes,
                ),
                JobType::Dedupe {
                    source_bucket,
                    local_output,
                    remote_output,
                    concurrency,
                    yes,
                } => dedupe::wizard::dispatch(
                    source_bucket,
                    local_output,
                    remote_output,
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
                JobType::PullTransform {
                    source_bucket,
                    local_output,
                    remote_output,
                    encryption_key,
                    file_types,
                    expand_zips,
                    image_format,
                    video_format,
                    audio_format,
                    concurrency,
                    yes,
                } => pull_transform::wizard::dispatch(
                    source_bucket,
                    local_output,
                    remote_output,
                    encryption_key,
                    file_types,
                    expand_zips,
                    image_format,
                    video_format,
                    audio_format,
                    concurrency,
                    yes,
                ),
            })
        }
    }
}
