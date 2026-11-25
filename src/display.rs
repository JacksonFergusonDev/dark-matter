use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Write};

use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::presets::UTF8_FULL_CONDENSED;
use comfy_table::{Attribute, Cell, CellAlignment, Color, Table};
use owo_colors::OwoColorize;

use crate::model::{BloatRow, CleanupRow, DiffRow, ExplainRow, OrphanRow, ReclaimResult, SortKey};

// Decimal units, matching Finder and Disk Utility on macOS: (unit, scale, decimals, 10^decimals).
const BYTE_UNITS: [(&str, f64, usize, f64); 3] = [
    ("KB", 1e3, 1, 10.0),
    ("MB", 1e6, 1, 10.0),
    ("GB", 1e9, 2, 100.0),
];

pub fn stdout_supports_color() -> bool {
    io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

pub fn stderr_supports_color() -> bool {
    io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

/// Format a byte count in decimal units (`1 MB = 1,000,000 bytes`).
///
/// The unit is chosen after rounding so values like `999,999` format as
/// `"1.0 MB"` rather than `"1000.0 KB"`.
pub fn format_bytes(size: f64) -> String {
    let rounded = size.round();
    if rounded < 1000.0 {
        let clean = if rounded == 0.0 { 0.0 } else { size };
        return format!("{clean:.0} B");
    }
    for (unit, scale, prec, factor) in BYTE_UNITS {
        let val = (size / scale * factor).round() / factor;
        if val < 1000.0 {
            return format!("{val:.prec$} {unit}");
        }
    }
    format!("{:.2} TB", size / 1e12)
}

/// Format a signed byte delta, red for growth, green for shrinkage, and dim for zero.
pub fn format_delta(size: f64, color: bool) -> String {
    if size.abs().round() == 0.0 {
        return if color {
            format!("{}", "0 B".dimmed())
        } else {
            "0 B".to_string()
        };
    }
    if size > 0.0 {
        let text = format!("+{}", format_bytes(size));
        if color {
            format!("{}", text.red())
        } else {
            text
        }
    } else {
        let text = format!("-{}", format_bytes(-size));
        if color {
            format!("{}", text.green())
        } else {
            text
        }
    }
}

fn build_table(headers: &[&str], right_align_cols: &[usize], color: bool) -> Table {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL_CONDENSED)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .force_no_tty();
    if color {
        table.enforce_styling();
    }

    let header_cells: Vec<Cell> = headers
        .iter()
        .map(|h| {
            let cell = Cell::new(h);
            if color {
                cell.fg(Color::Cyan).add_attribute(Attribute::Bold)
            } else {
                cell
            }
        })
        .collect();
    table.set_header(header_cells);

    for &idx in right_align_cols {
        if let Some(col) = table.column_mut(idx) {
            col.set_cell_alignment(CellAlignment::Right);
        }
    }
    table
}

fn styled_cell(
    text: impl ToString,
    fg: Option<Color>,
    attr: Option<Attribute>,
    color: bool,
) -> Cell {
    let mut cell = Cell::new(text);
    if color {
        if let Some(c) = fg {
            cell = cell.fg(c);
        }
        if let Some(a) = attr {
            cell = cell.add_attribute(a);
        }
    }
    cell
}

fn dim_cell(text: impl ToString, color: bool) -> Cell {
    styled_cell(text, None, Some(Attribute::Dim), color)
}

fn magenta_cell(text: impl ToString, color: bool) -> Cell {
    styled_cell(text, Some(Color::Magenta), None, color)
}

fn delta_cell(size: f64, color: bool) -> Cell {
    let text = format_delta(size, false);
    if size.abs().round() == 0.0 {
        dim_cell(text, color)
    } else if size > 0.0 {
        styled_cell(text, Some(Color::Red), None, color)
    } else {
        styled_cell(text, Some(Color::Green), None, color)
    }
}

fn ratio_cell(ratio: Option<f64>, color: bool) -> Cell {
    match ratio.filter(|v| !v.is_nan()) {
        None => dim_cell("—", color),
        Some(r) if r >= 10.0 => styled_cell(
            format!("{r:.1}x"),
            Some(Color::Red),
            Some(Attribute::Bold),
            color,
        ),
        Some(r) if r >= 3.0 => styled_cell(
            format!("{r:.1}x"),
            Some(Color::Yellow),
            Some(Attribute::Bold),
            color,
        ),
        Some(r) => styled_cell(format!("{r:.1}x"), Some(Color::Green), None, color),
    }
}

fn write_title(w: &mut impl Write, title: &str, color: bool) -> io::Result<()> {
    if color {
        writeln!(w, "{}", title.bold())
    } else {
        writeln!(w, "{title}")
    }
}

fn write_green(w: &mut impl Write, msg: &str, color: bool) -> io::Result<()> {
    if color {
        writeln!(w, "{}", msg.green())
    } else {
        writeln!(w, "{msg}")
    }
}

fn write_dim(w: &mut impl Write, msg: &str, color: bool) -> io::Result<()> {
    if color {
        writeln!(w, "{}", msg.dimmed())
    } else {
        writeln!(w, "{msg}")
    }
}

fn write_total_footer(
    w: &mut impl Write,
    total_bytes: f64,
    extra: &str,
    hint: &str,
    color: bool,
) -> io::Result<()> {
    let formatted = format_bytes(total_bytes);
    if color {
        writeln!(
            w,
            "{} {formatted}{extra} {}",
            "Total:".bold(),
            hint.dimmed()
        )
    } else {
        writeln!(w, "Total: {formatted}{extra} {hint}")
    }
}

/// Render the bloat analysis leaderboard to stdout.
pub fn render_bloat_table(
    rows: &[BloatRow],
    sort_by: SortKey,
    top_n: usize,
    fractional: bool,
    is_theoretical: bool,
    roots_only: bool,
) {
    let mut out = io::stdout().lock();
    let _ = render_bloat_table_to(
        &mut out,
        rows,
        sort_by,
        top_n,
        fractional,
        is_theoretical,
        roots_only,
        stdout_supports_color(),
    );
}

/// Render the bloat analysis leaderboard to an arbitrary writer.
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
pub fn render_bloat_table_to(
    w: &mut impl Write,
    rows: &[BloatRow],
    sort_by: SortKey,
    top_n: usize,
    fractional: bool,
    is_theoretical: bool,
    roots_only: bool,
    color: bool,
) -> io::Result<()> {
    if rows.is_empty() {
        let msg = "No package data available to render.";
        return if color {
            writeln!(w, "{}", msg.red().bold())
        } else {
            writeln!(w, "{msg}")
        };
    }

    // Only roots' weighted sizes add up to the total; hide intermediate deps by default.
    let mut filtered: Vec<&BloatRow> = if roots_only {
        rows.iter().filter(|r| r.is_root).collect()
    } else {
        rows.iter().collect()
    };
    let hidden = if roots_only {
        rows.len().saturating_sub(filtered.len())
    } else {
        0
    };

    filtered.sort_by(|a, b| match sort_by {
        SortKey::Ratio => {
            let ra = a.bloat_ratio.filter(|v| !v.is_nan());
            let rb = b.bloat_ratio.filter(|v| !v.is_nan());
            match (ra, rb) {
                (Some(va), Some(vb)) => vb.total_cmp(&va).then_with(|| a.package.cmp(&b.package)),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => a.package.cmp(&b.package),
            }
        }
        SortKey::Core => b
            .core_bytes
            .cmp(&a.core_bytes)
            .then_with(|| a.package.cmp(&b.package)),
        SortKey::Recursive => {
            if fractional {
                b.weighted_bytes
                    .total_cmp(&a.weighted_bytes)
                    .then_with(|| a.package.cmp(&b.package))
            } else {
                b.standard_bytes
                    .cmp(&a.standard_bytes)
                    .then_with(|| a.package.cmp(&b.package))
            }
        }
    });
    filtered.truncate(top_n);

    let core_label = if is_theoretical {
        "Archive Size"
    } else {
        "Core Size"
    };
    let rec_label = match (fractional, is_theoretical) {
        (true, true) => "Theoretical Rec. Size",
        (true, false) => "Weighted Rec. Size",
        (false, true) => "Theoretical Std. Size",
        (false, false) => "Standard Rec. Size",
    };

    let mut table = build_table(
        &["Package", core_label, rec_label, "Bloat Ratio", "Deps"],
        &[1, 2, 3, 4],
        color,
    );

    for row in filtered {
        let name = if row.is_root {
            row.package.clone()
        } else {
            format!("{} · dependency", row.package)
        };
        let rec_bytes = if fractional {
            row.weighted_bytes
        } else {
            row.standard_bytes as f64
        };
        table.add_row(vec![
            Cell::new(name),
            dim_cell(format_bytes(row.core_bytes as f64), color),
            magenta_cell(format_bytes(rec_bytes), color),
            ratio_cell(row.bloat_ratio, color),
            dim_cell(row.dep_count, color),
        ]);
    }

    write_title(w, "Dark Matter: Homebrew Bloat Analysis", color)?;
    writeln!(w, "{table}")?;

    if hidden > 0 {
        let noun = if hidden == 1 {
            "dependency"
        } else {
            "dependencies"
        };
        let caption = format!(
            "{hidden} {noun} not shown: each one's size is shared among the packages that need it. List them with --all."
        );
        write_dim(w, &caption, color)?;
    }
    Ok(())
}

/// Render the dependency breakdown table for a specific package to stdout.
pub fn render_explain_table(rows: &[ExplainRow], target: &str, is_theoretical: bool) {
    let mut out = io::stdout().lock();
    let _ = render_explain_table_to(
        &mut out,
        rows,
        target,
        is_theoretical,
        stdout_supports_color(),
    );
}

/// Render the dependency breakdown table for a specific package to an arbitrary writer.
pub fn render_explain_table_to(
    w: &mut impl Write,
    rows: &[ExplainRow],
    target: &str,
    is_theoretical: bool,
    color: bool,
) -> io::Result<()> {
    if rows.is_empty() {
        return write_green(
            w,
            &format!("'{target}' has no transitive dependencies."),
            color,
        );
    }

    let size_label = if is_theoretical {
        "Archive Size"
    } else {
        "Core Size"
    };
    let mut table = build_table(
        &["Dependency", size_label, "Shared By", "Attributed Size"],
        &[1, 2, 3],
        color,
    );

    for row in rows {
        let shared = if row.is_root {
            "requested".to_string()
        } else {
            row.shared_by.to_string()
        };
        table.add_row(vec![
            Cell::new(&row.dependency),
            dim_cell(format_bytes(row.size_bytes as f64), color),
            dim_cell(shared, color),
            magenta_cell(format_bytes(row.attributed_bytes), color),
        ]);
    }

    write_title(w, &format!("Dependency Breakdown: {target}"), color)?;
    writeln!(w, "{table}")
}

/// Render a target's dependencies as a tree with attributed sizes to stdout.
pub fn render_explain_tree(rows: &[ExplainRow], target: &str, is_theoretical: bool) {
    let mut out = io::stdout().lock();
    let _ = render_explain_tree_to(
        &mut out,
        rows,
        target,
        is_theoretical,
        stdout_supports_color(),
    );
}

/// Render a target's dependencies as a tree with attributed sizes to an arbitrary writer.
///
/// A dependency reached by several paths is expanded the first time it appears
/// and marked `(shown above)` on subsequent visits.
pub fn render_explain_tree_to(
    w: &mut impl Write,
    rows: &[ExplainRow],
    target: &str,
    _is_theoretical: bool,
    color: bool,
) -> io::Result<()> {
    if rows.is_empty() {
        return write_green(
            w,
            &format!("'{target}' has no transitive dependencies."),
            color,
        );
    }

    let row_map: HashMap<&str, &ExplainRow> =
        rows.iter().map(|r| (r.dependency.as_str(), r)).collect();
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for row in rows {
        for parent in &row.required_by {
            children
                .entry(parent.as_str())
                .or_default()
                .push(row.dependency.as_str());
        }
    }
    for kids in children.values_mut() {
        kids.sort_by(|&a, &b| {
            let attr_a = row_map.get(a).map_or(0.0, |r| r.attributed_bytes);
            let attr_b = row_map.get(b).map_or(0.0, |r| r.attributed_bytes);
            attr_b.total_cmp(&attr_a).then_with(|| a.cmp(b))
        });
        kids.dedup();
    }

    let total: f64 = rows.iter().map(|r| r.attributed_bytes).sum();
    let total_str = format_bytes(total);
    if color {
        let suffix = format!("· {total_str} attributed");
        writeln!(w, "{} {}", target.bold(), suffix.dimmed())?;
    } else {
        writeln!(w, "{target} · {total_str} attributed")?;
    }

    let mut seen: HashSet<&str> = HashSet::new();
    if let Some(top_kids) = children.get(target) {
        let count = top_kids.len();
        for (idx, &child) in top_kids.iter().enumerate() {
            write_tree_branch(
                w,
                child,
                "",
                idx + 1 == count,
                &row_map,
                &children,
                &mut seen,
                color,
            )?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_tree_branch<'a>(
    w: &mut impl Write,
    name: &'a str,
    prefix: &str,
    is_last: bool,
    row_map: &HashMap<&'a str, &'a ExplainRow>,
    children: &HashMap<&'a str, Vec<&'a str>>,
    seen: &mut HashSet<&'a str>,
    color: bool,
) -> io::Result<()> {
    let connector = if is_last { "└── " } else { "├── " };

    if !seen.insert(name) {
        let repeat = format!("{name} (shown above)");
        return if color {
            writeln!(w, "{prefix}{connector}{}", repeat.dimmed())
        } else {
            writeln!(w, "{prefix}{connector}{repeat}")
        };
    }

    let label = match row_map.get(name) {
        Some(row) => format_tree_node_label(name, row, color),
        None => name.to_string(),
    };
    writeln!(w, "{prefix}{connector}{label}")?;

    if let Some(kids) = children.get(name) {
        let child_prefix = format!("{prefix}{}", if is_last { "    " } else { "│   " });
        let count = kids.len();
        for (idx, &child) in kids.iter().enumerate() {
            write_tree_branch(
                w,
                child,
                &child_prefix,
                idx + 1 == count,
                row_map,
                children,
                seen,
                color,
            )?;
        }
    }
    Ok(())
}

fn format_tree_node_label(name: &str, row: &ExplainRow, color: bool) -> String {
    let size = format_bytes(row.size_bytes as f64);
    if row.is_root {
        let detail = format!("{size} · requested");
        return if color {
            format!("{name} {}", detail.dimmed())
        } else {
            format!("{name} {detail}")
        };
    }

    let share = format_bytes(row.attributed_bytes);
    if row.shared_by > 1 {
        let detail = format!("of {size}, shared by {}", row.shared_by);
        if color {
            format!("{name} {} {}", share.magenta(), detail.dimmed())
        } else {
            format!("{name} {share} {detail}")
        }
    } else if color {
        format!("{name} {}", share.magenta())
    } else {
        format!("{name} {share}")
    }
}

/// Render what uninstalling the requested targets would free to stdout.
pub fn render_reclaim(result: &ReclaimResult, targets: &[String]) {
    let mut out = io::stdout().lock();
    let _ = render_reclaim_to(&mut out, result, targets, stdout_supports_color());
}

/// Render what uninstalling the requested targets would free to an arbitrary writer.
pub fn render_reclaim_to(
    w: &mut impl Write,
    result: &ReclaimResult,
    targets: &[String],
    color: bool,
) -> io::Result<()> {
    for (target, dependents) in &result.blocked {
        let prefix = format!("'{target}' can't be uninstalled:");
        let deps = dependents.join(", ");
        if color {
            writeln!(w, "{} required by {deps}.", prefix.yellow().bold())?;
        } else {
            writeln!(w, "{prefix} required by {deps}.")?;
        }
    }

    if result.freed.is_empty() {
        return write_dim(w, "Nothing would be freed.", color);
    }

    let mut table = build_table(&["Package", "Size", "Removed As"], &[1], color);
    let mut total: u64 = 0;
    for row in &result.freed {
        total += row.core_bytes;
        let role = if row.is_target {
            "requested"
        } else {
            "dependency"
        };
        table.add_row(vec![
            Cell::new(&row.package),
            magenta_cell(format_bytes(row.core_bytes as f64), color),
            dim_cell(role, color),
        ]);
    }

    let title = format!("Reclaimable by removing {}", targets.join(", "));
    write_title(w, &title, color)?;
    writeln!(w, "{table}")?;

    let extra = format!(" across {} packages", result.freed.len());
    write_total_footer(
        w,
        total as f64,
        &extra,
        "(brew uninstall, then brew autoremove)",
        color,
    )
}

/// Render orphaned dependencies removable by `brew autoremove` to stdout.
pub fn render_orphans(rows: &[OrphanRow]) {
    let mut out = io::stdout().lock();
    let _ = render_orphans_to(&mut out, rows, stdout_supports_color());
}

/// Render orphaned dependencies removable by `brew autoremove` to an arbitrary writer.
pub fn render_orphans_to(w: &mut impl Write, rows: &[OrphanRow], color: bool) -> io::Result<()> {
    if rows.is_empty() {
        return write_green(w, "No orphaned dependencies.", color);
    }

    let mut table = build_table(&["Package", "Size", "Needed By"], &[1], color);
    let mut total: u64 = 0;
    for row in rows {
        total += row.core_bytes;
        let needed_by = if row.required_by.is_empty() {
            "—".to_string()
        } else {
            row.required_by.join(", ")
        };
        table.add_row(vec![
            Cell::new(&row.package),
            magenta_cell(format_bytes(row.core_bytes as f64), color),
            dim_cell(needed_by, color),
        ]);
    }

    write_title(w, "Orphaned Dependencies", color)?;
    writeln!(w, "{table}")?;
    write_total_footer(w, total as f64, "", "(reclaim with brew autoremove)", color)
}

/// Render older Cellar kegs removable by `brew cleanup` to stdout.
pub fn render_cleanup(rows: &[CleanupRow]) {
    let mut out = io::stdout().lock();
    let _ = render_cleanup_to(&mut out, rows, stdout_supports_color());
}

/// Render older Cellar kegs removable by `brew cleanup` to an arbitrary writer.
pub fn render_cleanup_to(w: &mut impl Write, rows: &[CleanupRow], color: bool) -> io::Result<()> {
    if rows.is_empty() {
        return write_green(w, "No old versions in the Cellar.", color);
    }

    let mut table = build_table(&["Package", "Version", "Size"], &[2], color);
    let mut total: u64 = 0;
    for row in rows {
        total += row.bytes;
        table.add_row(vec![
            Cell::new(&row.package),
            dim_cell(&row.version, color),
            magenta_cell(format_bytes(row.bytes as f64), color),
        ]);
    }

    write_title(w, "Old Versions in the Cellar", color)?;
    writeln!(w, "{table}")?;
    write_total_footer(w, total as f64, "", "(reclaim with brew cleanup)", color)
}

/// Render package differences between two snapshots to stdout.
pub fn render_diff(rows: &[DiffRow], top_n: usize, old_total: f64, new_total: f64) {
    let mut out = io::stdout().lock();
    let _ = render_diff_to(
        &mut out,
        rows,
        top_n,
        old_total,
        new_total,
        stdout_supports_color(),
    );
}

/// Render package differences between two snapshots to an arbitrary writer.
pub fn render_diff_to(
    w: &mut impl Write,
    rows: &[DiffRow],
    top_n: usize,
    old_total: f64,
    new_total: f64,
    color: bool,
) -> io::Result<()> {
    let header_label = "Total core size:";
    let old_str = format_bytes(old_total);
    let new_str = format_bytes(new_total);
    let delta_str = format_delta(new_total - old_total, color);
    if color {
        writeln!(
            w,
            "{} {old_str} → {new_str} ({delta_str})",
            header_label.bold()
        )?;
    } else {
        writeln!(w, "{header_label} {old_str} → {new_str} ({delta_str})")?;
    }

    if rows.is_empty() {
        return write_dim(w, "No packages changed.", color);
    }

    let mut table = build_table(
        &["Package", "Status", "Core Size", "Core Δ", "Weighted Δ"],
        &[2, 3, 4],
        color,
    );

    for row in rows.iter().take(top_n) {
        let core_size = if row.new_core > 0.0 {
            row.new_core
        } else {
            row.old_core
        };
        table.add_row(vec![
            Cell::new(&row.package),
            dim_cell(row.status.as_str(), color),
            dim_cell(format_bytes(core_size), color),
            delta_cell(row.core_delta, color),
            delta_cell(row.weighted_delta, color),
        ]);
    }

    write_title(w, "Snapshot Diff", color)?;
    writeln!(w, "{table}")?;

    if rows.len() > top_n {
        let overflow = format!("…and {} more.", rows.len() - top_n);
        write_dim(w, &overflow, color)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::model::{
        BloatRow, CleanupRow, DiffRow, DiffStatus, ExplainRow, OrphanRow, ReclaimResult,
        ReclaimRow, SortKey,
    };

    use super::*;

    fn bloat_row(
        package: &str,
        is_root: bool,
        core_bytes: u64,
        weighted_bytes: f64,
        standard_bytes: u64,
        bloat_ratio: Option<f64>,
        dep_count: usize,
    ) -> BloatRow {
        BloatRow {
            package: package.to_string(),
            core_bytes,
            standard_bytes,
            weighted_bytes,
            bloat_ratio,
            dep_count,
            is_root,
            is_requested: is_root,
        }
    }

    fn root_rows() -> Vec<BloatRow> {
        vec![
            bloat_row("top-level", true, 10, 20.0, 20, Some(2.0), 1),
            bloat_row("shared-lib", false, 10, 20.0, 20, Some(2.0), 1),
        ]
    }

    #[test]
    fn test_format_bytes() {
        let cases = [
            (0.0, "0 B"),
            (-0.0, "0 B"),
            (500.0, "500 B"),
            (999.4, "999 B"),
            // Units are chosen after rounding, so nothing reads "1000 <unit>".
            (999.6, "1.0 KB"),
            (1000.0, "1.0 KB"),
            (999_949.0, "999.9 KB"),
            (999_999.0, "1.0 MB"),
            (2_500_000.0, "2.5 MB"),
            (999_990_000.0, "1.00 GB"),
            (4_123_000_000.0, "4.12 GB"),
            (2_500_000_000_000.0, "2.50 TB"),
        ];
        for (size, expected) in cases {
            assert_eq!(format_bytes(size), expected, "size={size}");
        }
    }

    #[test]
    fn test_format_delta_plain_and_colored() {
        assert_eq!(format_delta(2000.0, false), "+2.0 KB");
        assert_eq!(format_delta(-512.0, false), "-512 B");
        assert_eq!(format_delta(0.0, false), "0 B");
        assert_eq!(format_delta(-0.0, false), "0 B");

        let pos = format_delta(2000.0, true);
        assert!(pos.contains("\x1b[31m") && pos.contains("+2.0 KB"));
        let neg = format_delta(-512.0, true);
        assert!(neg.contains("\x1b[32m") && neg.contains("-512 B"));
        let zero = format_delta(0.0, true);
        assert!(zero.contains("\x1b[2m") && zero.contains("0 B"));
    }

    #[test]
    fn test_render_bloat_table_empty() {
        let mut buf = Vec::new();
        render_bloat_table_to(&mut buf, &[], SortKey::Ratio, 20, true, false, true, false).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("No package data available to render."));
    }

    #[test]
    fn test_render_bloat_table_with_severe_ratios() {
        let rows = vec![
            bloat_row("bloated-pkg", true, 100, 1500.0, 1500, Some(15.0), 5),
            bloat_row("clean-pkg", true, 100, 110.0, 110, Some(1.1), 0),
        ];
        let mut buf = Vec::new();
        render_bloat_table_to(
            &mut buf,
            &rows,
            SortKey::Ratio,
            20,
            true,
            false,
            true,
            false,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("Dark Matter: Homebrew Bloat Analysis"));
        assert!(out.contains("bloated-pkg"));
        assert!(out.contains("15.0x"));
        assert!(out.contains("clean-pkg"));
        assert!(out.contains("1.1x"));
    }

    #[test]
    fn test_render_bloat_table_filters_non_roots_by_default() {
        let rows = root_rows();
        let mut buf = Vec::new();
        render_bloat_table_to(
            &mut buf,
            &rows,
            SortKey::Ratio,
            20,
            true,
            false,
            true,
            false,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("top-level"));
        assert!(!out.contains("shared-lib"));
    }

    #[test]
    fn test_render_bloat_table_keeps_requested_targets() {
        let rows = root_rows();
        let mut buf = Vec::new();
        render_bloat_table_to(
            &mut buf,
            &rows,
            SortKey::Ratio,
            20,
            true,
            false,
            false,
            false,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("top-level"));
        assert!(out.contains("shared-lib · dependency"));
    }

    #[test]
    fn test_render_bloat_table_captions_hidden_dependencies() {
        let rows = root_rows();
        let mut buf_roots = Vec::new();
        render_bloat_table_to(
            &mut buf_roots,
            &rows,
            SortKey::Ratio,
            20,
            true,
            false,
            true,
            false,
        )
        .unwrap();
        let out_roots = String::from_utf8(buf_roots).unwrap();
        assert!(out_roots.contains(
            "1 dependency not shown: each one's size is shared among the packages that need it. List them with --all."
        ));

        let mut buf_all = Vec::new();
        render_bloat_table_to(
            &mut buf_all,
            &rows,
            SortKey::Ratio,
            20,
            true,
            false,
            false,
            false,
        )
        .unwrap();
        let out_all = String::from_utf8(buf_all).unwrap();
        assert!(!out_all.contains("not shown"));

        let multi_deps = vec![
            bloat_row("top", true, 10, 20.0, 20, Some(2.0), 2),
            bloat_row("dep-a", false, 5, 5.0, 5, Some(1.0), 0),
            bloat_row("dep-b", false, 5, 5.0, 5, Some(1.0), 0),
        ];
        let mut buf_multi = Vec::new();
        render_bloat_table_to(
            &mut buf_multi,
            &multi_deps,
            SortKey::Ratio,
            20,
            true,
            false,
            true,
            false,
        )
        .unwrap();
        assert!(
            String::from_utf8(buf_multi)
                .unwrap()
                .contains("2 dependencies not shown")
        );
    }

    #[test]
    fn test_render_bloat_table_standard_mode_and_severity() {
        let rows = vec![
            bloat_row("red", true, 1000, 1.0, 2000, Some(12.0), 1),
            bloat_row("yellow", true, 1000, 1.0, 2000, Some(4.0), 1),
            bloat_row("green", true, 1000, 1.0, 2000, Some(1.5), 1),
        ];
        let mut buf = Vec::new();
        render_bloat_table_to(&mut buf, &rows, SortKey::Ratio, 20, false, true, true, true)
            .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("Archive Size"));
        assert!(out.contains("Theoretical Std. Size"));
        assert!(out.contains("2.0 KB"));
        assert!(out.contains("12.0x"));
        assert!(out.contains("4.0x"));
        assert!(out.contains("1.5x"));
        // ANSI escapes present for bold red, bold yellow, green.
        assert!(out.contains("\x1b["));
    }

    #[test]
    fn test_render_bloat_table_undefined_ratio_sorts_last() {
        let rows = vec![
            bloat_row("meta", true, 0, 100.0, 100, None, 1),
            bloat_row("lean", true, 100, 100.0, 100, Some(1.0), 1),
            bloat_row("heavy", true, 10, 100.0, 100, Some(10.0), 1),
        ];
        let mut buf = Vec::new();
        render_bloat_table_to(
            &mut buf,
            &rows,
            SortKey::Ratio,
            20,
            true,
            false,
            true,
            false,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        let pos_heavy = out.find("heavy").unwrap();
        let pos_lean = out.find("lean").unwrap();
        let pos_meta = out.find("meta").unwrap();
        assert!(pos_heavy < pos_lean);
        assert!(pos_lean < pos_meta);
        assert!(out.contains("—"));
    }

    #[test]
    fn test_render_bloat_table_sort_keys_and_top_n() {
        let rows = vec![
            bloat_row("alpha", true, 5000, 6000.0, 9000, Some(1.2), 1),
            bloat_row("beta", true, 1000, 8000.0, 7000, Some(8.0), 2),
        ];
        let mut core_buf = Vec::new();
        render_bloat_table_to(
            &mut core_buf,
            &rows,
            SortKey::Core,
            1,
            true,
            false,
            true,
            false,
        )
        .unwrap();
        let core_out = String::from_utf8(core_buf).unwrap();
        assert!(core_out.contains("alpha"));
        assert!(!core_out.contains("beta"));

        let mut rec_buf = Vec::new();
        render_bloat_table_to(
            &mut rec_buf,
            &rows,
            SortKey::Recursive,
            1,
            true,
            false,
            true,
            false,
        )
        .unwrap();
        let rec_out = String::from_utf8(rec_buf).unwrap();
        assert!(rec_out.contains("beta"));
        assert!(!rec_out.contains("alpha"));
        assert!(rec_out.contains("Weighted Rec. Size"));
    }

    #[test]
    fn test_render_explain_table_empty() {
        let mut buf = Vec::new();
        render_explain_table_to(&mut buf, &[], "leaf-node", false, false).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("'leaf-node' has no transitive dependencies."));
    }

    #[test]
    fn test_render_explain_table_modes() {
        let rows = vec![ExplainRow {
            dependency: "openssl".to_string(),
            size_bytes: 2000,
            shared_by: 4,
            attributed_bytes: 512.0,
            is_root: false,
            required_by: vec!["curl".to_string()],
        }];

        for (is_theoretical, expected_header) in [(true, "Archive Size"), (false, "Core Size")] {
            let mut buf = Vec::new();
            render_explain_table_to(&mut buf, &rows, "curl", is_theoretical, false).unwrap();
            let out = String::from_utf8(buf).unwrap();
            assert!(out.contains("Dependency Breakdown: curl"));
            assert!(out.contains(expected_header));
            assert!(out.contains("openssl"));
            assert!(out.contains("2.0 KB"));
            assert!(out.contains("512 B"));
        }
    }

    #[test]
    fn test_render_explain_table_marks_requested_dependencies() {
        let rows = vec![ExplainRow {
            dependency: "python".to_string(),
            size_bytes: 2000,
            shared_by: 0,
            attributed_bytes: 0.0,
            is_root: true,
            required_by: vec!["x".to_string()],
        }];
        let mut buf = Vec::new();
        render_explain_table_to(&mut buf, &rows, "x", false, false).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("python"));
        assert!(out.contains("2.0 KB"));
        assert!(out.contains("requested"));
        assert!(out.contains("0 B"));
    }

    #[test]
    fn test_render_explain_tree() {
        let rows = vec![
            ExplainRow {
                dependency: "python".to_string(),
                size_bytes: 4000,
                shared_by: 0,
                attributed_bytes: 0.0,
                is_root: true,
                required_by: vec!["app".to_string()],
            },
            ExplainRow {
                dependency: "openssl".to_string(),
                size_bytes: 2000,
                shared_by: 2,
                attributed_bytes: 1000.0,
                is_root: false,
                required_by: vec!["app".to_string(), "python".to_string()],
            },
            ExplainRow {
                dependency: "zlib".to_string(),
                size_bytes: 1000,
                shared_by: 1,
                attributed_bytes: 1000.0,
                is_root: false,
                required_by: vec!["openssl".to_string()],
            },
        ];

        let mut buf = Vec::new();
        render_explain_tree_to(&mut buf, &rows, "app", false, false).unwrap();
        let out = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = out.lines().collect();

        assert!(lines[0].contains("app"));
        assert!(lines[0].contains("2.0 KB attributed"));
        assert!(out.contains("├── openssl 1.0 KB of 2.0 KB, shared by 2"));
        assert!(out.contains("│   └── zlib 1.0 KB"));
        assert!(out.contains("└── python 4.0 KB · requested"));
        assert!(out.contains("    └── openssl (shown above)"));
    }

    #[test]
    fn test_render_explain_tree_empty() {
        let mut buf = Vec::new();
        render_explain_tree_to(&mut buf, &[], "x", false, false).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("no transitive dependencies"));
    }

    #[test]
    fn test_render_reclaim() {
        let mut blocked = BTreeMap::new();
        blocked.insert("lib".to_string(), vec!["x".to_string(), "y".to_string()]);
        let result = ReclaimResult {
            freed: vec![
                ReclaimRow {
                    package: "app".to_string(),
                    core_bytes: 2000,
                    is_target: true,
                },
                ReclaimRow {
                    package: "dep".to_string(),
                    core_bytes: 1000,
                    is_target: false,
                },
            ],
            blocked,
        };
        let targets = vec!["app".to_string(), "lib".to_string()];

        let mut buf = Vec::new();
        render_reclaim_to(&mut buf, &result, &targets, false).unwrap();
        let out = String::from_utf8(buf).unwrap();

        assert!(out.contains("'lib' can't be uninstalled: required by x, y."));
        assert!(out.contains("Reclaimable by removing app, lib"));
        assert!(out.contains("requested"));
        assert!(out.contains("dependency"));
        assert!(
            out.contains("Total: 3.0 KB across 2 packages (brew uninstall, then brew autoremove)")
        );
    }

    #[test]
    fn test_render_reclaim_nothing() {
        let result = ReclaimResult::default();
        let mut buf = Vec::new();
        render_reclaim_to(&mut buf, &result, &["x".to_string()], false).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("Nothing would be freed."));
    }

    #[test]
    fn test_render_orphans() {
        let rows = vec![
            OrphanRow {
                package: "p".to_string(),
                core_bytes: 1000,
                required_by: vec!["o".to_string()],
            },
            OrphanRow {
                package: "o".to_string(),
                core_bytes: 1000,
                required_by: vec![],
            },
        ];
        let mut buf = Vec::new();
        render_orphans_to(&mut buf, &rows, false).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("Orphaned Dependencies"));
        assert!(out.contains("—"));
        assert!(out.contains("Total: 2.0 KB (reclaim with brew autoremove)"));
    }

    #[test]
    fn test_render_cleanup() {
        let rows = vec![CleanupRow {
            package: "a".to_string(),
            version: "1.0".to_string(),
            bytes: 2000,
        }];
        let mut buf = Vec::new();
        render_cleanup_to(&mut buf, &rows, false).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("Old Versions in the Cellar"));
        assert!(out.contains("1.0"));
        assert!(out.contains("Total: 2.0 KB (reclaim with brew cleanup)"));
    }

    #[test]
    fn test_empty_reports() {
        let mut orphans_buf = Vec::new();
        render_orphans_to(&mut orphans_buf, &[], false).unwrap();
        assert!(
            String::from_utf8(orphans_buf)
                .unwrap()
                .contains("No orphaned dependencies.")
        );

        let mut cleanup_buf = Vec::new();
        render_cleanup_to(&mut cleanup_buf, &[], false).unwrap();
        assert!(
            String::from_utf8(cleanup_buf)
                .unwrap()
                .contains("No old versions in the Cellar.")
        );
    }

    #[test]
    fn test_render_diff() {
        let rows = vec![
            DiffRow {
                package: "grew".to_string(),
                status: DiffStatus::Changed,
                old_core: 1000.0,
                new_core: 3000.0,
                core_delta: 2000.0,
                weighted_delta: 2000.0,
            },
            DiffRow {
                package: "gone".to_string(),
                status: DiffStatus::Removed,
                old_core: 1000.0,
                new_core: 0.0,
                core_delta: -1000.0,
                weighted_delta: -1000.0,
            },
        ];

        let mut buf = Vec::new();
        render_diff_to(&mut buf, &rows, 1, 2000.0, 3000.0, false).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("Total core size: 2.0 KB → 3.0 KB (+1.0 KB)"));
        assert!(out.contains("Snapshot Diff"));
        assert!(out.contains("grew"));
        assert!(out.contains("changed"));
        assert!(out.contains("3.0 KB"));
        assert!(out.contains("+2.0 KB"));
        assert!(!out.contains("gone"));
        assert!(out.contains("…and 1 more."));

        // Also verify removed packages fall back to old_core when shown.
        let mut all_buf = Vec::new();
        render_diff_to(&mut all_buf, &rows, 5, 2000.0, 3000.0, false).unwrap();
        let all_out = String::from_utf8(all_buf).unwrap();
        assert!(all_out.contains("gone"));
        assert!(all_out.contains("removed"));
        assert!(all_out.contains("-1.0 KB"));
        assert!(!all_out.contains("…and"));
    }

    #[test]
    fn test_render_diff_no_changes() {
        let mut buf = Vec::new();
        render_diff_to(&mut buf, &[], 5, 10.0, 10.0, false).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("Total core size: 10 B → 10 B (0 B)"));
        assert!(out.contains("No packages changed."));
    }

    #[test]
    fn test_color_outputs_include_ansi_codes() {
        let mut tree_buf = Vec::new();
        let explain_rows = vec![ExplainRow {
            dependency: "zlib".to_string(),
            size_bytes: 1000,
            shared_by: 1,
            attributed_bytes: 1000.0,
            is_root: false,
            required_by: vec!["app".to_string()],
        }];
        render_explain_tree_to(&mut tree_buf, &explain_rows, "app", false, true).unwrap();
        let tree_out = String::from_utf8(tree_buf).unwrap();
        assert!(tree_out.contains("\x1b["));

        let mut diff_buf = Vec::new();
        let diff_rows = vec![DiffRow {
            package: "grew".to_string(),
            status: DiffStatus::Changed,
            old_core: 1000.0,
            new_core: 3000.0,
            core_delta: 2000.0,
            weighted_delta: 2000.0,
        }];
        render_diff_to(&mut diff_buf, &diff_rows, 5, 1000.0, 3000.0, true).unwrap();
        let diff_out = String::from_utf8(diff_buf).unwrap();
        assert!(diff_out.contains("\x1b[31m+2.0 KB\x1b[39m"));
    }
}
