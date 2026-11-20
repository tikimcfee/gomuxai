// The fixture the Go allowcmd_test.go lays out, shared by the tests here.
#![allow(dead_code)]

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use agent_allowcmd::Checker;

#[rustfmt::skip]
pub const SAFE: &[&str] = &[
    "send", "deliver", "list", "status", "wait", "events", "capture", "logs", "state get",
    "history", "friction add", "friction +1", "friction list", "friction show", "friction link",
    "friction links",
];

pub struct Fixture {
    // Held for its Drop: the directory goes when the fixture does.
    _dir: tempfile::TempDir,
    pub root: PathBuf,
}

impl Fixture {
    pub fn root_str(&self) -> &str {
        self.root.to_str().unwrap()
    }

    pub fn path(&self, rel: &str) -> String {
        self.root.join(rel).to_str().unwrap().to_string()
    }
}

/// fixture lays out root/bin/agent (the program), root/link/agent (a
/// symlink to it), and root/bin/other (another binary). Beyond the Go's:
/// root/sub/deep (a directory) and root/up, a symlink to it, so a path
/// through "up/.." resolves differently by its text and by the kernel.
pub fn fixture() -> (Fixture, Checker) {
    let dir = tempfile::Builder::new()
        .prefix("allowcmd-test-")
        .tempdir_in("/var/tmp")
        .unwrap();
    // Resolved, so the paths the checker reports compare equal.
    let root = std::fs::canonicalize(dir.path()).unwrap();
    for d in ["bin", "link", "sub/deep"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    let prog = root.join("bin").join("agent");
    for f in [&prog, &root.join("bin").join("other")] {
        std::fs::write(f, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    symlink(&prog, root.join("link").join("agent")).unwrap();
    symlink(root.join("sub").join("deep"), root.join("up")).unwrap();
    let look_root = root.clone();
    let look = move |name: &str, _cwd: &str| -> anyhow::Result<String> {
        match name {
            "agent" => Ok(path_str(&look_root.join("link").join("agent"))),
            "other" => Ok(path_str(&look_root.join("bin").join("other"))),
            _ => anyhow::bail!("not found"),
        }
    };
    let mut c = Checker::new(path_str(&prog), SAFE);
    c.look_path = Some(Box::new(look));
    (Fixture { _dir: dir, root }, c)
}

fn path_str(p: &Path) -> String {
    p.to_str().unwrap().to_string()
}
