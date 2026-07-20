//! Hard bounds for the user-authored role catalog exposed in the spawn tool schema.

pub(super) const MAX_ROLE_CATALOG_BYTES: usize = 4_000;
pub(super) const MAX_ROLE_CATALOG_ENTRIES: usize = 32;
pub(super) const MAX_ROLE_CATALOG_ENTRY_BYTES: usize = 2_000;
/// Code Mode renders each non-empty schema-description line with a five-byte comment prefix.
pub(super) const MAX_ROLE_CATALOG_RENDERED_LINES: usize = 64;

const ROLE_CATALOG_HEADER: &str = "Available roles:";
const ROLE_CATALOG_OMISSION_MARKER: &str = "... [additional roles omitted]";
const ROLE_CATALOG_ENTRY_OMISSION_MARKER: &str = "\n... [entry truncated]";

pub(super) fn bound_role_catalog(role_entries: impl IntoIterator<Item = String>) -> String {
    let mut entries = Vec::new();
    let mut catalog_bytes = ROLE_CATALOG_HEADER.len();
    let mut catalog_lines = rendered_line_count(ROLE_CATALOG_HEADER);
    let mut role_entries = role_entries.into_iter();
    let mut omitted = false;

    while entries.len() < MAX_ROLE_CATALOG_ENTRIES {
        let Some(entry) = role_entries.next() else {
            break;
        };
        let entry = truncate_catalog_entry(entry);
        let entry_lines = rendered_line_count(&entry);
        if catalog_bytes + 1 + entry.len() > MAX_ROLE_CATALOG_BYTES
            || catalog_lines + entry_lines > MAX_ROLE_CATALOG_RENDERED_LINES
        {
            omitted = true;
            break;
        }
        catalog_bytes += 1 + entry.len();
        catalog_lines += entry_lines;
        entries.push(entry);
    }
    if !omitted && role_entries.next().is_some() {
        omitted = true;
    }

    if omitted {
        let marker_lines = rendered_line_count(ROLE_CATALOG_OMISSION_MARKER);
        while catalog_bytes + 1 + ROLE_CATALOG_OMISSION_MARKER.len() > MAX_ROLE_CATALOG_BYTES
            || catalog_lines + marker_lines > MAX_ROLE_CATALOG_RENDERED_LINES
        {
            let Some(entry) = entries.pop() else {
                break;
            };
            catalog_bytes -= 1 + entry.len();
            catalog_lines -= rendered_line_count(&entry);
        }
    }

    let mut catalog = String::with_capacity(MAX_ROLE_CATALOG_BYTES.min(catalog_bytes));
    catalog.push_str(ROLE_CATALOG_HEADER);
    for entry in entries {
        catalog.push('\n');
        catalog.push_str(&entry);
    }
    if omitted {
        catalog.push('\n');
        catalog.push_str(ROLE_CATALOG_OMISSION_MARKER);
    }
    catalog
}

fn rendered_line_count(text: &str) -> usize {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .count()
}

fn truncate_catalog_entry(mut entry: String) -> String {
    if entry.len() <= MAX_ROLE_CATALOG_ENTRY_BYTES {
        return entry;
    }

    let mut retained_bytes =
        MAX_ROLE_CATALOG_ENTRY_BYTES - ROLE_CATALOG_ENTRY_OMISSION_MARKER.len();
    while !entry.is_char_boundary(retained_bytes) {
        retained_bytes -= 1;
    }
    entry.truncate(retained_bytes);
    entry.push_str(ROLE_CATALOG_ENTRY_OMISSION_MARKER);
    entry
}

#[cfg(test)]
#[path = "role_catalog_bounds_tests.rs"]
mod tests;
