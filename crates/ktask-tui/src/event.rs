//! [`AppEvent`]: everything that can change the interface's state.
//!
//! The thin shell translates the outside world into these values (a key
//! press, a journal event, a timer, a resized terminal) and hands them to
//! [`update`](crate::update). Nothing else reaches the state, which is what
//! lets a scripted sequence of them stand in for a terminal in tests.

use crossterm::event::KeyEvent;
use ktask_core::Event;

/// One input to [`update`](crate::update).
#[derive(Debug, Clone, PartialEq)]
pub enum AppEvent {
    /// The operator pressed a key.
    Key(KeyEvent),
    /// The supervisor recorded something in the journal.
    Core(Event),
    /// A clock tick, for anything that changes with time alone.
    Tick,
    /// The terminal was resized to this many columns and rows.
    Resize(u16, u16),
}
