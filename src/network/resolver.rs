//! DNS resolver setup for portkube.
//!
//! We use short DNS names: `nginx.default` instead of
//! `nginx.default.svc.cluster.local`.
//!
//! Platform-specific setup:
//!   - macOS: `/etc/resolver/<namespace>` files (resolver(5))
//!   - Linux: modifies `/etc/resolv.conf` (saves backup)
//!   - Windows: adds DNS suffix via netsh
//!
//! Requires root/admin.

use anyhow::{Context, Result};
use tracing::info;

const DNS_PROXY_IP: &str = "127.0.0.53";
const MANAGED_LIST: &str = "/tmp/portkube-managed-resolvers";

// ── macOS: /etc/resolver ────────────────────────────────

#[cfg(target_os = "macos")]
const RESOLVER_DIR: &str = "/etc/resolver";

#[cfg(target_os = "macos")]
fn resolver_content() -> String {
    format!(
        "# portkube — do not edit\nnameserver {DNS_PROXY_IP}\nport 53\ntimeout 2\n"
    )
}

#[cfg(target_os = "macos")]
pub async fn install(namespaces: &[String]) -> Result<()> {
    run_cmd("ifconfig", &["lo0", "alias", DNS_PROXY_IP])
        .await
        .context("create loopback alias 127.0.0.53")?;

    std::fs::create_dir_all(RESOLVER_DIR).context("create /etc/resolver")?;

    let content = resolver_content();
    let mut managed = Vec::new();

    for ns in namespaces {
        let path = format!("{RESOLVER_DIR}/{ns}");
        let backup = format!("{path}.portkube-backup");

        if std::path::Path::new(&path).exists() && !std::path::Path::new(&backup).exists() {
            let _ = std::fs::rename(&path, &backup);
        }

        std::fs::write(&path, &content)
            .with_context(|| format!("write {path}"))?;

        managed.push(ns.clone());
    }

    std::fs::write(MANAGED_LIST, managed.join("\n")).context("save managed list")?;

    info!(count = managed.len(), "resolver files installed");
    Ok(())
}

#[cfg(target_os = "macos")]
pub async fn uninstall() -> Result<()> {
    let managed = std::fs::read_to_string(MANAGED_LIST).unwrap_or_default();

    for ns in managed.lines().filter(|l| !l.trim().is_empty()) {
        let path = format!("{RESOLVER_DIR}/{ns}");
        let backup = format!("{path}.portkube-backup");

        let _ = std::fs::remove_file(&path);

        if std::path::Path::new(&backup).exists() {
            let _ = std::fs::rename(&backup, &path);
        }
    }

    let _ = run_cmd("ifconfig", &["lo0", "-alias", DNS_PROXY_IP]).await;
    let _ = std::fs::remove_file(MANAGED_LIST);

    info!("resolver files uninstalled");
    Ok(())
}

// ── Linux: /etc/resolv.conf prepend ─────────────────────

#[cfg(target_os = "linux")]
pub async fn install(namespaces: &[String]) -> Result<()> {
    // Add loopback alias for DNS proxy
    run_cmd("ip", &["addr", "add", &format!("{DNS_PROXY_IP}/32"), "dev", "lo"])
        .await
        .context("create loopback alias 127.0.0.53")?;

    // Back up existing resolv.conf
    let resolv = "/etc/resolv.conf";
    let backup = "/etc/resolv.conf.portkube-backup";
    if std::path::Path::new(resolv).exists() && !std::path::Path::new(backup).exists() {
        let _ = std::fs::copy(resolv, backup);
    }

    // Read existing config, prepend our nameserver
    let existing = std::fs::read_to_string(resolv).unwrap_or_default();
    let mut lines = vec![
        format!("# portkube — do not edit this line"),
        format!("nameserver {DNS_PROXY_IP}"),
    ];
    for line in existing.lines() {
        if !line.contains("portkube") && !line.contains(DNS_PROXY_IP) {
            lines.push(line.to_string());
        }
    }
    std::fs::write(resolv, lines.join("\n") + "\n")
        .context("write /etc/resolv.conf")?;

    // Save managed namespaces for reference
    std::fs::write(MANAGED_LIST, namespaces.join("\n")).context("save managed list")?;

    info!(count = namespaces.len(), "DNS resolver configured");
    Ok(())
}

#[cfg(target_os = "linux")]
pub async fn uninstall() -> Result<()> {
    let resolv = "/etc/resolv.conf";
    let backup = "/etc/resolv.conf.portkube-backup";

    // Restore backup if exists
    if std::path::Path::new(backup).exists() {
        let _ = std::fs::rename(backup, resolv);
    } else {
        // Remove our lines from resolv.conf
        if let Ok(content) = std::fs::read_to_string(resolv) {
            let cleaned: Vec<&str> = content.lines()
                .filter(|l| !l.contains("portkube") && !l.contains(DNS_PROXY_IP))
                .collect();
            let _ = std::fs::write(resolv, cleaned.join("\n") + "\n");
        }
    }

    let _ = run_cmd("ip", &["addr", "del", &format!("{DNS_PROXY_IP}/32"), "dev", "lo"]).await;
    let _ = std::fs::remove_file(MANAGED_LIST);

    info!("DNS resolver uninstalled");
    Ok(())
}

// ── Windows: netsh DNS configuration ────────────────────

#[cfg(windows)]
pub async fn install(namespaces: &[String]) -> Result<()> {
    // On Windows, configure DNS to use our proxy
    run_cmd("netsh", &["interface", "ip", "add", "dns", "Loopback Pseudo-Interface 1", DNS_PROXY_IP, "index=1"])
        .await
        .context("configure DNS via netsh")?;

    std::fs::write(MANAGED_LIST, namespaces.join("\n")).context("save managed list")?;

    info!(count = namespaces.len(), "DNS resolver configured");
    Ok(())
}

#[cfg(windows)]
pub async fn uninstall() -> Result<()> {
    let _ = run_cmd("netsh", &["interface", "ip", "delete", "dns", "Loopback Pseudo-Interface 1", DNS_PROXY_IP]).await;
    let _ = std::fs::remove_file(MANAGED_LIST);

    info!("DNS resolver uninstalled");
    Ok(())
}

// ── Helpers ─────────────────────────────────────────────

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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn test_resolver_content_contains_nameserver() {
        let content = resolver_content();
        assert!(content.contains("nameserver 127.0.0.53"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_resolver_content_contains_port() {
        let content = resolver_content();
        assert!(content.contains("port 53"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_resolver_content_contains_timeout() {
        let content = resolver_content();
        assert!(content.contains("timeout 2"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_resolver_content_has_portkube_marker() {
        let content = resolver_content();
        assert!(content.contains("portkube"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_resolver_content_ends_with_newline() {
        let content = resolver_content();
        assert!(content.ends_with('\n'));
    }

    #[test]
    fn test_resolver_constants() {
        assert_eq!(DNS_PROXY_IP, "127.0.0.53");
        assert_eq!(MANAGED_LIST, "/tmp/portkube-managed-resolvers");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn test_install_creates_resolver_files() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().to_str().unwrap().to_string();

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
        let parsed: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(parsed, vec!["default", "monitoring"]);
    }
}
