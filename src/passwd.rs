//! The selected root's own `etc/passwd` and `etc/group`, never NSS: one parser for the owners a
//! host plan resolves and the login a home in a host directory is chosen by (LD-399).

use std::fs;
use std::path::Path;

/// The passwd name and home directory of one selected user.
pub struct User {
    pub name: String,
    pub home: String,
    pub uid: u32,
    pub gid: u32,
    pub shell: String,
}

/// One group of the selected root's `etc/group`, with its supplementary members.
pub struct Group {
    pub name: String,
    pub gid: u32,
    pub members: Vec<String>,
}

/// Each line of a colon-separated database, split into its fields.
fn records(text: &str) -> impl Iterator<Item = Vec<&str>> {
    text.lines().map(|line| line.split(':').collect())
}

/// The user with `uid`, if the root's passwd file names one.
pub fn by_uid(root: &Path, uid: u32) -> Option<User> {
    let text = fs::read_to_string(root.join("etc/passwd")).ok()?;
    records(&text)
        .find(|fields| fields.len() == 7 && fields[2].parse::<u32>().ok() == Some(uid))
        .and_then(user)
}

/// The user called `name`, if the root's passwd file names one (LD-416: every declared home).
pub fn by_name(root: &Path, name: &str) -> Option<User> {
    let text = fs::read_to_string(root.join("etc/passwd")).ok()?;
    records(&text)
        .find(|fields| fields.len() == 7 && fields[0] == name)
        .and_then(user)
}

fn user(fields: Vec<&str>) -> Option<User> {
    Some(User {
        name: fields[0].to_string(),
        home: fields[5].to_string(),
        uid: fields[2].parse().ok()?,
        gid: fields[3].parse().ok()?,
        shell: fields[6].to_string(),
    })
}

/// Every user the root's passwd file names, in file order; none when there is no file.
pub fn users(root: &Path) -> Vec<User> {
    let text = fs::read_to_string(root.join("etc/passwd")).unwrap_or_default();
    records(&text)
        .filter(|fields| fields.len() == 7)
        .filter_map(user)
        .collect()
}

/// Every group the root's group file names, in file order; none when there is no file.
pub fn groups(root: &Path) -> Vec<Group> {
    let text = fs::read_to_string(root.join("etc/group")).unwrap_or_default();
    records(&text)
        .filter(|fields| fields.len() == 4)
        .filter_map(|fields| {
            Some(Group {
                name: fields[0].to_string(),
                gid: fields[2].parse().ok()?,
                members: fields[3]
                    .split(',')
                    .filter(|m| !m.is_empty())
                    .map(str::to_string)
                    .collect(),
            })
        })
        .collect()
}

/// The id of the owner or group `name` in the database at `path` (`etc/passwd` or `etc/group`).
pub fn lookup(path: &Path, name: &str) -> Option<u32> {
    let text = fs::read_to_string(path).ok()?;
    records(&text)
        .find(|fields| fields.first() == Some(&name))
        .and_then(|fields| fields.get(2)?.parse().ok())
}
