//! The container-side bash core, embedded into the binary.
//!
//! introdus is a single self-contained binary, but the container's security
//! core stays bash (see PLAN.md). We `include_str!` those files at build time
//! and [`materialize`] them into a per-container assets directory at launch.
//! That directory doubles as:
//!   * the **base-image build context** (`Dockerfile` at its root + the
//!     `container/` tree the Dockerfile `COPY`s), and
//!   * the source of the **runtime bind-mounts** (`setup.sh`,
//!     `firewall-entrypoint.sh`, `tinyproxy.conf`) that `launch` mounts into the
//!     container so edits apply without a rebuild — exactly as the old
//!     `launch_dev_container.sh` did.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// One embedded file: its repo-relative path, contents, and whether it should
/// be materialized executable.
struct Asset {
    /// Path relative to the assets/build-context root (e.g. `container/bin/rc-notify`).
    rel: &'static str,
    contents: &'static str,
    exec: bool,
}

/// Every file needed to build the base image and to bind-mount the security
/// core at runtime. Paths mirror the repo layout so the Dockerfile's `COPY`
/// directives resolve unchanged.
const ASSETS: &[Asset] = &[
    Asset {
        rel: "Dockerfile",
        contents: include_str!("../../../Dockerfile"),
        exec: false,
    },
    Asset {
        rel: "setup.sh",
        contents: include_str!("../../../setup.sh"),
        exec: true,
    },
    Asset {
        rel: "container/agents.sh",
        contents: include_str!("../../../container/agents.sh"),
        exec: false,
    },
    Asset {
        rel: "container/bin/egress-log",
        contents: include_str!("../../../container/bin/egress-log"),
        exec: true,
    },
    Asset {
        rel: "container/bin/install-agents",
        contents: include_str!("../../../container/bin/install-agents"),
        exec: true,
    },
    Asset {
        rel: "container/bin/rc-notify",
        contents: include_str!("../../../container/bin/rc-notify"),
        exec: true,
    },
    Asset {
        rel: "container/bin/run-claude",
        contents: include_str!("../../../container/bin/run-claude"),
        exec: true,
    },
    Asset {
        rel: "container/claude/settings.json",
        contents: include_str!("../../../container/claude/settings.json"),
        exec: false,
    },
    Asset {
        rel: "container/claude/test_notify.sh",
        contents: include_str!("../../../container/claude/test_notify.sh"),
        exec: true,
    },
    Asset {
        rel: "container/egress/firewall-entrypoint.sh",
        contents: include_str!("../../../container/egress/firewall-entrypoint.sh"),
        exec: true,
    },
    Asset {
        rel: "container/egress/tinyproxy.conf",
        contents: include_str!("../../../container/egress/tinyproxy.conf"),
        exec: false,
    },
];

/// Write every embedded asset under `dir`, preserving relative paths and file
/// modes. Overwrites existing files so a new binary version refreshes the core.
pub fn materialize(dir: &Path) -> Result<()> {
    for a in ASSETS {
        let target = dir.join(a.rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&target, a.contents)
            .with_context(|| format!("writing asset {}", target.display()))?;
        set_mode(&target, a.exec)?;
    }
    Ok(())
}

/// The materialized `setup.sh` (bind-mounted at `/setup.sh`).
pub fn setup_script(dir: &Path) -> PathBuf {
    dir.join("setup.sh")
}

/// The materialized firewall entrypoint (bind-mounted at
/// `/usr/local/bin/firewall-entrypoint.sh`).
pub fn entrypoint(dir: &Path) -> PathBuf {
    dir.join("container/egress/firewall-entrypoint.sh")
}

/// The materialized tinyproxy config (bind-mounted at
/// `/etc/tinyproxy/tinyproxy.conf`).
pub fn tinyproxy_conf(dir: &Path) -> PathBuf {
    dir.join("container/egress/tinyproxy.conf")
}

/// The Dockerfile at the root of the build context (`dir` itself).
pub fn dockerfile(dir: &Path) -> PathBuf {
    dir.join("Dockerfile")
}

#[cfg(unix)]
fn set_mode(path: &Path, exec: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if exec { 0o755 } else { 0o644 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {mode:o} {}", path.display()))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _exec: bool) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ta21_assets_embed_nonempty() {
        assert!(ASSETS.iter().all(|a| !a.contents.is_empty()));
        // Spot-check the security-critical core is really embedded.
        let entry = ASSETS
            .iter()
            .find(|a| a.rel.ends_with("firewall-entrypoint.sh"))
            .unwrap();
        assert!(
            entry.contents.contains("nft"),
            "entrypoint must install nft"
        );
    }

    /// Materialize the tree, then run the embedded `rc-notify` with a stub
    /// `paseo` on PATH that records its argv + stdin. Returns what the stub saw
    /// (empty when it was never invoked). `terminal` seeds `PASEO_TERMINAL_ID`.
    #[cfg(unix)]
    fn run_rc_notify(dir: &Path, event: &str, terminal: Option<&str>) -> String {
        use std::os::unix::fs::PermissionsExt;

        let bin = dir.join("stub-bin");
        std::fs::create_dir_all(&bin).unwrap();
        // Per-invocation record: the stub appends, so a shared path would let an
        // earlier call's output masquerade as this one's.
        let record = dir.join(format!(
            "paseo-calls-{event}-{}.log",
            terminal.unwrap_or("none")
        ));
        let stub = bin.join("paseo");
        std::fs::write(
            &stub,
            "#!/usr/bin/env bash\n{ printf '%s ' \"$@\"; printf '\\n'; cat; } >> \"$RECORD\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut cmd = std::process::Command::new(dir.join("container/bin/rc-notify"));
        cmd.arg(event)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("RECORD", &record)
            // Never touch a real /run/notify from a test (see rc-notify).
            .env("RC_NOTIFY_TARGET", dir.join("no-such-endpoint"))
            .env("PASEO_ACTIVITY_TOKEN", "t")
            .env("PASEO_TERMINAL_ACTIVITY_URL", "http://127.0.0.1:1/activity")
            .env_remove("PASEO_TERMINAL_ID");
        if let Some(id) = terminal {
            cmd.env("PASEO_TERMINAL_ID", id);
        }
        assert!(cmd.status().unwrap().success(), "rc-notify must exit 0");
        std::fs::read_to_string(&record).unwrap_or_default()
    }

    /// rc-notify mirrors its event to paseo when — and only when — it runs
    /// inside a terminal the paseo daemon spawned.
    ///
    /// The `idle_prompt` stdin sentinel is load-bearing, not decoration: paseo's
    /// claude hook provider resolves a `Notification` to `needs-input` *only* for
    /// that marker and registers no `PermissionRequest` hook at all, so without
    /// it a claude permission prompt raises nothing on the phone.
    #[test]
    #[cfg(unix)]
    fn ta172_rc_notify_reports_paseo_terminal_activity() {
        let dir = std::env::temp_dir().join(format!("introdus-rcnotify-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        materialize(&dir).unwrap();

        let waiting = run_rc_notify(&dir, "waiting", Some("term-1"));
        assert!(
            waiting.contains("hooks claude Notification"),
            "waiting must report a Notification: {waiting:?}"
        );
        assert!(
            waiting.contains("idle_prompt"),
            "waiting must carry the idle_prompt sentinel on stdin: {waiting:?}"
        );

        let done = run_rc_notify(&dir, "done", Some("term-1"));
        assert!(
            done.contains("hooks claude Stop"),
            "done must report Stop (→ idle): {done:?}"
        );

        // Outside a paseo terminal the bridge stays silent — an ordinary
        // run-claude window keeps the host FIFO as its only path.
        assert!(
            run_rc_notify(&dir, "waiting", None).is_empty(),
            "no PASEO_TERMINAL_ID must mean no paseo call"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ta22_materialize_writes_tree_with_modes() {
        let dir = std::env::temp_dir().join(format!("introdus-assets-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        materialize(&dir).unwrap();

        assert!(dockerfile(&dir).is_file());
        assert!(setup_script(&dir).is_file());
        assert!(tinyproxy_conf(&dir).is_file());
        let entry = entrypoint(&dir);
        assert!(entry.is_file());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&entry).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "entrypoint must be executable");
            let conf_mode = std::fs::metadata(tinyproxy_conf(&dir))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(conf_mode & 0o777, 0o644, "conf must not be executable");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
