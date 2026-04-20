/*
Copyright 2022 The Kuasar Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

//! init.d hook runner.
//!
//! Runs all executable files in `/etc/kuasar/init.d/` in lexicographic order,
//! similar to sysvinit `rc.d`.  A non-zero exit from any hook aborts startup
//! and the caller should send a `FATAL` message to the host.
//!
//! Image builders add hooks by copying scripts to `/etc/kuasar/init.d/`:
//!
//! ```
//! /etc/kuasar/init.d/
//!   ├── 00-hostname.sh
//!   ├── 10-extra-mounts.sh
//!   ├── 50-warmup.sh
//!   └── 90-readiness.sh    ← custom readiness override
//! ```

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{anyhow, Context, Result};
use log::info;

pub const HOOKS_DIR: &str = "/etc/kuasar/init.d";

/// Run all hook scripts in `dir`, sorted lexicographically.
///
/// Returns `Ok(())` if all hooks exit 0, or `Err` with the script name and exit
/// code of the first failure.
pub fn run_hooks(dir: impl AsRef<Path>) -> Result<()> {
    let dir = dir.as_ref();
    if !dir.exists() {
        return Ok(());
    }

    let mut scripts = collect_executables(dir)?;
    scripts.sort();

    for script in &scripts {
        let name = script.file_name().unwrap_or_default().to_string_lossy();
        info!("kuasar-init: running hook {}", name);

        let status = Command::new(script)
            .status()
            .with_context(|| format!("failed to exec hook {}", name))?;

        if !status.success() {
            let code = status.code().unwrap_or(-1);
            return Err(anyhow!("init.d hook failed: {} (exit {})", name, code));
        }
        info!("kuasar-init: hook {} succeeded", name);
    }
    Ok(())
}

fn collect_executables(dir: &Path) -> Result<Vec<PathBuf>> {
    use std::os::unix::fs::PermissionsExt;
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).context("read hooks dir")? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_file() && (meta.permissions().mode() & 0o111 != 0) {
            out.push(entry.path());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn test_run_hooks_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        run_hooks(dir.path()).expect("no hooks → Ok");
    }

    #[test]
    fn test_run_hooks_nonexistent_dir() {
        run_hooks("/nonexistent-kuasar-hooks-dir").expect("missing dir → Ok");
    }

    #[test]
    fn test_run_hooks_success() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("00-ok.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        run_hooks(dir.path()).expect("exit 0 → Ok");
    }

    #[test]
    fn test_run_hooks_failure() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("10-fail.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 2\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = run_hooks(dir.path()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("10-fail.sh"),
            "error must name the script: {}",
            msg
        );
        assert!(
            msg.contains("exit 2"),
            "error must include exit code: {}",
            msg
        );
    }

    #[test]
    fn test_run_hooks_skips_non_executable() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("00-notexec.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
        run_hooks(dir.path()).expect("non-executable file is ignored");
    }

    #[test]
    fn test_run_hooks_lexicographic_order() {
        // The second script exits non-zero; first must complete successfully first.
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("ran-first.txt");
        let flag_str = flag.to_str().unwrap().to_string();

        let s1 = dir.path().join("00-first.sh");
        std::fs::write(&s1, format!("#!/bin/sh\ntouch {}\n", flag_str)).unwrap();
        std::fs::set_permissions(&s1, std::fs::Permissions::from_mode(0o755)).unwrap();

        let s2 = dir.path().join("10-second.sh");
        std::fs::write(&s2, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&s2, std::fs::Permissions::from_mode(0o755)).unwrap();

        let _ = run_hooks(dir.path()); // Will fail on 10-second.sh
        assert!(
            flag.exists(),
            "00-first.sh must run before 10-second.sh fails"
        );
    }
}
