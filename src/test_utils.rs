use bytes::Bytes;
use http_body_util::Full;
use serde_json::Value;
use std::sync::Arc;

/// Create a fake kube::Client backed by a closure that returns (status, json) for each request.
pub fn fake_k8s(
    handler: impl Fn(&http::Uri, &http::Method) -> (u16, Value) + Send + Sync + 'static,
) -> kube::Client {
    let handler = Arc::new(handler);
    let svc = tower::service_fn(move |req: http::Request<_>| {
        let handler = handler.clone();
        async move {
            let (status, json) = handler(req.uri(), req.method());
            let body = Full::new(Bytes::from(serde_json::to_vec(&json).unwrap()));
            Ok::<_, std::convert::Infallible>(
                http::Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(body)
                    .unwrap(),
            )
        }
    });
    kube::Client::new(svc, "default")
}

/// Build a k8s ServiceList JSON response.
pub fn service_list_json(items: Vec<Value>) -> Value {
    serde_json::json!({
        "kind": "ServiceList",
        "apiVersion": "v1",
        "metadata": {"resourceVersion": "1"},
        "items": items
    })
}

/// Build a single k8s Service JSON object.
pub fn service_json(name: &str, ns: &str, cluster_ip: &str, svc_type: &str, ports: Vec<i32>) -> Value {
    serde_json::json!({
        "metadata": {
            "name": name,
            "namespace": ns,
            "uid": format!("uid-{name}"),
            "resourceVersion": "1",
            "creationTimestamp": "2024-01-01T00:00:00Z"
        },
        "spec": {
            "type": svc_type,
            "clusterIP": cluster_ip,
            "selector": {"app": name},
            "ports": ports.iter().map(|p| serde_json::json!({"port": p, "protocol": "TCP"})).collect::<Vec<_>>()
        }
    })
}

/// Build a k8s error Status JSON (e.g. for 422 responses).
pub fn error_status_json(code: u16, message: &str) -> Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": message,
        "reason": "Invalid",
        "code": code
    })
}

/// Build a k8s PodList JSON response.
pub fn pod_list_json(items: Vec<Value>) -> Value {
    serde_json::json!({
        "kind": "PodList",
        "apiVersion": "v1",
        "metadata": {"resourceVersion": "1"},
        "items": items
    })
}

/// Build a k8s Pod JSON with ready condition.
pub fn ready_pod_json(name: &str, ns: &str) -> Value {
    serde_json::json!({
        "metadata": {
            "name": name,
            "namespace": ns,
            "uid": format!("uid-{name}"),
            "resourceVersion": "1",
            "creationTimestamp": "2024-01-01T00:00:00Z"
        },
        "spec": {
            "containers": [{"name": "main", "image": "nginx:latest"}]
        },
        "status": {
            "phase": "Running",
            "conditions": [
                {"type": "Ready", "status": "True"}
            ]
        }
    })
}
