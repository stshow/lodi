# Flake home fixtures (fh-1, LD-399)

`tests/host_flake_home.rs` copies this tree into a scratch `--root`: `passwd` and `group` become
`etc/passwd` and `etc/group`, and `hosts/` is a directory of hosts whose `box` holds a home for
the login `sample` and one for `other`. `{uid}` and `{gid}` are the test's own ids and `{other}`
an id that is not, so every home path stays inside the scratch root and no test reaches a real
home. Written by hand: nothing here is recorded from a binary.
