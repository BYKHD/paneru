//! Non-GUI diagnostic using the exact ownership/watcher implementation.
#[path = "../src/owned.rs"]
mod owned;
use std::io::Write;

fn main() -> std::io::Result<()> {
    assert!(owned::enabled());
    assert!(owned::reject_service_control().is_err());
    let parent = owned::owner_pid()?.expect("explicit owner required");
    let (sender, receiver) = std::sync::mpsc::channel();
    owned::watch_parent(parent, move || {
        let _ = sender.send(());
    })?;
    println!("armed");
    std::io::stdout().flush()?;
    receiver.recv().expect("watcher must deliver parent exit");
    println!("parent-exit");
    std::io::stdout().flush()?;
    if std::env::args().any(|arg| arg == "--stall-exit") {
        std::thread::sleep(std::time::Duration::from_secs(30));
    }
    Ok(())
}
