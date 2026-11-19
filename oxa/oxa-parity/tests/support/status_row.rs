//! #1512 S3: the footer's extension status row. The pi footer draws one row below the footer
//! (`no sandbox · …`, `sandbox`, `YOLO full access`); every footer check skips it to find the
//! footer rows themselves.
use oxa_parity::pty::Screen;

/// `true` for a footer extension status row.
pub fn is_status_row(text: &str) -> bool {
    let t = text.trim();
    t == "sandbox"
        || t.starts_with("no sandbox")
        || t.starts_with("YOLO full access")
        || t.ends_with("YOLO full access")
}

/// The last row that is not an extension status row.
pub fn last_footer_row(s: &Screen) -> u16 {
    let (rows, _) = s.size();
    let mut row = rows - 1;
    while row > 1 && is_status_row(&s.row(row)) {
        row -= 1;
    }
    row
}
