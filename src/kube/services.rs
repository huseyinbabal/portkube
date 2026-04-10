use anyhow::Result;
use k8s_openapi::api::core::v1::Service;
use kube::{api::ListParams, Api};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct KubeService {
    pub name: String,
    pub namespace: String,
    pub service_type: String,
    pub cluster_ip: String,
    pub ports: Vec<ServicePort>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ServicePort {
    pub port: i32,
    pub target_port: String,
    pub protocol: String,
    pub name: Option<String>,
}

impl KubeService {
    /// A human-friendly port summary like "8080/TCP, 443/TCP"
    pub fn ports_display(&self) -> String {
        self.ports
            .iter()
            .map(|p| format!("{}/{}", p.port, p.protocol))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The first HTTP-ish url you'd try in a browser
    #[allow(dead_code)]
    pub fn local_url(&self, ip: &str) -> Option<String> {
        self.ports.first().map(|p| {
            let scheme = if p.port == 443 { "https" } else { "http" };
            format!("{}://{}:{}", scheme, ip, p.port)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_svc(ports: Vec<ServicePort>) -> KubeService {
        KubeService {
            name: "nginx".into(),
            namespace: "default".into(),
            service_type: "ClusterIP".into(),
            cluster_ip: "10.96.0.1".into(),
            ports,
        }
    }

    fn tcp_port(port: i32) -> ServicePort {
        ServicePort {
            port,
            target_port: port.to_string(),
            protocol: "TCP".into(),
            name: None,
        }
    }

    #[test]
    fn test_ports_display_single() {
        let svc = make_svc(vec![tcp_port(80)]);
        assert_eq!(svc.ports_display(), "80/TCP");
    }

    #[test]
    fn test_ports_display_multiple() {
        let svc = make_svc(vec![tcp_port(80), tcp_port(443)]);
        assert_eq!(svc.ports_display(), "80/TCP, 443/TCP");
    }

    #[test]
    fn test_ports_display_empty() {
        let svc = make_svc(vec![]);
        assert_eq!(svc.ports_display(), "");
    }

    #[test]
    fn test_ports_display_udp() {
        let svc = make_svc(vec![ServicePort {
            port: 53,
            target_port: "53".into(),
            protocol: "UDP".into(),
            name: Some("dns".into()),
        }]);
        assert_eq!(svc.ports_display(), "53/UDP");
    }

    #[test]
    fn test_local_url_http() {
        let svc = make_svc(vec![tcp_port(8080)]);
        assert_eq!(svc.local_url("10.96.0.1"), Some("http://10.96.0.1:8080".into()));
    }

    #[test]
    fn test_local_url_https() {
        let svc = make_svc(vec![tcp_port(443)]);
        assert_eq!(svc.local_url("10.96.0.1"), Some("https://10.96.0.1:443".into()));
    }

    #[test]
    fn test_local_url_port_80() {
        let svc = make_svc(vec![tcp_port(80)]);
        assert_eq!(svc.local_url("10.96.0.1"), Some("http://10.96.0.1:80".into()));
    }

    #[test]
    fn test_local_url_no_ports() {
        let svc = make_svc(vec![]);
        assert_eq!(svc.local_url("10.96.0.1"), None);
    }

    // ── Fake k8s integration tests ──────────────────────────

    use crate::test_utils::*;

    #[tokio::test]
    async fn test_list_services_all_namespaces() {
        let client = fake_k8s(|uri, _method| {
            assert!(uri.path().contains("/services"));
            (200, service_list_json(vec![
                service_json("nginx", "default", "10.96.0.10", "ClusterIP", vec![80]),
                service_json("api", "prod", "10.96.0.20", "NodePort", vec![8080, 443]),
            ]))
        });

        let svcs = list_services(&client, None).await.unwrap();
        assert_eq!(svcs.len(), 2);

        assert_eq!(svcs[0].name, "nginx");
        assert_eq!(svcs[0].namespace, "default");
        assert_eq!(svcs[0].service_type, "ClusterIP");
        assert_eq!(svcs[0].cluster_ip, "10.96.0.10");
        assert_eq!(svcs[0].ports.len(), 1);
        assert_eq!(svcs[0].ports[0].port, 80);
        assert_eq!(svcs[0].ports[0].protocol, "TCP");

        assert_eq!(svcs[1].name, "api");
        assert_eq!(svcs[1].namespace, "prod");
        assert_eq!(svcs[1].service_type, "NodePort");
        assert_eq!(svcs[1].ports.len(), 2);
    }

    #[tokio::test]
    async fn test_list_services_namespaced() {
        let client = fake_k8s(|uri, _method| {
            assert!(uri.path().contains("/namespaces/monitoring/services"));
            (200, service_list_json(vec![
                service_json("grafana", "monitoring", "10.96.1.5", "ClusterIP", vec![3000]),
            ]))
        });

        let svcs = list_services(&client, Some("monitoring")).await.unwrap();
        assert_eq!(svcs.len(), 1);
        assert_eq!(svcs[0].name, "grafana");
        assert_eq!(svcs[0].namespace, "monitoring");
    }

    #[tokio::test]
    async fn test_list_services_empty() {
        let client = fake_k8s(|_uri, _method| {
            (200, service_list_json(vec![]))
        });

        let svcs = list_services(&client, None).await.unwrap();
        assert!(svcs.is_empty());
    }

    #[tokio::test]
    async fn test_list_services_no_spec_filtered() {
        let client = fake_k8s(|_uri, _method| {
            // Service with no spec → filtered out by filter_map
            (200, service_list_json(vec![serde_json::json!({
                "metadata": {"name": "headless", "namespace": "default"},
            })]))
        });

        let svcs = list_services(&client, None).await.unwrap();
        assert!(svcs.is_empty());
    }

    #[tokio::test]
    async fn test_list_services_missing_name_filtered() {
        let client = fake_k8s(|_uri, _method| {
            // Service with no name → filtered out
            (200, service_list_json(vec![serde_json::json!({
                "metadata": {"namespace": "default"},
                "spec": {"ports": [{"port": 80}]}
            })]))
        });

        let svcs = list_services(&client, None).await.unwrap();
        assert!(svcs.is_empty());
    }

    #[tokio::test]
    async fn test_list_services_defaults_for_optional_fields() {
        let client = fake_k8s(|_uri, _method| {
            // Service with minimal fields — optional fields should get defaults
            (200, service_list_json(vec![serde_json::json!({
                "metadata": {"name": "minimal"},
                "spec": {"ports": [{"port": 80}]}
            })]))
        });

        let svcs = list_services(&client, None).await.unwrap();
        assert_eq!(svcs.len(), 1);
        assert_eq!(svcs[0].namespace, "default");
        assert_eq!(svcs[0].service_type, "ClusterIP");
        assert_eq!(svcs[0].cluster_ip, "-");
        assert_eq!(svcs[0].ports[0].protocol, "TCP");
    }

    #[tokio::test]
    async fn test_list_services_int_or_string_target_port() {
        let client = fake_k8s(|_uri, _method| {
            (200, service_list_json(vec![serde_json::json!({
                "metadata": {"name": "web", "namespace": "default"},
                "spec": {
                    "ports": [
                        {"port": 80, "targetPort": 8080},
                        {"port": 443, "targetPort": "https"}
                    ]
                }
            })]))
        });

        let svcs = list_services(&client, None).await.unwrap();
        assert_eq!(svcs[0].ports[0].target_port, "8080");
        assert_eq!(svcs[0].ports[1].target_port, "https");
    }
}

/// List services across all namespaces (or a single one)
pub async fn list_services(
    client: &kube::Client,
    namespace: Option<&str>,
) -> Result<Vec<KubeService>> {
    let svcs: Vec<Service> = match namespace {
        Some(ns) => {
            let api: Api<Service> = Api::namespaced(client.clone(), ns);
            api.list(&ListParams::default()).await?.items
        }
        None => {
            let api: Api<Service> = Api::all(client.clone());
            api.list(&ListParams::default()).await?.items
        }
    };

    let out = svcs
        .into_iter()
        .filter_map(|svc| {
            let meta = svc.metadata;
            let spec = svc.spec?;
            let name = meta.name?;
            let ns = meta.namespace.unwrap_or("default".into());

            let ports = spec
                .ports
                .unwrap_or_default()
                .into_iter()
                .map(|p| {
                    use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
                    ServicePort {
                        port: p.port,
                        target_port: match p.target_port {
                            Some(IntOrString::Int(i)) => i.to_string(),
                            Some(IntOrString::String(s)) => s,
                            None => p.port.to_string(),
                        },
                        protocol: p.protocol.unwrap_or("TCP".into()),
                        name: p.name,
                    }
                })
                .collect();

            Some(KubeService {
                name,
                namespace: ns,
                service_type: spec.type_.unwrap_or("ClusterIP".into()),
                cluster_ip: spec.cluster_ip.unwrap_or("-".into()),
                ports,
            })
        })
        .collect();

    Ok(out)
}
