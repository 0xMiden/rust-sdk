pub mod account;
pub mod address;
pub mod call;
pub mod clear_config;
pub mod exec;
pub mod export;
pub mod import;
pub mod info;
pub mod init;
pub mod network_note_status;
pub mod new_account;
pub mod new_transactions;
pub mod notes;
pub mod sync;
pub mod tags;
pub mod transactions;

#[cfg(feature = "dap")]
use crate::errors::CliError;

#[cfg(feature = "dap")]
fn report_replay_snapshot_write(
    recorder: &miden_debug::ReplaySnapshotRecorder,
    requested_path: &std::path::Path,
) -> Result<(), CliError> {
    match recorder.take() {
        Some(Ok(write)) => {
            println!(
                "Wrote replay snapshot ({} event(s), {} forest(s)) to {}; replay it with \
                 `miden-debug --replay {}`.",
                write.event_count,
                write.forest_count,
                write.path.display(),
                write.path.display()
            );
            Ok(())
        },
        Some(Err(err)) => Err(CliError::ReplaySnapshot(Box::new(err))),
        None => Err(CliError::ReplaySnapshot(
            format!(
                "debug session ended without writing replay snapshot to {}",
                requested_path.display()
            )
            .into(),
        )),
    }
}
