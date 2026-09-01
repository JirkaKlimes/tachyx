//! Moves the pointer a few pixels and returns it, to confirm injection reaches
//! the compositor.
//!
//! Run with the name of an output, for example `cargo run -p tachyx-input
//! --example probe -- eDP-1`.

use tachyx_proto::input::InputEvent;

fn main() -> Result<(), tachyx_input::Error> {
    let output = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "eDP-1".to_owned());
    let mut injector = tachyx_input::Injector::new(&output)?;
    println!("injector ready on {output}");

    for (dx, dy) in [(5.0, 5.0), (-5.0, -5.0)] {
        injector.apply(&[InputEvent::PointerMotion { dx, dy }])?;
        std::thread::sleep(std::time::Duration::from_millis(120));
        println!("moved {dx:+} {dy:+}");
    }
    println!("pointer returned to where it started");
    Ok(())
}
