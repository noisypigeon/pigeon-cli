use crate::commands::job::cli::{
    EmailAction, EmailArgs, JobCommands, JobType, RcloneAction, RcloneArgs,
};
use crate::commands::job::{deduplicate, email_import, rclone, transform};
use crate::core::observability::Observable as _;

pub fn dispatch(command: JobCommands) -> i32 {
    match command {
        JobCommands::Run(run_args) => {
            let name = run_args.job_type.command_name();
            let job_name = run_args.job_type.job_name();
            crate::observability::run_instrumented(name, move || match run_args.job_type {
                JobType::Email(EmailArgs { action }) => match action {
                    EmailAction::Import {
                        identities,
                        destination,
                        local_output,
                        messages_per_minute,
                        batch_size,
                        max_connections,
                        push_concurrency,
                        report_bucket,
                        non_interactive,
                    } => email_import::wizard::dispatch(
                        identities,
                        destination,
                        local_output,
                        messages_per_minute,
                        batch_size,
                        max_connections,
                        push_concurrency,
                        report_bucket,
                        job_name,
                        non_interactive,
                    ),
                },
                JobType::Deduplicate {
                    source_bucket,
                    local_output,
                    destination_bucket,
                    concurrency,
                    transfers,
                    checkers,
                    tpslimit,
                    upload_only,
                    report_bucket,
                    non_interactive,
                } => deduplicate::wizard::dispatch(
                    source_bucket,
                    local_output,
                    destination_bucket,
                    concurrency,
                    transfers,
                    checkers,
                    tpslimit,
                    upload_only,
                    report_bucket,
                    job_name,
                    non_interactive,
                ),
                JobType::Rclone(RcloneArgs { action }) => match action {
                    RcloneAction::Copy {
                        source_bucket,
                        destination_bucket,
                        local_output,
                        report_bucket,
                        transfers,
                        checkers,
                        tpslimit,
                        non_interactive,
                    } => rclone::wizard::dispatch_copy(
                        source_bucket,
                        destination_bucket,
                        local_output,
                        report_bucket,
                        transfers,
                        checkers,
                        tpslimit,
                        job_name,
                        non_interactive,
                    ),
                    RcloneAction::Delete {
                        source_bucket,
                        local_output,
                        report_bucket,
                        checkers,
                        non_interactive,
                    } => rclone::wizard::dispatch_delete(
                        source_bucket,
                        local_output,
                        report_bucket,
                        checkers,
                        job_name,
                        non_interactive,
                    ),
                },
                JobType::Transform {
                    input_file_type,
                    video_quality,
                    audio_quality,
                    source,
                    destination,
                    local_output,
                    concurrency,
                    download_transfers,
                    download_checkers,
                    upload_transfers,
                    max_pending_downloads,
                    tpslimit,
                    report_bucket,
                    non_interactive,
                } => transform::wizard::dispatch(
                    input_file_type,
                    video_quality,
                    audio_quality,
                    source,
                    destination,
                    local_output,
                    concurrency,
                    download_transfers,
                    download_checkers,
                    upload_transfers,
                    max_pending_downloads,
                    tpslimit,
                    report_bucket,
                    job_name,
                    non_interactive,
                ),
            })
        }
    }
}
