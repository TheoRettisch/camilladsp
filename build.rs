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
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--ignore-submodules=none",
            ])
            .output()
            .map_err(|error| error.to_string())?;
        if !status.status.success() {
            return Err("cannot verify protection source cleanliness".into());
        }
        if !status.stdout.is_empty() {
            return Err("protection builds require a clean Git worktree and index (including untracked files)".into());
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
    if let (Some(pin), Some(head)) = (&supplied, &from_git)
        && pin != head
    {
        return Err("CAMILLA_PROTECTION_REVISION differs from source HEAD".into());
    }
    let revision = supplied.or(from_git).ok_or(
        "source archives require CAMILLA_PROTECTION_REVISION from the pinned package version",
    )?;
    if !valid_revision(&revision) {
        return Err("protection source revision must be a lowercase full Git SHA".into());
    }
    Ok(revision)
}

#[cfg(not(test))]
fn main() {
    // Protection needs Linux CLOCK_MONOTONIC, 64-bit ALSA frame counts and f64 samples.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
        || env::var("CARGO_CFG_TARGET_POINTER_WIDTH").as_deref() != Ok("64")
        || env::var_os("CARGO_FEATURE_32BIT").is_some()
    {
        panic!("the protection fork only builds for 64-bit Linux, without the 32bit feature");
    }
    println!("cargo:rerun-if-env-changed=CAMILLA_PROTECTION_REVISION");
    // Intentionally absent: Cargo reruns for a missing input on every build.
    // Checking only HEAD/ref metadata misses dirty incremental source builds,
    // and enumerating existing files misses newly added build inputs.
    let out = env::var("OUT_DIR").expect("Cargo output directory");
    println!("cargo:rerun-if-changed={out}/protection-source-check-always");
    let root = env::var("CARGO_MANIFEST_DIR").expect("Cargo manifest directory");
    let supplied = env::var("CAMILLA_PROTECTION_REVISION").ok();
    let revision =
        source_revision(Path::new(&root), supplied).unwrap_or_else(|error| panic!("{error}"));
    println!("cargo:rustc-env=CAMILLA_PROTECTION_BUILD_REVISION={revision}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf, process::Output};

    struct SourceFixture(PathBuf);

    impl Drop for SourceFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    impl SourceFixture {
        fn git(&self, args: &[&str]) -> String {
            let output = Command::new("git")
                .current_dir(&self.0)
                .args([
                    "-c",
                    "user.name=Provenance test",
                    "-c",
                    "user.email=test@example.invalid",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        }

        fn stage(&self, path: &str) {
            let blob = self.git(&["hash-object", "-w", path]);
            self.git(&[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{blob},{path}"),
            ]);
        }

        fn build(&self) -> Output {
            Command::new(option_env!("CARGO").unwrap_or("cargo"))
                .current_dir(&self.0)
                .args(["build", "--offline", "--locked"])
                .env("CARGO_TARGET_DIR", self.0.join("target"))
                .env_remove("CAMILLA_PROTECTION_REVISION")
                .output()
                .unwrap()
        }
    }

    #[test]
    fn dirty_incremental_build_cannot_reuse_clean_revision() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let source = SourceFixture(env::temp_dir().join(format!(
            "protection-provenance-{}-{unique}",
            std::process::id()
        )));
        fs::create_dir_all(source.0.join("src")).unwrap();
        let main = "fn main() { println!(\"{}\", env!(\"CAMILLA_PROTECTION_BUILD_REVISION\")); }\n";
        for (path, contents) in [
            (".gitignore", "/target\n"),
            (
                "Cargo.toml",
                "[package]\nname = \"protection-provenance-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            ),
            (
                "Cargo.lock",
                "version = 4\n[[package]]\nname = \"protection-provenance-fixture\"\nversion = \"0.1.0\"\n",
            ),
            ("build.rs", include_str!("build.rs")),
            ("src/main.rs", main),
        ] {
            fs::write(source.0.join(path), contents).unwrap();
        }
        source.git(&["init", "--quiet"]);
        for path in [
            ".gitignore",
            "Cargo.toml",
            "Cargo.lock",
            "build.rs",
            "src/main.rs",
        ] {
            source.stage(path);
        }
        // Plumbing creates only this disposable fixture's history and index.
        let tree = source.git(&["write-tree"]);
        let head = source.git(&["commit-tree", &tree, "-m", "Clean source fixture"]);
        source.git(&["update-ref", "HEAD", &head]);
        for _ in 0..2 {
            let clean = source.build();
            assert!(clean.status.success(), "{clean:?}");
        }
        let binary = Command::new(source.0.join("target/debug/protection-provenance-fixture"))
            .output()
            .unwrap();
        assert!(binary.status.success());
        assert_eq!(String::from_utf8(binary.stdout).unwrap().trim(), head);
        assert!(source_revision(&source.0, Some("0".repeat(40))).is_err());

        fs::write(
            source.0.join("src/main.rs"),
            main.replace("println!(\"{}\"", "println!(\"changed-{}\""),
        )
        .unwrap();
        let dirty = source.build();
        assert!(
            !dirty.status.success(),
            "incremental dirty build reused clean provenance"
        );
        assert!(String::from_utf8_lossy(&dirty.stderr).contains("clean Git worktree and index"));

        source.stage("src/main.rs");
        fs::write(source.0.join("src/main.rs"), main).unwrap();
        let staged = source.build();
        assert!(!staged.status.success(), "staged changes were ignored");
        source.git(&["read-tree", "HEAD"]);
        assert!(source.build().status.success());
        fs::write(source.0.join("src/new.rs"), "// new build input\n").unwrap();
        let untracked = source.build();
        assert!(!untracked.status.success(), "new source file was ignored");
    }

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
