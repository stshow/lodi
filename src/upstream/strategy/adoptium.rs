//! `strategy = "adoptium"`: the Adoptium API publishes the asset's **own URL and its own
//! SHA-256**, so this strategy is the hash source and the recipe declares neither `[asset] url`
//! nor `checksum_file`.
//!
//! Discovery asks one page of one *feature release* line (`21`, `25`, …), because that is how
//! the API is shaped: there is no endpoint that lists every version of every line. Which line
//! is asked for comes from the recipe's `feature_version` when it has one, and otherwise from
//! the constraint the caller wrote — `jdk = "21"` asks for 21. A bare `latest` names no line at
//! all, so the API's own `available_lts_releases` is read first and its **maximum** is the line:
//! `latest` is the newest long-term-support release, never the newest early-access build, which
//! is a version of a line that `/ga` does not publish (design call: LD record of this package).
//!
//! **The version text and the ordering key come from different fields of the same record**, and
//! that is deliberate. `version_data.semver` is what a user sees and what the lock records —
//! `21.0.12+101.0.LTS` — but its build metadata is ignored by version precedence (`spec/04`
//! §4.1), so `21.0.12+101.0.LTS` (which is OpenJDK `21.0.12.1+1`, a patched build) and
//! `21.0.12+8.0.LTS` would tie and the older of the two could be chosen. The ordering key is
//! therefore built from the record's own structured fields — `major`, `minor`, `security`,
//! `patch` (absent means 0) and `build` — which is exactly the order Adoptium publishes them in.
//!
//! There is no second vendor, no second JVM implementation and no fallback: `jvm_impl` is
//! `hotspot` and anything else is `E_RECIPE_INVALID`. A binary the API publishes without a
//! `package.checksum` is `E_NO_CHECKSUM` in the resolver and **not** a download.

use crate::catalogue::Reader;
use crate::diag::Diagnostic;
use crate::fetch::Fetcher;
use crate::upstream::strategy::{AssetUrl, Candidate, Context, Discovery, Strategy};
use crate::upstream::{fetch_error, json};
use crate::version::Version;
use toml_edit::TableLike;

/// The API root. One host, one vendor: Adoptium's own service and nothing else.
const API: &str = "https://api.adoptium.net";
/// Where every binary the API links to is published: Adoptium's own release repositories. A
/// `package.link` anywhere else is refused, whatever digest comes with it (M-1.0 u-4, LD-363).
const LINK_PREFIX: &str = "https://github.com/adoptium/";
/// How many releases of the chosen line one page carries, as `TASKS.md` T-5 specifies it.
const PAGE_SIZE: u32 = 20;
/// The operating system this build resolves for (OD-14: Linux, x86_64).
const OS: &str = "linux";
/// The only JVM implementation 0.4 accepts.
const HOTSPOT: &str = "hotspot";
/// The image types the API distinguishes and this strategy accepts.
const IMAGE_TYPES: &[&str] = &["jdk", "jre"];
/// The feature-version line numbers a recipe may name. 8 is the oldest Adoptium publishes.
const FEATURE_RANGE: std::ops::RangeInclusive<i64> = 8..=99;

pub const STRATEGY: Strategy = Strategy {
    parse,
    discover,
    asset_url: AssetUrl::FromStrategy,
    supplies_digest: true,
};

fn parse(
    r: &Reader,
    versions: &dyn TableLike,
    _before_version: &[&str],
) -> Result<Discovery, Diagnostic> {
    r.keys(
        versions,
        "versions",
        &["strategy", "feature_version", "image_type", "jvm_impl"],
    )?;

    let feature_version = match versions.get("feature_version") {
        None => None,
        Some(item) => match item.as_integer() {
            Some(n) if FEATURE_RANGE.contains(&n) => Some(n as u32),
            _ => {
                return Err(r.invalid(format!(
                    "`feature_version` in [versions] must be an integer from {} to {}",
                    FEATURE_RANGE.start(),
                    FEATURE_RANGE.end()
                )));
            }
        },
    };

    let image_type = r
        .opt_string(versions, "image_type", "versions")?
        .unwrap_or_else(|| IMAGE_TYPES[0].to_string());
    if !IMAGE_TYPES.contains(&image_type.as_str()) {
        return Err(r.invalid(format!(
            "`image_type` in [versions] is `{image_type}`; Adoptium publishes {}",
            IMAGE_TYPES.join(" and ")
        )));
    }

    let jvm_impl = r
        .opt_string(versions, "jvm_impl", "versions")?
        .unwrap_or_else(|| HOTSPOT.to_string());
    if jvm_impl != HOTSPOT {
        return Err(r.invalid(format!(
            "`jvm_impl` in [versions] is `{jvm_impl}`; this build resolves `{HOTSPOT}` only"
        )));
    }

    Ok(Discovery::Adoptium {
        feature_version,
        image_type,
        jvm_impl,
    })
}

/// The first run of ASCII digits in a constraint, as its feature version: `21` for `"21"`,
/// `17` for `">=17, <22"`, and **nothing** for `"latest"`, which names no line.
pub(crate) fn feature_of(constraint: &str) -> Option<u32> {
    let start = constraint.find(|c: char| c.is_ascii_digit())?;
    let rest = &constraint[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// The maximum of the API's own `available_lts_releases`. This is the one extra request this
/// strategy ever makes, and only for a constraint that names no line.
fn newest_lts(fetcher: &dyn Fetcher) -> Result<u32, Diagnostic> {
    let url = format!("{API}/v3/info/available_releases");
    let body = fetcher.get(&url).map_err(fetch_error)?;
    let info = json(&body, &url)?;
    info["available_lts_releases"]
        .as_array()
        .and_then(|list| list.iter().filter_map(serde_json::Value::as_u64).max())
        .map(|n| n as u32)
        .ok_or_else(|| {
            Diagnostic::new(
                "E_RECIPE_CTX",
                format!("{url} lists no available_lts_releases"),
            )
        })
}

/// Whether `link` names a file under [`LINK_PREFIX`]: that prefix, then a path of printable
/// ASCII with no `..` segment and no backslash, so it cannot step out of the prefix.
fn link_is_adoptiums(link: &str) -> bool {
    link.strip_prefix(LINK_PREFIX).is_some_and(|rest| {
        !rest.is_empty()
            && rest.bytes().all(|b| b.is_ascii_graphic() && b != b'\\')
            && !rest
                .split(['/', '?', '#'])
                .any(|segment| segment == ".." || segment.eq_ignore_ascii_case("%2e%2e"))
    })
}

fn discover(
    fetcher: &dyn Fetcher,
    discovery: &Discovery,
    ctx: &Context,
) -> Result<Vec<Candidate>, Diagnostic> {
    let Discovery::Adoptium {
        feature_version,
        image_type,
        jvm_impl,
    } = discovery
    else {
        return Err(ctx.invalid("adoptium was asked to discover another strategy"));
    };

    // The API names architectures its own way (`x64`), which is what `[arch_names]` is for.
    let architecture = ctx
        .vars
        .iter()
        .find(|(k, _)| *k == "arch_name")
        .or_else(|| ctx.vars.iter().find(|(k, _)| *k == "arch"))
        .map(|(_, v)| *v)
        .ok_or_else(|| ctx.invalid("no architecture to ask the Adoptium API for"))?;

    let feature = match feature_version {
        Some(n) => *n,
        None => match feature_of(ctx.constraint_text) {
            Some(n) => n,
            None => newest_lts(fetcher)?,
        },
    };

    let url = format!(
        "{API}/v3/assets/feature_releases/{feature}/ga?os={OS}&architecture={architecture}\
         &image_type={image_type}&jvm_impl={jvm_impl}&page_size={PAGE_SIZE}"
    );
    let body = fetcher.get(&url).map_err(fetch_error)?;
    let list = json(&body, &url)?;
    let shape = || {
        Diagnostic::new(
            "E_RECIPE_CTX",
            format!("{url}: unexpected feature-release list shape"),
        )
    };

    let mut out: Vec<Candidate> = Vec::new();
    for release in list.as_array().ok_or_else(shape)? {
        let data = &release["version_data"];
        let Some(text) = data["semver"].as_str() else {
            return Err(shape());
        };
        // The ordering key, from the record's own fields rather than from the semver string:
        // see this module's header for why the two differ.
        let part = |key: &str| data[key].as_u64();
        let (Some(major), Some(minor), Some(security), Some(build)) = (
            part("major"),
            part("minor"),
            part("security"),
            part("build"),
        ) else {
            return Err(shape());
        };
        // The text is what the lock records and a user reads, so it must be a version, and the
        // version the structured fields say this record is (M-1.0 u-4, LD-363).
        let parsed = Version::parse(text).ok().filter(|v| {
            v.parts.len() == 3 && v.parts == [major, minor, security] && v.pre.is_none()
        });
        if parsed.is_none() {
            return Err(Diagnostic::new(
                "E_RECIPE_CTX",
                format!(
                    "{url}: `version_data.semver` `{text}` is not the version {major}.{minor}.{security}"
                ),
            ));
        }
        let version = Version {
            parts: vec![major, minor, security, part("patch").unwrap_or(0), build],
            pre: None,
            build: None,
        };
        if out.iter().any(|c| c.text == text) {
            continue;
        }

        // The query already asks for one architecture, one image type and one implementation,
        // so a release that carries a binary carries the right one.
        let binaries = release["binaries"].as_array().ok_or_else(shape)?;
        let Some(package) = binaries.first().map(|b| &b["package"]) else {
            // The line published this version without a build for this architecture: the
            // version is real and the artifact is missing, which the resolver reports as
            // `E_UNSUPPORTED_ARCH` rather than silence.
            out.push(Candidate {
                version,
                text: text.to_string(),
                tag: None,
                asset: None,
                size: None,
                digest: None,
                for_arch: false,
                index_fields: Default::default(),
            });
            continue;
        };
        let (Some(name), Some(link)) = (package["name"].as_str(), package["link"].as_str()) else {
            return Err(shape());
        };
        if !link_is_adoptiums(link) {
            return Err(Diagnostic::new(
                "E_RECIPE_CTX",
                format!("{url}: the {text} binary links to {link}, not under {LINK_PREFIX}"),
            ));
        }
        out.push(Candidate {
            version,
            text: text.to_string(),
            tag: None,
            asset: Some((name.to_string(), link.to_string())),
            size: package["size"].as_u64(),
            // The API publishes a bare hex digest; the resolver reads `sha256:`-tagged ones.
            // A binary with no `checksum` yields no digest at all, which is `E_NO_CHECKSUM`.
            digest: package["checksum"]
                .as_str()
                .map(|hex| format!("sha256:{hex}")),
            for_arch: true,
            index_fields: Default::default(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_constraint_names_a_feature_line_only_when_it_carries_a_number() {
        assert_eq!(feature_of("21"), Some(21));
        assert_eq!(feature_of("17.0.9"), Some(17));
        assert_eq!(feature_of(">=17, <22"), Some(17));
        assert_eq!(feature_of("^25"), Some(25));
        assert_eq!(feature_of("latest"), None);
    }

    #[test]
    fn a_binary_link_is_held_to_adoptiums_own_repositories() {
        assert!(link_is_adoptiums(
            "https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.8%2B9/x.tar.gz"
        ));
        for link in [
            "https://example.org/adoptium/x.tar.gz",
            "https://github.com/adoptium-mirror/x.tar.gz",
            "https://github.com.example.org/adoptium/x.tar.gz",
            "http://github.com/adoptium/x.tar.gz",
            "https://github.com/adoptium/../other/x.tar.gz",
            "https://github.com/adoptium/%2E%2E/other/x.tar.gz",
            "https://github.com/adoptium/a\\b",
            "https://github.com/adoptium/a b",
            "https://github.com/adoptium/",
        ] {
            assert!(!link_is_adoptiums(link), "{link}");
        }
    }

    #[test]
    fn the_ordering_key_puts_a_patched_build_above_the_build_it_patches() {
        // 21.0.12+101.0.LTS is OpenJDK 21.0.12.1+1; 21.0.12+8.0.LTS is 21.0.12+8.
        let patched = Version {
            parts: vec![21, 0, 12, 1, 1],
            pre: None,
            build: None,
        };
        let original = Version {
            parts: vec![21, 0, 12, 0, 8],
            pre: None,
            build: None,
        };
        assert!(patched > original);
    }
}
