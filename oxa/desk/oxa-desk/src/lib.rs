//! oxa-desk: the window of the desktop app (views and widgets). No window exists yet.
//!
//! The binary is an oxa (see `main.rs`): only an argument list that asks for the window leaves
//! the oxa path, so every child that oxa starts from `current_exe()` (askpass, workers, subagents,
//! the local engine) behaves exactly as under `oxa`.

/// Arguments that only the desktop understands start with this. oxa has no such flag.
const DESK_FLAG: &str = "--desk-";

/// Whether this start asks for the window instead of the oxa command line.
///
/// Until the window exists, only an explicit `--desk-*` first argument does; a bare `oxa-desk`
/// is still `oxa`. The first argument decides: a later one belongs to an oxa command.
pub fn wants_window(args: &[String]) -> bool {
    args.first().is_some_and(|first| first.starts_with(DESK_FLAG))
}

/// Opens the window and returns the process exit code. There is no window in this build, so
/// this says so and fails instead of pretending.
pub fn run(args: &[String]) -> i32 {
    let asked = args.first().map(String::as_str).unwrap_or_default();
    eprintln!("oxa-desk: {asked}: the desktop window is not part of this build yet");
    2
}

#[cfg(test)]
mod tests {
    use super::{run, wants_window};

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn everything_oxa_understands_stays_oxa() {
        // What oxa itself, its workers and its helpers are started with.
        let oxa: [&[&str]; 8] = [
            &[],
            &["--version"],
            &["-p", "hello"],
            &["--mode", "rpc"],
            &["work", "status", "--json"],
            &["lead", "serve"],
            &["-p", "--desk-smoke"],
            &["desk"],
        ];
        for list in oxa {
            assert!(!wants_window(&args(list)), "{list:?}");
        }
    }

    #[test]
    fn a_desk_flag_first_asks_for_the_window_and_fails_while_there_is_none() {
        let asked = args(&["--desk-smoke"]);
        assert!(wants_window(&asked));
        assert_ne!(run(&asked), 0);
    }
}
