use anyhow::Result;
use kube::config::Kubeconfig;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct KubeContext {
    pub name: String,
    pub cluster: String,
    pub namespace: String,
    pub user: String,
    pub is_active: bool,
}

/// Read all contexts from ~/.kube/config
pub fn list_contexts() -> Result<Vec<KubeContext>> {
    let kubeconfig = Kubeconfig::read()?;
    Ok(contexts_from_kubeconfig(&kubeconfig))
}

/// Extract contexts from a parsed Kubeconfig (testable without filesystem).
pub fn contexts_from_kubeconfig(kubeconfig: &Kubeconfig) -> Vec<KubeContext> {
    let current = kubeconfig.current_context.clone().unwrap_or_default();

    kubeconfig
        .contexts
        .iter()
        .map(|named| {
            let ctx = named.context.as_ref();
            KubeContext {
                is_active: named.name == current,
                name: named.name.clone(),
                cluster: ctx
                    .map(|c| c.cluster.clone())
                    .unwrap_or_else(|| "-".into()),
                namespace: ctx
                    .and_then(|c| c.namespace.clone())
                    .unwrap_or_else(|| "default".into()),
                user: ctx
                    .and_then(|c| c.user.clone())
                    .unwrap_or_else(|| "-".into()),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::config::{Context, NamedContext};

    fn make_kubeconfig(
        contexts: Vec<NamedContext>,
        current: Option<String>,
    ) -> Kubeconfig {
        Kubeconfig {
            current_context: current,
            contexts,
            ..Default::default()
        }
    }

    fn named_ctx(name: &str, cluster: &str, ns: Option<&str>, user: Option<&str>) -> NamedContext {
        NamedContext {
            name: name.into(),
            context: Some(Context {
                cluster: cluster.into(),
                namespace: ns.map(|s| s.into()),
                user: user.map(|s| s.into()),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn test_contexts_from_kubeconfig_basic() {
        let cfg = make_kubeconfig(
            vec![named_ctx("dev", "dev-cluster", Some("default"), Some("dev-user"))],
            Some("dev".into()),
        );
        let ctxs = contexts_from_kubeconfig(&cfg);
        assert_eq!(ctxs.len(), 1);
        assert_eq!(ctxs[0].name, "dev");
        assert_eq!(ctxs[0].cluster, "dev-cluster");
        assert_eq!(ctxs[0].namespace, "default");
        assert_eq!(ctxs[0].user, "dev-user");
        assert!(ctxs[0].is_active);
    }

    #[test]
    fn test_contexts_active_flag() {
        let cfg = make_kubeconfig(
            vec![
                named_ctx("dev", "c1", Some("ns1"), Some("u1")),
                named_ctx("prod", "c2", Some("ns2"), Some("u2")),
            ],
            Some("prod".into()),
        );
        let ctxs = contexts_from_kubeconfig(&cfg);
        assert!(!ctxs[0].is_active);
        assert!(ctxs[1].is_active);
    }

    #[test]
    fn test_contexts_defaults_when_fields_missing() {
        let cfg = make_kubeconfig(
            vec![NamedContext {
                name: "minimal".into(),
                context: Some(Context {
                    cluster: "c".into(),
                    namespace: None,
                    user: None,
                    ..Default::default()
                }),
            }],
            None,
        );
        let ctxs = contexts_from_kubeconfig(&cfg);
        assert_eq!(ctxs[0].namespace, "default");
        assert_eq!(ctxs[0].user, "-");
        assert!(!ctxs[0].is_active);
    }

    #[test]
    fn test_contexts_no_context_body() {
        let cfg = make_kubeconfig(
            vec![NamedContext { name: "empty".into(), context: None }],
            None,
        );
        let ctxs = contexts_from_kubeconfig(&cfg);
        assert_eq!(ctxs[0].cluster, "-");
        assert_eq!(ctxs[0].namespace, "default");
        assert_eq!(ctxs[0].user, "-");
    }

    #[test]
    fn test_contexts_empty_kubeconfig() {
        let cfg = make_kubeconfig(vec![], None);
        let ctxs = contexts_from_kubeconfig(&cfg);
        assert!(ctxs.is_empty());
    }

    #[test]
    fn test_contexts_no_current_context() {
        let cfg = make_kubeconfig(
            vec![named_ctx("dev", "c1", Some("ns"), Some("u"))],
            None,
        );
        let ctxs = contexts_from_kubeconfig(&cfg);
        assert!(!ctxs[0].is_active);
    }
}

/// Build a kube::Client for the given context name
pub async fn client_for_context(name: &str) -> Result<kube::Client> {
    let kubeconfig = Kubeconfig::read()?;
    let opts = kube::config::KubeConfigOptions {
        context: Some(name.into()),
        ..Default::default()
    };
    let config = kube::Config::from_custom_kubeconfig(kubeconfig, &opts).await?;
    Ok(kube::Client::try_from(config)?)
}
