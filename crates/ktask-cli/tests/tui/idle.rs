//! Nothing happening: with no key pressed and no other process touching the queue, the
//! screen is drawn once and the interface then sits still — no timer wakes it, and it burns
//! no CPU while it waits.

use std::thread;
use std::time::Duration;

use super::navigate::Fixture;
use super::support::Result;

/// Long enough that the timer this loop used to run on (every 200ms) would have fired several
/// times over, short enough to keep the test quick.
const IDLE: Duration = Duration::from_millis(900);

#[test]
fn nothing_happening_draws_no_further_frames_and_uses_no_cpu() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(24)?;

    let frames_before = terminal.frame_count();
    let cpu_before = terminal.cpu_ticks()?;
    thread::sleep(IDLE);
    let frames_after = terminal.frame_count();
    let cpu_after = terminal.cpu_ticks()?;

    assert_eq!(
        frames_before, frames_after,
        "an idle screen with no key pressed and nothing changed drew a frame it had no reason to"
    );
    // A generous allowance for scheduler noise around the sleep itself: a genuinely blocked
    // process uses no measurable CPU here, while a 200ms poll loop doing real work several
    // times over nearly a second would clear this easily.
    assert!(
        cpu_after - cpu_before <= 2,
        "an idle screen used {} ticks of CPU while waiting for nothing",
        cpu_after - cpu_before
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}
