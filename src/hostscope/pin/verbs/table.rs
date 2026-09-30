//! What `lodi host versions NAME` prints: newest first, one row per version with the day it
//! arrived and whether it is the installed, the pinned or the latest one, then one line to copy.
//! There is no prompt and no picker.

use super::cache::Arrival;
use crate::diag::Diagnostic;

/// The table for `name`. `offered` is in any order; it is printed newest arrival first, and the
/// newest is `latest`. The line to copy names the newest version.
pub fn render(
    name: &str,
    offered: &[Arrival],
    installed: Option<&str>,
    pinned: Option<&str>,
) -> String {
    let rows: Vec<Row> = offered
        .iter()
        .map(|a| Row {
            version: a.version.clone(),
            at: Some(a.arrived),
        })
        .collect();
    let latest = newest(&rows).map(|r| r.version.clone());
    listing(
        "arrived",
        &rows,
        installed,
        pinned,
        latest
            .map(|v| format!("lodi host pin {name} --to {v}"))
            .as_deref(),
    )
}

/// One row of a listing: a version, and the instant it arrived or was served, when known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub version: String,
    pub at: Option<i64>,
}

fn newest(rows: &[Row]) -> Option<&Row> {
    sorted(rows).into_iter().next()
}

fn sorted(rows: &[Row]) -> Vec<&Row> {
    let mut rows: Vec<&Row> = rows.iter().collect();
    rows.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.version.cmp(&b.version)));
    rows
}

/// What `lodi host versions NAME` prints: newest first, a row with no day last, the first row
/// `latest`, then `copy` after one blank line. `column` names the day column: `arrived` where
/// the release's interface says when each version was published, `served` where a dated index
/// says only what it served on that day (Debian and Arch, P1).
pub fn listing(
    column: &str,
    rows: &[Row],
    installed: Option<&str>,
    pinned: Option<&str>,
    copy: Option<&str>,
) -> String {
    let rows = sorted(rows);
    let latest = rows.first().map(|r| r.version.as_str());
    let width = rows
        .iter()
        .map(|r| r.version.len())
        .chain(std::iter::once("version".len()))
        .max()
        .unwrap_or(0);
    let mut out = format!("{:width$}  {column:10}  state\n", "version");
    for row in &rows {
        let v = row.version.as_str();
        let states: Vec<&str> = [
            (installed == Some(v), "installed"),
            (pinned == Some(v), "pinned"),
            (latest == Some(v), "latest"),
        ]
        .into_iter()
        .filter_map(|(on, s)| on.then_some(s))
        .collect();
        let day = row.at.map_or_else(
            || "-".to_string(),
            |at| crate::util::format_utc(at)[..10].to_string(),
        );
        out.push_str(format!("{v:width$}  {day:10}  {}", states.join(", ")).trim_end());
        out.push('\n');
    }
    if let Some(copy) = copy {
        out.push_str(&format!("\n{copy}\n"));
    }
    out
}

/// `E_UNKNOWN_PACKAGE` for a name `distro`'s archive does not offer, naming the nearest names
/// among `known` when any is near.
pub fn unknown_package(name: &str, distro: &str, known: &[String]) -> Diagnostic {
    let d = Diagnostic::new(
        "E_UNKNOWN_PACKAGE",
        format!("the {distro} archive offers no package `{name}`"),
    );
    let near = crate::tools::nearest(name, known);
    match near.as_slice() {
        [] => d,
        [one] => d.hint(format!("did you mean `{one}`?")),
        many => d.hint(format!(
            "did you mean {}?",
            many.iter()
                .take(3)
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}
