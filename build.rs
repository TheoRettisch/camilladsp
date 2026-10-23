use std::{env, path::Path, process::Command};

fn valid_revision(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn source_revision(root: &Path, supplied: Option<String>) -> Result<String, String> {
    // Never accidentally identify a parent repository containing an unpacked
    // source archive. A worktree's .git is a file; a normal checkout's is a dir.
    let from_git = if root.join(".git").exists() {
        for name in ["HEAD", "refs", "packed-refs"] {
            let path = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["rev-parse", "--path-format=absolute", "--git-path", name])
                .output()
                .map_err(|error| error.to_string())?;
            if !path.status.success() {
                return Err("cannot locate protection source Git metadata".into());
            }
            println!(
                "cargo:rerun-if-changed={}",
                String::from_utf8_lossy(&path.stdout).trim()
            );
        }
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "HEAD"])
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err("cannot resolve protection source HEAD".into());
        }
        Some(
            String::from_utf8(output.stdout)
                .map_err(|error| error.to_string())?
                .trim()
                .to_owned(),
        )
    } else {
        None
    };
    if let (Some(pin), Some(head)) = (&supplied, &from_git) {
        if pin != head {
            return Err("CAMILLA_PROTECTION_REVISION differs from source HEAD".into());
        }
    }
    let revision = supplied.or(from_git).ok_or(
        "source archives require CAMILLA_PROTECTION_REVISION from the pinned package version",
    )?;
    if !valid_revision(&revision) {
        return Err("protection source revision must be a lowercase full Git SHA".into());
    }
    Ok(revision)
}

fn main() {
    println!("cargo:rerun-if-env-changed=CAMILLA_PROTECTION_REVISION");
    println!("cargo:rerun-if-changed=build.rs");
    let root = env::var("CARGO_MANIFEST_DIR").expect("Cargo manifest directory");
    let supplied = env::var("CAMILLA_PROTECTION_REVISION").ok();
    let revision =
        source_revision(Path::new(&root), supplied).unwrap_or_else(|error| panic!("{error}"));
    println!("cargo:rustc-env=CAMILLA_PROTECTION_BUILD_REVISION={revision}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archives_require_an_exact_explicit_revision() {
        let archive = Path::new("/nonexistent-protection-source-archive");
        assert!(source_revision(archive, None).is_err());
        for invalid in [
            "",
            "unavailable",
            "796576a",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "000000000000000000000000000000000000000g",
        ] {
            assert!(source_revision(archive, Some(invalid.into())).is_err());
        }
        let revision = "a".repeat(40);
        assert_eq!(
            source_revision(archive, Some(revision.clone())).unwrap(),
            revision
        );
    }
}
