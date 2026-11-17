//! Pure command-menu presentation over the canonical declarations.
use crate::command_registry::{entries, CommandId, Surface, COMMANDS};

pub fn menu(scope: CommandId, surface: Surface) -> Option<String> {
    let root = COMMANDS.iter().find(|entry| entry.id == scope)?;
    let mut text = format!("{}\n", root.summary);
    for entry in entries(surface).filter(|entry| entry.id != scope && entry.path.starts_with(root.path)) {
        text.push_str(&format!("{} — {}\n", entry.path.join(" "), entry.summary));
    }
    Some(text)
}
