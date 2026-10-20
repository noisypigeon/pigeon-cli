use crate::commands::job::cli::{
    EmailAction, EmailArgs, JobCommands, JobType, RcloneAction, RcloneArgs,
};
use crate::commands::job::shared_wizard::note_unused_ledger_bucket;
use crate::commands::job::{deduplicate, email_import, email_scan, rclone, transform};
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
                        debug_log_bucket,
                        ledger_bucket,
                        legacy,
                        non_interactive,
                    } => email_import::wizard::dispatch(
                        identities,
                        destination,
                        local_output,
                        messages_per_minute,
                        batch_size,
                        max_connections,
                        push_concurrency,
                        debug_log_bucket,
                        ledger_bucket,
                        legacy,
                        job_name,
                        non_interactive,
                    ),
                    EmailAction::Scan {
                        identities,
                        ledger_bucket,
                        destination,
                        local_output,
                        debug_log_bucket,
                        non_interactive,
                    } => email_scan::wizard::dispatch(
                        identities,
                        ledger_bucket,
                        destination,
                        local_output,
                        debug_log_bucket,
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
                    debug_log_bucket,
                    ledger_bucket,
                    non_interactive,
                } => {
                    note_unused_ledger_bucket(job_name, &ledger_bucket);
                    deduplicate::wizard::dispatch(
                        source_bucket,
                        local_output,
                        destination_bucket,
                        concurrency,
                        transfers,
                        checkers,
                        tpslimit,
                        upload_only,
                        debug_log_bucket,
                        job_name,
                        non_interactive,
                    )
                }
                JobType::Rclone(RcloneArgs { action }) => match action {
                    RcloneAction::Copy {
                        source_bucket,
                        destination_bucket,
                        local_output,
                        debug_log_bucket,
                        ledger_bucket,
                        transfers,
                        checkers,
                        tpslimit,
                        non_interactive,
                    } => {
                        note_unused_ledger_bucket(job_name, &ledger_bucket);
                        rclone::wizard::dispatch_copy(
                            source_bucket,
                            destination_bucket,
                            local_output,
                            debug_log_bucket,
                            transfers,
                            checkers,
                            tpslimit,
                            job_name,
                            non_interactive,
                        )
                    }
                    RcloneAction::Delete {
                        source_bucket,
                        local_output,
                        debug_log_bucket,
                        ledger_bucket,
                        checkers,
                        non_interactive,
                    } => {
                        note_unused_ledger_bucket(job_name, &ledger_bucket);
                        rclone::wizard::dispatch_delete(
                            source_bucket,
                            local_output,
                            debug_log_bucket,
                            checkers,
                            job_name,
                            non_interactive,
                        )
                    }
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
                    debug_log_bucket,
                    ledger_bucket,
                    non_interactive,
                } => {
                    note_unused_ledger_bucket(job_name, &ledger_bucket);
                    transform::wizard::dispatch(
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
                        debug_log_bucket,
                        job_name,
                        non_interactive,
                    )
                }
            })
        }
    }
}
