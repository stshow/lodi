//! The shapes a credential takes in the configuration files people actually write, each with a
//! fake value, shared by the host capture's tests and the home import's.
//!
//! Every value here is invented and grants nothing anywhere. A provider token is spelled as its
//! prefix followed by [`FAKE_TAIL`], put together at run time, so that no line of this
//! repository reads as a token to a scanner and the prefix alone is what the rule under test
//! looks for. None of these texts holds an address or a path of any machine.
#![allow(dead_code)]

/// What stands in for a secret value in every shape.
pub const FAKE_VALUE: &str = "lodi-fake-value-0000";

/// What follows a provider's prefix in a fake token.
pub const FAKE_TAIL: &str = "LODIFAKE0000";

/// One credential shape: a short name for messages, the text of a file that holds it, the
/// marker the content scan must name, and the fake part that must never leave the file.
pub struct Shape {
    pub name: &'static str,
    pub text: String,
    pub marker: &'static str,
    pub fake: String,
}

fn shape(name: &'static str, text: String, marker: &'static str, fake: String) -> Shape {
    Shape {
        name,
        text,
        marker,
        fake,
    }
}

/// Every shape, in a fixed order. The marker is the one `lodi::hostscope::import::files`'s scan
/// finds first, spelled as its lists spell it.
pub fn shapes() -> Vec<Shape> {
    let v = FAKE_VALUE.to_string();
    let mut out = vec![
        shape(
            "shell export, upper case",
            format!("# session defaults\nexport FAKE_TOKEN={v}\n"),
            "token",
            v.clone(),
        ),
        shape(
            "YAML key, upper case",
            format!("service:\n  API_KEY: {v}\n"),
            "api_key",
            v.clone(),
        ),
        shape(
            "netrc line",
            format!("machine mirror.invalid login lodi password {v}\n"),
            "passw",
            v.clone(),
        ),
        shape(
            "HTTP bearer header",
            format!("header = \"Authorization: Bearer {v}\"\n"),
            "bearer ",
            v.clone(),
        ),
        shape(
            "HTTP basic header",
            format!("Authorization: Basic {v}\n"),
            "authorization:",
            v.clone(),
        ),
        shape(
            "AWS credentials file",
            format!("[default]\naws_secret_access_key = {v}\n"),
            "secret",
            v.clone(),
        ),
        shape(
            "INI apikey",
            format!("[weather]\nApiKey={v}\n"),
            "apikey",
            v.clone(),
        ),
        shape(
            "header key with a dash",
            format!("X-Api-Key {v}\n"),
            "api-key",
            v.clone(),
        ),
        shape(
            "private key block, lower case",
            format!("-----begin rsa private key-----\n{v}\n"),
            "PRIVATE KEY",
            v.clone(),
        ),
    ];
    // A bare provider token, with no key name beside it that says what it is.
    for (name, prefix) in [
        ("GitHub classic token", "ghp_"),
        ("GitHub fine-grained token", "github_pat_"),
        ("GitLab token", "glpat-"),
        ("Slack bot token", "xoxb-"),
        ("AWS access key id", "AKIA"),
        ("Google API key", "AIza"),
        ("Anthropic key", "sk-ant-"),
    ] {
        let fake = format!("{prefix}{FAKE_TAIL}");
        out.push(shape(
            name,
            format!("let g:assistant_key = '{fake}'\n"),
            prefix,
            fake,
        ));
    }
    out
}

/// A configuration file that holds no credential, and that holds lower-case letters a folded
/// `AKIA` would match — the reason provider prefixes are compared with their case.
pub const BENIGN: &str =
    "# keyboard layouts: sk (Slovakia), us\nXKBLAYOUT=\"sk,us\"\nset editing-mode vi\n";
