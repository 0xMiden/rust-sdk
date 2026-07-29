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
fn report_replay_snapshot_write(
    recorder: Option<&miden_debug::ReplaySnapshotRecorder>,
    requested_path: Option<&std::path::Path>,
    classify_error: impl Fn(String, String) -> crate::errors::CliError,
) -> Result<(), crate::errors::CliError> {
    let Some(recorder) = recorder else {
        return Ok(());
    };

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
        Some(Err(err)) => Err(classify_error(
            err.to_string(),
            format!("failed to write replay snapshot to {}", err.path.display()),
        )),
        None => {
            let path = requested_path
                .map_or_else(|| "<unknown>".to_string(), |path| path.display().to_string());
            Err(classify_error(
                "replay snapshot was not written".to_string(),
                format!("debug session ended without writing replay snapshot to {path}"),
            ))
        },
    }
}
