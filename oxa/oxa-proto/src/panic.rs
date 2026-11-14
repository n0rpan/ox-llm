//! Panic containment (pre-flight A1/A2/B9).
//!
//! The release binary is built with `panic = "unwind"`, so a panic at a boundary where
//! third-party or user code runs (an extension host function, a script job, a tool, a hook set,
//! a front end's command) is caught there and turned into an error the user sees, and the
//! session continues. [`contain`] is that boundary.
//!
//! While a thread is inside [`contain`], the process panic hook ([`install_hook`]) does not
//! print the default `thread '...' panicked at` message (the terminal UI owns the screen); the
//! message is handed back to the boundary instead. Outside a contained region the previous hook
//! runs unchanged. A front end that restores the terminal from its own hook asks
//! [`is_contained`] first: a contained panic must leave the terminal as it is.
use std::any::Any;
use std::cell::{Cell, RefCell};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    static LAST: RefCell<Option<String>> = const { RefCell::new(None) };
}

static HOOK: AtomicBool = AtomicBool::new(false);

/// `true` while the current thread runs inside [`contain`].
pub fn is_contained() -> bool {
    DEPTH.with(|d| d.get() > 0)
}

/// Run `f`; a panic in it is caught and returned as its message (`Err`). The state `f` touched
/// is the caller's to repair (it is treated as unwind-safe).
pub fn contain<R>(f: impl FnOnce() -> R) -> Result<R, String> {
    DEPTH.with(|d| d.set(d.get() + 1));
    let out = catch_unwind(AssertUnwindSafe(f));
    DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    out.map_err(|payload| {
        let seen = LAST.with(|l| l.borrow_mut().take());
        seen.unwrap_or_else(|| message(payload.as_ref()))
    })
}

/// The text of a panic payload (`&str` / `String`; anything else is "unknown panic").
pub fn message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// Install the process panic hook (idempotent; call it first thing in `main`). Inside a
/// contained region the message is kept for [`contain`] and nothing is printed; elsewhere the
/// previous hook runs.
pub fn install_hook() {
    if HOOK.swap(true, Ordering::SeqCst) {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if is_contained() {
            let text = message(info.payload());
            LAST.with(|l| *l.borrow_mut() = Some(text));
            return;
        }
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contain_returns_the_message_and_restores_the_depth() {
        install_hook();
        assert!(!is_contained());
        let r: Result<(), String> = contain(|| panic!("boom {}", 7));
        assert_eq!(r, Err("boom 7".to_string()));
        assert!(!is_contained());
        assert_eq!(contain(|| 5), Ok(5));
        let nested = contain(|| {
            assert!(is_contained());
            contain(|| -> u8 { panic!("inner") })
        });
        assert_eq!(nested, Ok(Err("inner".to_string())));
        assert!(!is_contained());
    }
}
