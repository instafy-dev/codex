use super::*;
use std::process::Stdio;
use std::time::Duration;
use tokio::time::Instant;

#[tokio::test]
async fn contained_process_shutdown_is_confirmed_and_exact_handle_termination_is_idempotent()
-> io::Result<()> {
    let job = JobObject::create_without_breakaway()?;
    let mut command = Command::new("cmd.exe");
    command
        .args(["/D", "/C", "set /p stop="])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    job.prepare_suspended_spawn(&mut command);
    let mut child = command.spawn()?;
    let pid = child.id().expect("spawned child has an id");
    let process = JobObject::open_process_handle(pid)?;
    assert!(job.assign_and_resume_process(pid)?);
    assert!(job.has_running_processes()?);
    assert!(!JobObject::process_has_exited(&process)?);

    job.terminate()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while job.has_running_processes()? && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!job.has_running_processes()?);
    child.wait().await?;
    assert!(JobObject::process_has_exited(&process)?);
    JobObject::terminate_process_handle(&process)?;
    job.terminate()?;
    Ok(())
}
