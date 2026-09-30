//! `strategy = "text_index"`: a text document upstream publishes, whose lines carry the
//! versions (M-0.4 T-4).
//!
//! The recipe's `line` is a template with exactly one `${version}`. A line of the fetched
//! document matches when it holds the template's literal prefix and, after it, the template's
//! literal suffix; what lies between them is the version, trimmed. A line that does not match,
//! and a middle that is not a version, are **ignored** — the document is somebody else's file
//! and most of it says nothing about this tool. There is no regular expression here and no
//! strategy may add one (LD-80, design call D5).

use crate::catalogue::Reader;
use crate::diag::Diagnostic;
use crate::fetch::Fetcher;
use crate::upstream::strategy::{AssetUrl, Candidate, Context, Discovery, Strategy};
use crate::version::Version;
use std::collections::BTreeMap;
use toml_edit::TableLike;

pub const STRATEGY: Strategy = Strategy {
    parse,
    discover,
    asset_url: AssetUrl::FromRecipe,
    supplies_digest: false,
};

fn parse(
    r: &Reader,
    versions: &dyn TableLike,
    before_version: &[&str],
) -> Result<Discovery, Diagnostic> {
    r.keys(versions, "versions", &["strategy", "url", "line"])?;
    let url = r.string(versions, "url", "versions")?;
    r.check_template(&url, before_version)?;
    let line = r.string(versions, "line", "versions")?;
    // `${version}` is what the line yields, not something rendered into it; everything else in
    // the template is an architecture variable known before discovery.
    let mut known: Vec<&str> = before_version.to_vec();
    known.push("version");
    r.check_template(&line, &known)?;
    if line.matches("${version}").count() != 1 {
        return Err(r.invalid("`line` in [versions] needs exactly one `${version}`"));
    }
    Ok(Discovery::TextIndex { url, line })
}

/// The version a line carries, by the template's literal prefix and suffix. An empty suffix
/// means "to the end of the line", which is how a document of bare version numbers is read.
pub(crate) fn capture_line(template: &str, line: &str) -> Option<String> {
    let (before, after) = template.split_once("${version}")?;
    let start = line.find(before)? + before.len();
    let rest = &line[start..];
    let middle = if after.is_empty() {
        rest
    } else {
        &rest[..rest.find(after)?]
    };
    let middle = middle.trim();
    (!middle.is_empty()).then(|| middle.to_string())
}

fn discover(
    fetcher: &dyn Fetcher,
    discovery: &Discovery,
    ctx: &Context,
) -> Result<Vec<Candidate>, Diagnostic> {
    let Discovery::TextIndex { url, line } = discovery else {
        return Err(ctx.invalid("text_index was asked to discover another strategy"));
    };
    let url = ctx.render(url)?;
    // The architecture variables are rendered in; `${version}` is left standing, because it is
    // the hole each line is matched against.
    let mut vars = ctx.vars.to_vec();
    vars.push(("version", "${version}"));
    let template = crate::catalogue::render(line, &vars).map_err(|e| ctx.invalid(e))?;

    let body = fetcher.get(&url).map_err(crate::upstream::fetch_error)?;
    let text = String::from_utf8_lossy(&body);
    let mut out: Vec<Candidate> = Vec::new();
    for line in text.lines() {
        let Some(text) = capture_line(&template, line) else {
            continue;
        };
        let Ok(version) = Version::parse(&text) else {
            continue;
        };
        // One line per version: a document may name the same version more than once.
        if out.iter().any(|c| c.text == text) {
            continue;
        }
        out.push(Candidate {
            version,
            text,
            tag: None,
            asset: None,
            size: None,
            digest: None,
            for_arch: true,
            index_fields: BTreeMap::new(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::capture_line;

    #[test]
    fn a_line_matches_by_its_literal_prefix_and_suffix() {
        let t = "/rust-${version}-x86_64-unknown-linux-gnu.tar.gz\"";
        assert_eq!(
            capture_line(
                t,
                "url = \"https://e.test/dist/rust-1.2.3-x86_64-unknown-linux-gnu.tar.gz\""
            ),
            Some("1.2.3".to_string())
        );
        // Another component, another architecture and the xz twin are not this line.
        assert_eq!(
            capture_line(
                t,
                "url = \"https://e.test/dist/rust-std-1.2.3-x86_64-unknown-linux-gnu.tar.gz\""
            ),
            Some("std-1.2.3".to_string()),
            "the middle is captured, and rejected later because it is not a version"
        );
        assert_eq!(
            capture_line(
                t,
                "xz_url = \"https://e.test/dist/rust-1.2.3-x86_64-unknown-linux-gnu.tar.xz\""
            ),
            None
        );
        assert_eq!(capture_line(t, "date = \"2026-01-01\""), None);
    }

    #[test]
    fn an_empty_suffix_reads_to_the_end_of_the_line() {
        assert_eq!(
            capture_line("${version}", "  1.2.3  "),
            Some("1.2.3".into())
        );
        assert_eq!(capture_line("${version}", "   "), None);
    }
}
