//! The one way a test keeps a screen. Only a frame a [`super::Terminal`] wait returned can
//! be recorded, so every stored screen is a settled PTY frame of the real binary.
use super::{
    golden::{self, Input},
    normalise::{self, Volatile},
};
use std::cell::RefCell;

/// Where the suite leaves what it recorded, for the `screens` gate to compare.
const OUTPUT: &str = "KTASK_SCREENS_DIR";

thread_local! {
    /// The frames this test's terminals have shown.
    static SHOWN: RefCell<Vec<Shown>> = const { RefCell::new(Vec::new()) };
    /// The screens this test has recorded; libtest runs each test on its own thread.
    static RECORDED: RefCell<Vec<golden::Frame>> = const { RefCell::new(Vec::new()) };
    /// How many sandbox names this test has asked for.
    static SANDBOXES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// The paths of this test's sandboxes, and what they read as.
    static PATHS: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
}

struct Shown {
    screen: String,
    size: (u16, u16),
    /// Whether a wait took it as settled; a frame only looked at cannot be recorded.
    settled: bool,
    /// Whether its terminal had received mouse input by then.
    mouse_used: bool,
}

/// The next sandbox name of the running test: the same on every run, and another each
/// time it is asked, so a name a concurrent run holds is passed over.
pub(super) fn sandbox_name() -> String {
    let number = SANDBOXES.get();
    SANDBOXES.set(number + 1);
    let test = std::thread::current();
    // FNV-1a: stable across toolchains, unlike the standard hasher.
    let hash = format!("{}#{number}", test.name().unwrap_or_default())
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
    format!("run-{:06x}", hash & 0xff_ffff)
}

/// Called for every sandbox a test opens: its path may appear on screen.
pub(super) fn sandbox_opened(path: &std::path::Path) {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_owned());
    PATHS.with_borrow_mut(|paths| {
        paths.extend(normalise::sandbox_paths(&path.to_string_lossy()));
    });
}

/// Called by the terminal for every screen it hands to a test.
pub(super) fn shown(screen: &str, size: (u16, u16), settled: bool, mouse_used: bool) {
    SHOWN.with_borrow_mut(|shown| {
        let same =
            |was: &Shown| was.screen == screen && was.size == size && was.mouse_used == mouse_used;
        match shown.last_mut().filter(|was| same(was)) {
            Some(was) => was.settled |= settled,
            None => shown.push(Shown {
                screen: screen.to_owned(),
                size,
                settled,
                mouse_used,
            }),
        }
    });
}

/// The size at which `screen` may be recorded as an `input` frame: it must be one a wait
/// returned, and not a keyboard frame of a terminal that had received mouse input.
pub fn accepted(step: &str, input: Input, screen: &str) -> Result<(u16, u16), String> {
    let refused = |why: &str| Err(format!("screen {step:?} {why}:\n{screen}"));
    SHOWN.with_borrow(|shown| {
        let mut taken = shown.iter().rev().filter(|was| was.screen == screen);
        let looked_at = taken.clone().next().is_some();
        match taken.find(|was| was.settled) {
            Some(was) if was.mouse_used && input == Input::Keyboard => {
                refused("is labelled keyboard after mouse input")
            }
            Some(was) => Ok(was.size),
            None if looked_at => refused("was only looked at, never settled by a wait"),
            None => refused("is not a frame a terminal showed"),
        }
    })
}

/// Records `screen`, as a wait on a terminal returned it, under `step`. `input` says how
/// the session was driven: a `mouse` flag or [`Input`]. A frame shown after its terminal
/// received mouse input is refused as a keyboard frame.
pub fn record(step: &str, input: impl Into<Input>, screen: &str) {
    let input = input.into();
    let size = accepted(step, input, screen).unwrap_or_else(|refusal| panic!("{refusal}"));
    let text = PATHS.with_borrow(|paths| {
        normalise::normalise(
            screen,
            &Volatile {
                paths,
                checkout: env!("CARGO_MANIFEST_DIR"),
                version: env!("CARGO_PKG_VERSION"),
            },
        )
    });
    let test = std::thread::current()
        .name()
        .filter(|name| *name != "main")
        .expect("screens are recorded on a test's thread")
        .to_owned();
    RECORDED.with_borrow_mut(|frames| {
        golden::add(frames, step, size, input, text).unwrap_or_else(|error| panic!("{error}"));
        let Some(directory) = std::env::var_os(OUTPUT) else {
            return;
        };
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory)
            .and_then(|()| {
                std::fs::write(
                    directory.join(golden::file_name(&test)),
                    golden::render(frames),
                )
            })
            .unwrap_or_else(|error| panic!("recording screen {step:?}: {error}"));
    });
}
