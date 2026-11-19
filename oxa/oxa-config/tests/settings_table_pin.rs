//! Pins the settings table: the full `schema()` output and the descriptor keys per group in
//! display order. The expected text is `tests/fixtures/settings_table_pin.txt`, generated from the
//! table as it stood before the descriptor files became the only table (consolidation item e1).
//! Adding a setting means adding its line to the fixture; nothing else may change it.

use oxa_config::{in_group, schema, Group};

fn expected() -> Vec<&'static str> {
    include_str!("fixtures/settings_table_pin.txt")
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .collect()
}

fn actual() -> Vec<String> {
    let mut out: Vec<String> = schema()
        .iter()
        .map(|d| format!("{}|{:?}|{}|{}", d.key, d.kind, d.default, d.supported))
        .collect();
    for g in Group::ALL {
        out.extend(in_group(g).map(|d| format!("{g:?}|{}", d.key)));
    }
    out
}

#[test]
fn schema_and_descriptor_order_match_the_pinned_fixture() {
    let want = expected();
    let got = actual();
    for (i, (w, g)) in want.iter().zip(got.iter()).enumerate() {
        assert_eq!(w, g, "line {i} differs");
    }
    assert_eq!(want.len(), got.len(), "row count differs");
}
