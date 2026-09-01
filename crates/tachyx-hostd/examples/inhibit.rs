//! Holds an idle inhibitor for a few seconds and releases it, to confirm the
//! compositor accepts one from a daemon that has no window.

fn main() -> Result<(), tachyx_hostd::idle::Error> {
    let inhibitor = tachyx_hostd::idle::Inhibitor::new()?;
    println!("idle inhibited");
    std::thread::sleep(std::time::Duration::from_secs(3));
    drop(inhibitor);
    println!("idle released");
    Ok(())
}
