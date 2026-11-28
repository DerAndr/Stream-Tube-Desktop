//! Standalone self-update staging and bundle replacement for macOS Intel (`x86_64`).
//! Kept in its own module so upstream changes to `updater.rs` merge cleanly.
use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn stage_bundle(dir: &Path, zip_path: &Path) -> Result<PathBuf> {
    let unpacked = dir.join("unpacked");
    let _ = std::fs::remove_dir_all(&unpacked);
    std::fs::create_dir_all(&unpacked)?;
    let st = Command::new("/usr/bin/ditto")
        .args(["-x", "-k"])
        .arg(zip_path)
        .arg(&unpacked)
        .status()
        .context("failed to unpack update archive")?;
    let _ = std::fs::remove_file(zip_path);
    if !st.success() {
        anyhow::bail!("failed to unpack the update archive");
    }
    let staged = unpacked.join("Stream Tube.app");
    if !staged.join("Contents/MacOS/streamtube").is_file() {
        anyhow::bail!("Stream Tube.app was not found inside the update archive");
    }
    let _ = Command::new("/usr/bin/xattr").arg("-cr").arg(&staged).status();
    let _ = Command::new("/bin/chmod").args(["-R", "a+rX"]).arg(&staged).status();
    let ok = Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(&staged)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        anyhow::bail!("the downloaded app bundle failed signature verification");
    }
    Ok(staged)
}

pub fn installer_command(staged_bundle: &Path, executable: Option<&Path>) -> Command {
    let bundle = executable
        .and_then(|exe| exe.parent()?.parent()?.parent())
        .filter(|b| b.extension().and_then(|e| e.to_str()) == Some("app"));
    let Some(target) = bundle else {
        let mut cmd = Command::new("/usr/bin/false");
        cmd.arg("not-an-app-bundle");
        return cmd;
    };
    let pid = std::process::id().to_string();
    let script = r#"
target="$1"; staged="$2"; pid="$3"
i=0
while kill -0 "$pid" 2>/dev/null && [ $i -lt 50 ]; do
  sleep 0.2
  i=$((i+1))
done
if rm -rf "$target" 2>/dev/null && mv "$staged" "$target" 2>/dev/null; then
  chgrp -R admin "$target" 2>/dev/null || true
  /usr/bin/open "$target"
else
  /usr/bin/osascript \
    -e 'on run argv' \
    -e 'do shell script "rm -rf " & quoted form of item 1 of argv & " && mv " & quoted form of item 2 of argv & " " & quoted form of item 1 of argv & " && chown -R root:admin " & quoted form of item 1 of argv with administrator privileges' \
    -e 'end run' "$target" "$staged" && /usr/bin/open "$target"
fi
"#;
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(script)
        .arg("streamtube-updater")
        .arg(target)
        .arg(staged_bundle)
        .arg(pid);
    cmd
}

#[cfg(test)]
mod tests {
    use super::installer_command;
    use crate::updater::is_newer;
    use std::path::Path;

    #[test]
    fn intel_tags_compare_by_semver() {
        assert!(is_newer("intel-v0.7.7-alpha", "0.7.5-alpha"));
        assert!(!is_newer("intel-v0.7.7-alpha", "0.7.7-alpha"));
    }

    #[test]
    fn installer_targets_parent_app_bundle() {
        let exe = Path::new("/Applications/Stream Tube.app/Contents/MacOS/streamtube");
        let staged = Path::new("/tmp/update/unpacked/Stream Tube.app");
        let cmd = installer_command(staged, Some(exe));
        assert_eq!(cmd.get_program(), "/bin/sh");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args[3], Path::new("/Applications/Stream Tube.app").as_os_str());
        assert_eq!(args[4], staged.as_os_str());
    }
}
