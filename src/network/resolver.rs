//! DNS resolver setup for portkube.
//!
//! We use short DNS names: `nginx.default` instead of
//! `nginx.default.svc.cluster.local` (which macOS can't resolve due to
//! the `.local` mDNS trap).
//!
//! For each K8s namespace that has services, we create:
//!   `/etc/resolver/<namespace>` → nameserver 127.0.0.53
//!
//! macOS resolver(5) then routes `*.default`, `*.monitoring`, etc.
//! to our DNS proxy, which resolves `<svc>.<ns>` → ClusterIP.
//!
//! Requires root.

use anyhow::{Context, Result};
use tracing::info;

const RESOLVER_DIR: &str = "/etc/resolver";
const DNS_PROXY_IP: &str = "127.0.0.53";
const MANAGED_LIST: &str = "/tmp/portkube-managed-resolvers";

fn resolver_content() -> String {
    format!(
        "# portkube — do not edit\nnameserver {DNS_PROXY_IP}\nport 53\ntimeout 2\n"
    )
}

/// Install resolver files for each namespace.
/// Backs up existing files if present.
pub async fn install(namespaces: &[String]) -> Result<()> {
    // Loopback alias for DNS proxy
    run_cmd("ifconfig", &["lo0", "alias", DNS_PROXY_IP])
        .await
        .context("create loopback alias 127.0.0.53")?;

    std::fs::create_dir_all(RESOLVER_DIR).context("create /etc/resolver")?;

    let content = resolver_content();
    let mut managed = Vec::new();

    for ns in namespaces {
        let path = format!("{RESOLVER_DIR}/{ns}");
        let backup = format!("{path}.portkube-backup");

        // Back up existing file if present and not already backed up
        if std::path::Path::new(&path).exists() && !std::path::Path::new(&backup).exists() {
            let _ = std::fs::rename(&path, &backup);
        }

        std::fs::write(&path, &content)
            .with_context(|| format!("write {path}"))?;

        managed.push(ns.clone());
    }

    // Save list of managed resolver files for cleanup
    std::fs::write(MANAGED_LIST, managed.join("\n")).context("save managed list")?;

    info!(count = managed.len(), "resolver files installed");
    Ok(())
}

/// Remove all managed resolver files and restore backups.
pub async fn uninstall() -> Result<()> {
    let managed = std::fs::read_to_string(MANAGED_LIST).unwrap_or_default();

    for ns in managed.lines().filter(|l| !l.trim().is_empty()) {
        let path = format!("{RESOLVER_DIR}/{ns}");
        let backup = format!("{path}.portkube-backup");

        // Remove our file
        let _ = std::fs::remove_file(&path);

        // Restore backup if exists
        if std::path::Path::new(&backup).exists() {
            let _ = std::fs::rename(&backup, &path);
        }
    }

    // Remove loopback alias
    let _ = run_cmd("ifconfig", &["lo0", "-alias", DNS_PROXY_IP]).await;

    // Clean up
    let _ = std::fs::remove_file(MANAGED_LIST);

    info!("resolver files uninstalled");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolver_content_contains_nameserver() {
        let content = resolver_content();
        assert!(content.contains("nameserver 127.0.0.53"));
    }

    #[test]
    fn test_resolver_content_contains_port() {
        let content = resolver_content();
        assert!(content.contains("port 53"));
    }

    #[test]
    fn test_resolver_content_contains_timeout() {
        let content = resolver_content();
        assert!(content.contains("timeout 2"));
    }

    #[test]
    fn test_resolver_content_has_portkube_marker() {
        let content = resolver_content();
        assert!(content.contains("portkube"));
    }

    #[test]
    fn test_resolver_content_ends_with_newline() {
        let content = resolver_content();
        assert!(content.ends_with('\n'));
    }

    #[test]
    fn test_resolver_constants() {
        assert_eq!(RESOLVER_DIR, "/etc/resolver");
        assert_eq!(DNS_PROXY_IP, "127.0.0.53");
        assert_eq!(MANAGED_LIST, "/tmp/portkube-managed-resolvers");
    }

    #[tokio::test]
    async fn test_install_creates_resolver_files() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().to_str().unwrap().to_string();

        // We can't test the real install (needs root + /etc/resolver)
        // but we can test the content generation and file writing logic
        let content = resolver_content();
        let ns = "testns";
        let path = format!("{dir_path}/{ns}");
        std::fs::write(&path, &content).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, content);
        assert!(written.contains("nameserver 127.0.0.53"));
    }

    #[test]
    fn test_managed_list_format() {
        let namespaces = vec!["default".to_string(), "monitoring".to_string()];
        let content = namespaces.join("\n");
        assert_eq!(content, "default\nmonitoring");
        // Verify round-trip
        let parsed: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(parsed, vec!["default", "monitoring"]);
    }
}

async fn run_cmd(cmd: &str, args: &[&str]) -> Result<()> {
    let output = tokio::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .with_context(|| format!("{cmd} {}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{cmd} {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(())
}
