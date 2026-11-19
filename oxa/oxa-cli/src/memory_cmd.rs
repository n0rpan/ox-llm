//! M3: `oxa memory list|show|rm`, the owner's view of the memory store (`oxa_resources::memory`)
//! under `Dirs::memory_dir`. `list` prints the generated index, `show <name>` one entry, and
//! `rm <name>` deletes an entry together with its index line.
//!
//! Exit codes: 0 done; 1 no entry has that name; 2 a usage error or an invalid name.
use oxa_config::Dirs;
use oxa_proto::{codes, OxaError};
use oxa_resources::memory;
use std::io::Write;

const USAGE: &str = "usage: oxa memory list | oxa memory show <name> | oxa memory rm <name>";

fn usage(message: &str) -> OxaError {
    OxaError::new(codes::USAGE, message).with_hint(USAGE)
}

fn emit(out: &mut dyn Write, text: &str) -> Result<(), OxaError> {
    out.write_all(text.as_bytes())
        .map_err(|e| OxaError::io("cannot write memory output", &e))
}

/// Run `oxa memory ..`: the words after `memory`. Returns the exit code.
pub fn run_memory(
    words: &[String],
    dirs: &Dirs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, OxaError> {
    let dir = dirs.memory_dir();
    match words {
        [verb] if verb == "list" => {
            let index = memory::index(&dir, usize::MAX);
            if index.is_empty() {
                emit(out, "No memory entries.\n")?;
            } else {
                emit(out, &index)?;
            }
            Ok(0)
        }
        [verb, name] if verb == "show" => match memory::read(&dir, name)? {
            Some(entry) => {
                emit(
                    out,
                    &format!(
                        "{} ({}): {}\n\n",
                        entry.name,
                        entry.kind.as_str(),
                        entry.description
                    ),
                )?;
                emit(out, &entry.text)?;
                Ok(0)
            }
            None => not_found(name, err),
        },
        [verb, name] if verb == "rm" => {
            if memory::delete(&dir, name)? {
                emit(out, &format!("removed {name}\n"))?;
                Ok(0)
            } else {
                not_found(name, err)
            }
        }
        _ => Err(usage("expected: list, show <name> or rm <name>")),
    }
}

/// A missing entry is a runtime failure (exit 1), reported on stderr.
fn not_found(name: &str, err: &mut dyn Write) -> Result<i32, OxaError> {
    let _ = writeln!(err, "no memory entry named {name}");
    Ok(1)
}
