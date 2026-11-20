//! The settings page sheet: the rows of `oxa/docs/pages/settings.md`, in page order (issue #2042,
//! R3), one module per section of the page.
//!
//! `super::Descriptor` says what the `/settings` menu shows for a key; this sheet says what the
//! settings page's table says for it. Both live beside each other here, keyed by the same schema
//! keys, and `tools/ci/settings_table.py` renders the page's section blocks from this sheet. The
//! page's prose stays hand-written, so a settings-docs merge conflict is fixed by rerunning the
//! generator instead of by hand. A table row is edited in its section module below.

mod interface;
mod models;
mod other;
mod permissions;
mod retries;
mod sessions;
mod tools;

/// One section of the settings page, in page order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Page {
    Interface,
    Models,
    Tools,
    Sessions,
    Retries,
    Permissions,
    Other,
}

impl Page {
    /// Every section, in page order.
    pub const ALL: [Page; 7] = [
        Page::Interface,
        Page::Models,
        Page::Tools,
        Page::Sessions,
        Page::Retries,
        Page::Permissions,
        Page::Other,
    ];

    /// Stable id, used in the page's `<!-- generated:<id> -->` markers and in `sheet/<id>.rs`.
    pub const fn id(self) -> &'static str {
        match self {
            Page::Interface => "interface",
            Page::Models => "models",
            Page::Tools => "tools",
            Page::Sessions => "sessions",
            Page::Retries => "retries",
            Page::Permissions => "permissions",
            Page::Other => "other",
        }
    }

    /// The `###` heading written above the section's table.
    pub const fn title(self) -> &'static str {
        match self {
            Page::Interface => interface::TITLE,
            Page::Models => models::TITLE,
            Page::Tools => tools::TITLE,
            Page::Sessions => sessions::TITLE,
            Page::Retries => retries::TITLE,
            Page::Permissions => permissions::TITLE,
            Page::Other => other::TITLE,
        }
    }

    /// The section's rows, in the order the page lists them.
    pub fn of(self) -> &'static [PageRow] {
        match self {
            Page::Interface => interface::ROWS,
            Page::Models => models::ROWS,
            Page::Tools => tools::ROWS,
            Page::Sessions => sessions::ROWS,
            Page::Retries => retries::ROWS,
            Page::Permissions => permissions::ROWS,
            Page::Other => other::ROWS,
        }
    }
}

/// One row of the page's `Key | Type | Default | Meaning` table, as it is written there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRow {
    /// Schema key, or schema key + "." + field path.
    pub key: &'static str,
    /// The Type cell.
    pub kind: &'static str,
    /// The Default cell.
    pub default: &'static str,
    /// The Meaning cell.
    pub text: &'static str,
}

/// Starts a page row. One per documented setting, in page order.
const fn row(key: &'static str, kind: &'static str, default: &'static str, text: &'static str) -> PageRow {
    PageRow {
        key,
        kind,
        default,
        text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Every row documents a schema key (or a field of an object schema key), so the page never
    /// lists a setting oxa does not have.
    #[test]
    fn page_rows_are_known_schema_keys() {
        let schema = crate::schema();
        for page in Page::ALL {
            for row in page.of() {
                let owner = schema
                    .iter()
                    .filter(|k| row.key == k.key || row.key.starts_with(&format!("{}.", k.key)))
                    .max_by_key(|k| k.key.len());
                let owner = owner.unwrap_or_else(|| panic!("row {}: no schema key", row.key));
                if row.key != owner.key {
                    assert_eq!(
                        owner.kind,
                        crate::Kind::Object,
                        "row {} must sit under an object",
                        row.key
                    );
                }
            }
        }
    }

    /// A key appears once: the page would otherwise document one setting twice.
    #[test]
    fn page_row_keys_are_unique() {
        let mut seen = HashSet::new();
        for page in Page::ALL {
            for row in page.of() {
                assert!(seen.insert(row.key), "duplicate page row: {}", row.key);
            }
        }
    }

    /// The sections and their ids are what the generator looks for in the page.
    #[test]
    fn sections_are_well_formed() {
        let mut ids = HashSet::new();
        for page in Page::ALL {
            assert!(ids.insert(page.id()), "duplicate section id: {}", page.id());
            assert!(!page.title().is_empty());
            assert!(!page.of().is_empty(), "{} has no rows", page.id());
        }
        assert!(Page::ALL
            .iter()
            .flat_map(|page| page.of())
            .any(|row| row.key == "oxa.sandbox.net"));
    }
}
