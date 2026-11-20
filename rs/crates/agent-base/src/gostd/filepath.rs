//! Go's `path/filepath` on Unix: lexical path handling on strings, with Go's
//! results (`dir("x")` is ".", `join` cleans), which `std::path` doesn't
//! give: a `Path` keeps "a//b/./c" as written and has no `..` folding.

use super::os::{PathError, getwd};

/// Go's `filepath.Separator`.
pub const SEPARATOR: char = '/';

/// Go's `filepath.Clean`: the shortest path equivalent to `path` by purely
/// lexical processing. Multiple slashes become one, `.` elements go, an inner
/// `..` removes the element before it, a `..` at the root is dropped, and the
/// empty path is ".".
pub fn clean(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let rooted = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    // How many leading ".." a relative path keeps.
    let mut kept_dotdot = 0usize;
    for elem in path.split('/') {
        match elem {
            "" | "." => {}
            ".." => {
                if parts.len() > kept_dotdot {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                    kept_dotdot += 1;
                }
            }
            e => parts.push(e),
        }
    }
    let joined = parts.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_string(),
        (false, false) => joined,
    }
}

/// Go's `filepath.Join`: the non-empty elements joined with slashes, cleaned;
/// "" when every element is empty.
pub fn join<S: AsRef<str>>(elems: &[S]) -> String {
    let parts: Vec<&str> = elems
        .iter()
        .map(|e| e.as_ref())
        .filter(|e| !e.is_empty())
        .collect();
    if parts.is_empty() {
        return String::new();
    }
    clean(&parts.join("/"))
}

/// Go's `filepath.Dir`: everything but the last element, cleaned; "." for a
/// path with no slash.
pub fn dir(path: &str) -> String {
    let i = path.rfind('/').map(|i| i + 1).unwrap_or(0);
    clean(&path[..i])
}

/// Go's `filepath.Base`: the last element, trailing slashes ignored; "." for
/// the empty path and "/" for a path of only slashes.
pub fn base(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    match trimmed.rfind('/') {
        Some(i) => trimmed[i + 1..].to_string(),
        None => trimmed.to_string(),
    }
}

/// Go's `filepath.Ext`: the suffix from the last dot in the last element, or
/// "".
pub fn ext(path: &str) -> String {
    for (i, c) in path.char_indices().rev() {
        match c {
            '/' => break,
            '.' => return path[i..].to_string(),
            _ => {}
        }
    }
    String::new()
}

/// Go's `filepath.Split`: `path` split after its last slash, so
/// `dir + file == path`.
pub fn split(path: &str) -> (String, String) {
    let i = path.rfind('/').map(|i| i + 1).unwrap_or(0);
    (path[..i].to_string(), path[i..].to_string())
}

/// Go's `filepath.IsAbs`.
pub fn is_abs(path: &str) -> bool {
    path.starts_with('/')
}

/// Go's `filepath.Abs`: `path` cleaned, joined to the working directory if it
/// is relative.
pub fn abs(path: &str) -> anyhow::Result<String> {
    if is_abs(path) {
        return Ok(clean(path));
    }
    let wd = getwd()?;
    Ok(join(&[wd.as_str(), path]))
}

/// Go's `filepath.Rel`: a path that joined to `basepath` gives `targpath`, or
/// an error when there is none ("Rel: can't make b relative to a").
pub fn rel(basepath: &str, targpath: &str) -> anyhow::Result<String> {
    let base = clean(basepath);
    let targ = clean(targpath);
    if targ == base {
        return Ok(".".to_string());
    }
    let base_v = if base == "." { "" } else { base.as_str() };
    let targ_v = if targ == "." { "" } else { targ.as_str() };
    if base_v.starts_with('/') != targ_v.starts_with('/') {
        anyhow::bail!("Rel: can't make {targpath} relative to {basepath}");
    }
    let bl = base_v.len();
    let tl = targ_v.len();
    let (b, t) = (base_v.as_bytes(), targ_v.as_bytes());
    let (mut b0, mut bi, mut t0, mut ti) = (0usize, 0usize, 0usize, 0usize);
    loop {
        while bi < bl && b[bi] != b'/' {
            bi += 1;
        }
        while ti < tl && t[ti] != b'/' {
            ti += 1;
        }
        if targ_v[t0..ti] != base_v[b0..bi] {
            break;
        }
        if bi < bl {
            bi += 1;
        }
        if ti < tl {
            ti += 1;
        }
        b0 = bi;
        t0 = ti;
    }
    if &base_v[b0..bi] == ".." {
        anyhow::bail!("Rel: can't make {targpath} relative to {basepath}");
    }
    if b0 != bl {
        let seps = base_v[b0..bl].matches('/').count();
        let mut out = String::from("..");
        for _ in 0..seps {
            out.push_str("/..");
        }
        if t0 != tl {
            out.push('/');
            out.push_str(&targ_v[t0..]);
        }
        return Ok(out);
    }
    Ok(targ_v[t0..].to_string())
}

/// Go's `filepath.EvalSymlinks`: `path` with every symbolic link in it
/// resolved, cleaned. A relative path stays relative (unlike
/// `std::fs::canonicalize`), and a path that doesn't exist is an error.
pub fn eval_symlinks(path: &str) -> anyhow::Result<String> {
    let mut path = path.to_string();
    let mut vol_len = 0usize;
    if path.starts_with('/') {
        vol_len = 1;
    }
    let mut vol = path[..vol_len].to_string();
    let mut dest = vol.clone();
    let mut links_walked = 0;
    let mut start;
    let mut end = vol_len;
    loop {
        start = end;
        let p = path.as_bytes();
        if start >= p.len() {
            break;
        }
        while start < p.len() && p[start] == b'/' {
            start += 1;
        }
        end = start;
        while end < p.len() && p[end] != b'/' {
            end += 1;
        }
        if end == start {
            break; // No more path components.
        }
        let elem = &path[start..end];
        if elem == "." {
            continue;
        }
        if elem == ".." {
            // Back up to the previous component if possible.
            let r = dest[vol_len.min(dest.len())..]
                .rfind('/')
                .map(|i| i + vol_len);
            match r {
                Some(r) if &dest[r + 1..] != ".." => dest.truncate(r),
                _ => {
                    if dest.len() > vol_len {
                        dest.push('/');
                    }
                    dest.push_str("..");
                }
            }
            continue;
        }
        // An ordinary path component: add it to the result.
        if !dest.is_empty() && !dest.ends_with('/') {
            dest.push('/');
        }
        dest.push_str(elem);

        let meta =
            std::fs::symlink_metadata(&dest).map_err(|e| PathError::new("lstat", &dest, e))?;
        if !meta.file_type().is_symlink() {
            if !meta.is_dir() && end < path.len() {
                return Err(std::io::Error::from_raw_os_error(libc::ENOTDIR).into());
            }
            continue;
        }
        // Found a symlink.
        links_walked += 1;
        if links_walked > 255 {
            anyhow::bail!("EvalSymlinks: too many links");
        }
        let link = std::fs::read_link(&dest).map_err(|e| PathError::new("readlink", &dest, e))?;
        let link = link.to_string_lossy().into_owned();
        path = format!("{link}{}", &path[end..]);
        if link.starts_with('/') {
            // A symlink to an absolute path.
            dest = "/".to_string();
            end = 1;
            vol = "/".to_string();
            vol_len = 1;
        } else {
            // A symlink to a relative path: replace the last path component in dest.
            match dest[vol_len.min(dest.len())..]
                .rfind('/')
                .map(|i| i + vol_len)
            {
                Some(r) => dest.truncate(r),
                None => dest = vol.clone(),
            }
            end = 0;
        }
    }
    Ok(clean(&dest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_cases() {
        for (in_, want) in [
            ("", "."),
            ("abc", "abc"),
            ("abc/def", "abc/def"),
            ("a/b/c", "a/b/c"),
            (".", "."),
            ("..", ".."),
            ("../..", "../.."),
            ("../../abc", "../../abc"),
            ("/abc", "/abc"),
            ("/", "/"),
            ("abc/", "abc"),
            ("abc/def/", "abc/def"),
            ("/abc/", "/abc"),
            ("//abc", "/abc"),
            ("///abc", "/abc"),
            ("abc//def//ghi", "abc/def/ghi"),
            ("abc/./def", "abc/def"),
            ("/./abc/def", "/abc/def"),
            ("abc/.", "abc"),
            ("abc/def/ghi/../jkl", "abc/def/jkl"),
            ("abc/def/../ghi/../jkl", "abc/jkl"),
            ("abc/def/..", "abc"),
            ("abc/def/../..", "."),
            ("/abc/def/../..", "/"),
            ("abc/def/../../..", ".."),
            ("/abc/def/../../..", "/"),
            ("abc/def/../../../ghi/jkl/../../../mno", "../../mno"),
            ("abc/./../def", "def"),
            ("abc//./../def", "def"),
            ("abc/../../././../def", "../../def"),
        ] {
            assert_eq!(clean(in_), want, "clean({in_:?})");
        }
    }

    #[test]
    fn dir_base_join_ext() {
        assert_eq!(dir("gomuxai.db"), ".");
        assert_eq!(dir("/a/b"), "/a");
        assert_eq!(dir("a/"), "a");
        assert_eq!(dir("/"), "/");
        assert_eq!(dir(""), ".");
        assert_eq!(base(""), ".");
        assert_eq!(base("///"), "/");
        assert_eq!(base("/a/b/"), "b");
        assert_eq!(join(&["", ""]), "");
        assert_eq!(join(&[".", "bus"]), "bus");
        assert_eq!(join(&["/a", "../b", "c"]), "/b/c");
        assert_eq!(ext("a/b.tar.gz"), ".gz");
        assert_eq!(ext("a.b/c"), "");
        assert_eq!(split("a/b/c"), ("a/b/".to_string(), "c".to_string()));
    }

    #[test]
    fn rel_cases() {
        for (b, t, want) in [
            ("a/b", "a/b", "."),
            ("a/b/.", "a/b", "."),
            ("a/b", "a/b/c", "c"),
            ("a/b", "a/c", "../c"),
            ("a/b", "c/d", "../../c/d"),
            ("/a/b", "/a/b/c/d", "c/d"),
            ("/", "/a", "a"),
            ("/a/b", "/", "../.."),
            ("a", ".", ".."),
        ] {
            assert_eq!(rel(b, t).unwrap(), want, "rel({b:?}, {t:?})");
        }
        assert!(rel("/a", "b").is_err());
        assert!(rel("../a", "b").is_err());
    }

    #[test]
    fn eval_symlinks_resolves_and_keeps_relative() {
        let dir = tempfile::Builder::new().tempdir_in("/var/tmp").unwrap();
        let root = dir.path().to_str().unwrap().to_string();
        std::fs::create_dir(format!("{root}/real")).unwrap();
        std::fs::write(format!("{root}/real/f"), "x").unwrap();
        std::os::unix::fs::symlink("real", format!("{root}/link")).unwrap();
        std::os::unix::fs::symlink(format!("{root}/real/f"), format!("{root}/abs")).unwrap();
        let real_root = eval_symlinks(&root).unwrap();
        assert_eq!(
            eval_symlinks(&format!("{root}/link/f")).unwrap(),
            format!("{real_root}/real/f")
        );
        assert_eq!(
            eval_symlinks(&format!("{root}/abs")).unwrap(),
            format!("{real_root}/real/f")
        );
        assert!(eval_symlinks(&format!("{root}/missing")).is_err());
        assert!(eval_symlinks(&format!("{root}/real/f/x")).is_err());
    }
}
