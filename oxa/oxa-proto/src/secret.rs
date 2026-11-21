//! A password a process of this session asks the owner for: what the dialog is told about the
//! request ([`SecretAsk`]) and the owner's answer ([`SecretValue`]).
//!
//! The answer has one home from the dialog to the write into the asking process's socket. It is
//! not a `String`: it cannot be printed, formatted, cloned or serialised, and its buffer is
//! overwritten with zeros when it is dropped.

/// What a `Ui` is told about one password request ([`crate::Ui::secret_ask`]). Everything here is
/// shown to the owner as data; none of it is secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecretAsk<'a> {
    /// The dialog's own title, written by oxa.
    pub title: &'a str,
    /// The requesting process's own prompt text (sudo's `Password:` line), as written.
    pub prompt: &'a str,
    /// The exact command the asking process was started for. Empty when it is not known.
    pub command: &'a str,
    /// The process id of the asking process (the `sudo` oxa started). `0` when it is not known.
    pub pid: u32,
    /// The tool whose call started that process (`bash`).
    pub tool: &'a str,
}

/// The owner's answer to a password request. It deliberately has no `Debug`, `Display`, `Clone`
/// or serialisation: the only way to read it is [`SecretValue::expose`], and the buffer is
/// overwritten with zeros when the value is dropped.
///
/// The buffer is allocated once and never grows, so no reallocation leaves a copy behind.
pub struct SecretValue(Vec<u8>);

impl SecretValue {
    /// Copy `bytes` into a new value. The caller still owns (and should clear) its own copy.
    pub fn from_bytes(bytes: &[u8]) -> SecretValue {
        let mut buf = Vec::with_capacity(bytes.len());
        buf.extend_from_slice(bytes);
        SecretValue(buf)
    }

    /// Take over the buffer of `text` without copying it: the string's allocation becomes the
    /// value's, so it is zeroed with it (including bytes typed and deleted again).
    pub fn from_string(text: String) -> SecretValue {
        SecretValue(text.into_bytes())
    }

    /// The secret bytes. Write them to where they are needed; never log, record or display them.
    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// Overwrite the whole allocation with zeros: the bytes in use and the spare capacity, which
    /// can hold characters that were typed and deleted again.
    fn wipe(&mut self) {
        let capacity = self.0.capacity();
        // Within the capacity: this never reallocates.
        self.0.resize(capacity, 0);
        self.0.fill(0);
        // Keep the compiler from dropping the writes as dead stores before the buffer is freed.
        std::hint::black_box(self.0.as_slice());
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        self.wipe();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HeadlessUi, Ui};

    /// A test that fails to compile when the type implements the trait: the probe call is then
    /// ambiguous between the two impls.
    macro_rules! assert_not_impl {
        ($name:ident, $ty:ty, $tr:path) => {
            #[test]
            fn $name() {
                #[allow(dead_code)]
                trait Probe<A> {
                    fn probe() {}
                }
                #[allow(dead_code)]
                struct Hit;
                impl<T: ?Sized> Probe<()> for T {}
                impl<T: ?Sized + $tr> Probe<Hit> for T {}
                let _ = <$ty as Probe<_>>::probe;
            }
        };
    }

    assert_not_impl!(a_secret_value_has_no_debug, SecretValue, std::fmt::Debug);
    assert_not_impl!(a_secret_value_has_no_display, SecretValue, std::fmt::Display);
    assert_not_impl!(a_secret_value_has_no_clone, SecretValue, Clone);

    /// `Drop` is exactly `wipe`: after it every byte of the allocation is zero.
    #[test]
    fn the_buffer_is_zero_after_the_drop_wipe() {
        let mut value = SecretValue::from_bytes(b"opaque-example");
        assert_eq!(value.expose(), b"opaque-example");
        value.wipe();
        assert!(value.expose().len() >= 14);
        assert!(value.expose().iter().all(|b| *b == 0));

        // Characters typed and deleted again stay in the string's spare capacity: wiped too.
        let mut typed = String::from("opaque-example-then-deleted");
        typed.truncate(6);
        let mut value = SecretValue::from_string(typed);
        assert_eq!(value.expose(), b"opaque");
        value.wipe();
        assert!(value.expose().len() >= 27, "the spare capacity is covered");
        assert!(value.expose().iter().all(|b| *b == 0));
    }

    /// A `Ui` that only has the old `secret` dialog.
    struct Old(HeadlessUi);

    impl Ui for Old {
        fn secret(&mut self, title: &str, body: &str) -> Option<String> {
            Some(format!("{title}/{body}"))
        }
        fn select(&mut self, t: &str, o: &[String], ms: Option<u32>) -> Option<usize> {
            self.0.select(t, o, ms)
        }
        fn confirm(&mut self, t: &str, b: &str, ms: Option<u32>) -> Option<bool> {
            self.0.confirm(t, b, ms)
        }
        fn input(&mut self, t: &str, p: &str) -> Option<String> {
            self.0.input(t, p)
        }
        fn editor(&mut self, t: &str, p: &str) -> Option<String> {
            self.0.editor(t, p)
        }
        fn notify(&mut self, text: &str, level: crate::Level) {
            self.0.notify(text, level);
        }
        fn set_status(&mut self, _: &str, _: Option<&str>) {}
        fn set_widget(&mut self, _: &str, _: Option<&[String]>, _: crate::Placement) {}
        fn set_title(&mut self, _: &str) {}
        fn set_editor_text(&mut self, _: &str) {}
        fn ask_access(&mut self, a: &crate::Access) -> crate::Answer {
            self.0.ask_access(a)
        }
    }

    #[test]
    fn secret_ask_falls_back_to_the_secret_dialog_and_a_ui_that_cannot_ask_answers_none() {
        let ask = SecretAsk {
            title: "T",
            prompt: "Password:",
            command: "sudo true",
            pid: 7,
            tool: "bash",
        };
        let value = Old(HeadlessUi::default()).secret_ask(&ask).expect("answered");
        assert_eq!(value.expose(), b"T/Password:");
        assert!(HeadlessUi::default().secret_ask(&ask).is_none());
    }
}
