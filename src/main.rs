use axum::{
    body::{to_bytes, Body},
    extract::{Path, State, ConnectInfo},
    http::{Request, StatusCode},
    response::{sse::Event, IntoResponse, Sse},
    routing::{get, post},
    Json, Router,
};
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
// CloudEvents support - we'll parse JSON directly
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    convert::Infallible,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{broadcast, RwLock};
use tokio_stream::{wrappers::BroadcastStream, Stream as _, StreamExt};
use tower_http::services::ServeDir;
use tracing::{error, info, warn};
use kube::{Api, Client, ResourceExt};
use k8s_openapi::api::core::v1::{Service, Node};

/// Package version from Cargo.toml (set at compile time by Cargo).
fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Format a host for use in a URL: wrap IPv6 addresses in brackets so host:port is unambiguous.
fn host_for_url(host: &str) -> String {
    if host.parse::<Ipv6Addr>().is_ok() {
        format!("[{}]", host)
    } else {
        host.to_string()
    }
}

/// Format source part of connection line: "ip:port" for IPv4, "[ip]:port" for IPv6.
fn format_connection_source(addr: SocketAddr) -> String {
    match addr.ip() {
        IpAddr::V4(_) => format!("{}:{}", addr.ip(), addr.port()),
        IpAddr::V6(_) => format!("[{}]:{}", addr.ip(), addr.port()),
    }
}

// Application state
#[derive(Clone)]
struct AppState {
    // Channel for broadcasting PTP status updates
    tx: broadcast::Sender<PtpStatus>,
    // Last broadcast status (merge os_clock_sync_state, lock_state, clock_class into every event so UI has full state)
    last_ptp_status: Arc<RwLock<Option<PtpStatus>>>,
    // HTTP client for making requests to cloud-event-proxy
    client: reqwest::Client,
    // Kubernetes client
    k8s_client: Client,
    // Cached PTP configuration
    ptp_config: Arc<PtpConfig>,
}

// PTP configuration discovered from Kubernetes
#[derive(Clone, Debug)]
struct PtpConfig {
    proxy_url: String,
    callback_uri: String,
    node_name: String,
    ptp_namespace: String,
}

// PTP Status structure
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PtpStatus {
    timestamp: String,
    os_clock_sync_state: Option<String>,
    lock_state: Option<String>,
    clock_class: Option<u8>,
    /// HTTP request line: method, uri, version (for Event Log in UI)
    #[serde(skip_serializing_if = "Option::is_none")]
    http_method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    http_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    http_version: Option<String>,
    /// Connection line: source_ip:port -> dest:port
    #[serde(skip_serializing_if = "Option::is_none")]
    connection_line: Option<String>,
    /// Request headers (for Event Log in UI)
    #[serde(skip_serializing_if = "Option::is_none")]
    request_headers: Option<HashMap<String, String>>,
    /// Exact JSON body of the POST (for Event Log in UI; same as what PTP fast event proxy sent)
    #[serde(skip_serializing_if = "Option::is_none")]
    request_body: Option<Value>,
    #[serde(flatten)]
    additional: HashMap<String, Value>,
}

// Subscription request structure
#[derive(Serialize, Deserialize, Debug)]
struct SubscriptionRequest {
    #[serde(rename = "EndpointUri")]
    endpoint_uri: String,
    #[serde(rename = "ResourceAddress")]
    resource_address: String,
}

// Publisher structure from /publishers endpoint
#[derive(Deserialize, Debug)]
struct Publisher {
    #[serde(rename = "ResourceAddress")]
    ResourceAddress: String,
    #[serde(rename = "EndpointURI")]
    EndpointURI: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing - only show PTP Event API logs
    tracing_subscriber::fmt()
        .with_env_filter("ptp_fast_event_consumer=info")
        .with_target(false)
        .init();

    info!("Starting PTP Fast Event Consumer (version: {})", app_version());

    // Create Kubernetes client
    let k8s_client = Client::try_default().await?;
    info!("Kubernetes client initialized");

    // Discover PTP configuration
    let ptp_config = discover_ptp_config(&k8s_client).await?;
    info!("PTP configuration discovered: proxy_url={}, callback_uri={}, node_name={}", 
          ptp_config.proxy_url, ptp_config.callback_uri, ptp_config.node_name);

    // Create broadcast channel for SSE
    let (tx, _rx) = broadcast::channel::<PtpStatus>(100);

    // Create HTTP client
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    let state = AppState { 
        tx, 
        last_ptp_status: Arc::new(RwLock::new(None)),
        client: client.clone(),
        k8s_client: k8s_client.clone(),
        ptp_config: Arc::new(ptp_config),
    };

    // Subscribe to PTP events on startup
    let subscribe_state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = subscribe_to_ptp_events(&subscribe_state).await {
            error!("Failed to subscribe to PTP events: {}", e);
        }
    });

    // Build the router
    let app = Router::new()
        .route("/api/events", post(handle_cloudevent))
        .route("/api/events", get(handle_validation)) // GET handler for cloud-event-proxy validation
        .route("/api/status", get(get_status))
        .route("/api/version", get(get_version))
        .route("/api/sse", get(sse_handler))
        .route("/api/subscriptions", get(list_subscriptions))
        .route("/api/subscriptions/:id", axum::routing::delete(unsubscribe))
        .route("/api/subscriptions/resubscribe", post(resubscribe_events))
        .nest_service("/", ServeDir::new("static"))
        .with_state(state);

    // Start the server with ConnectInfo support (bind to all interfaces, IPv4 and IPv6)
    let listener = tokio::net::TcpListener::bind("[::]:8080").await?;
    info!("Server listening on http://[::]:8080 (IPv4 and IPv6)");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;

    Ok(())
}

// Discover PTP configuration from Kubernetes
async fn discover_ptp_config(k8s_client: &Client) -> anyhow::Result<PtpConfig> {
    // Get node name from environment
    let node_name_env = std::env::var("NODE_NAME")
        .unwrap_or_else(|_| "unknown".to_string());
    
    // Get PTP namespace
    let ptp_namespace = std::env::var("PTP_NAMESPACE")
        .unwrap_or_else(|_| "openshift-ptp".to_string());
    
    // Get our own namespace
    let pod_namespace = std::env::var("POD_NAMESPACE")
        .unwrap_or_else(|_| "ptp-consumer".to_string());
    
    // Discover PTP service
    let services: Api<Service> = Api::namespaced(k8s_client.clone(), &ptp_namespace);
    let service_list = services.list(&Default::default()).await?;
    
    // Find the PTP event publisher service (typically named like ptp-event-publisher-service-<node>)
    let ptp_service = service_list.iter()
        .find(|svc| {
            let name = svc.name_any();
            name.starts_with("ptp-event-publisher-service") || 
            name.contains("ptp-event") || 
            name.contains("cloud-event-proxy")
        })
        .ok_or_else(|| anyhow::anyhow!("PTP event publisher service not found in namespace {}. Available services: {}", 
            ptp_namespace, 
            service_list.iter().map(|s| s.name_any()).collect::<Vec<_>>().join(", ")))?;
    
    // Get service ClusterIP or construct DNS name (IPv6 ClusterIPs need brackets in URLs)
    let proxy_url = if let Some(cluster_ip) = ptp_service.spec.as_ref()
        .and_then(|spec| spec.cluster_ip.as_ref())
        .filter(|ip| !ip.is_empty() && *ip != "None") {
        format!("http://{}:9043", host_for_url(cluster_ip))
    } else {
        // Fall back to DNS name
        format!("http://{}.{}.svc.cluster.local:9043", ptp_service.name_any(), ptp_namespace)
    };
    
    info!("Discovered PTP service: {} at {}", ptp_service.name_any(), proxy_url);
    
    // Discover our own service and use Kubernetes DNS name for callback URI
    let our_services: Api<Service> = Api::namespaced(k8s_client.clone(), &pod_namespace);
    let our_service_list = our_services.list(&Default::default()).await?;
    
    let our_service = our_service_list.iter()
        .find(|svc| svc.name_any().contains("ptp-fast-event-consumer"))
        .ok_or_else(|| anyhow::anyhow!("ptp-fast-event-consumer service not found in namespace {}", pod_namespace))?;
    
    let callback_uri = format!(
        "http://{}.{}.svc.cluster.local:8080/api/events",
        our_service.name_any(),
        pod_namespace
    );
    
    info!("Discovered callback URI: {}", callback_uri);
    
    // Get full node name from Kubernetes Node resource
    let full_node_name = {
        let nodes: Api<Node> = Api::all(k8s_client.clone());
        match nodes.get(&node_name_env).await {
            Ok(node) => {
                node.metadata.name
                    .unwrap_or_else(|| node_name_env.clone())
            }
            Err(_) => {
                warn!("Could not fetch node {} from Kubernetes, using node name from environment: {}", node_name_env, node_name_env);
                node_name_env
            }
        }
    };
    
    info!("Using node name: {}", full_node_name);
    
    Ok(PtpConfig {
        proxy_url,
        callback_uri,
        node_name: full_node_name,
        ptp_namespace,
    })
}

// Subscribe to PTP events from cloud-event-proxy
async fn subscribe_to_ptp_events(
    state: &AppState,
) -> anyhow::Result<()> {
    info!("Subscribing to PTP events...");

    let config = &state.ptp_config;
    
    // Build resource addresses dynamically based on discovered node name
    let subscriptions = vec![
        format!("/cluster/node/{}/sync/sync-status/os-clock-sync-state", config.node_name),
        format!("/cluster/node/{}/sync/ptp-status/lock-state", config.node_name),
        format!("/cluster/node/{}/sync/ptp-status/clock-class", config.node_name),
    ];
    
    // Subscribe to each resource
    for resource_address in subscriptions {
        let subscription = SubscriptionRequest {
            endpoint_uri: config.callback_uri.clone(),
            resource_address: resource_address.clone(),
        };
        
        // Debug: log the subscription request
        let json_payload = serde_json::to_string(&subscription).unwrap_or_default();
        info!("Subscribing to {} with payload: {}", resource_address, json_payload);
        
        // Try v2 API first, then fall back to v1
        let url_v2 = format!("{}/api/ocloudNotifications/v2/subscriptions", config.proxy_url);
        
        // Build request manually to ensure proper Content-Type and body
        let response = match state.client.post(&url_v2)
            .header("Content-Type", "application/json")
            .body(json_payload.clone())
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                error!("Failed to send subscription request for {}: {}", resource_address, e);
                continue;
            }
        };
        
        if response.status().is_success() {
            info!("Successfully subscribed to {}", resource_address);
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            warn!("Subscription failed for {}: {} - {}", resource_address, status, body);
        }
    }
    
    Ok(())
}

// Handle incoming CloudEvents from the fast event proxy (callback uses our Kubernetes Service).
// Source IP is taken from the TCP connection peer only (no X-Forwarded-For or other headers).
async fn handle_cloudevent(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Result<impl IntoResponse, StatusCode> {
    let (parts, body) = req.into_parts();
    let method = parts.method.as_str();
    let uri = &parts.uri;
    let version_str = match parts.version {
        axum::http::Version::HTTP_09 => "HTTP/0.9",
        axum::http::Version::HTTP_10 => "HTTP/1.0",
        axum::http::Version::HTTP_11 => "HTTP/1.1",
        axum::http::Version::HTTP_2 => "HTTP/2",
        axum::http::Version::HTTP_3 => "HTTP/3",
        _ => "HTTP/?",
    };
    let dest = parts
        .headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:8080");
    let connection_line = format!("{} -> {}", format_connection_source(addr), dest);
    info!("{}", connection_line);
    info!("{} {} {}", method, uri, version_str);
    let request_headers: HashMap<String, String> = parts
        .headers
        .iter()
        .filter_map(|(name, value)| value.to_str().ok().map(|v| (name.as_str().to_string(), v.to_string())))
        .collect();
    for (name, value) in &request_headers {
        info!("  {}: {}", name, value);
    }
    let body_bytes = to_bytes(body, usize::MAX).await.map_err(|_| StatusCode::BAD_REQUEST)?;
    let payload: Value = serde_json::from_slice(&body_bytes).map_err(|_| StatusCode::BAD_REQUEST)?;
    if let Ok(pretty) = serde_json::to_string_pretty(&payload) {
        info!("{}", pretty);
    }

    // Extract PTP status information from the CloudEvent
    let mut ptp_status = PtpStatus {
        timestamp: payload
            .get("time")
            .or_else(|| payload.get("timestamp"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
        os_clock_sync_state: None,
        lock_state: None,
        clock_class: None,
        http_method: Some(method.to_string()),
        http_uri: Some(uri.to_string()),
        http_version: Some(version_str.to_string()),
        connection_line: Some(connection_line),
        request_headers: Some(request_headers),
        request_body: Some(payload.clone()),
        additional: HashMap::new(),
    };

    // Event type (e.g. event.sync.ptp-status.ptp-state-change, event.sync.sync-status.os-clock-sync-state-change)
    let event_type = payload.get("type").and_then(|t| t.as_str()).unwrap_or("").trim();
    info!("CloudEvent type: {:?}", event_type);

    // Helper: is this value entry a metric (numeric/decimal), not the enumeration state?
    let is_metric_value = |value_obj: &Value| {
        let vt = value_obj.get("value_type").or_else(|| value_obj.get("valueType")).and_then(|v| v.as_str());
        let dt = value_obj.get("data_type").or_else(|| value_obj.get("dataType")).and_then(|v| v.as_str());
        let is_metric_type = dt == Some("metric")
            || vt.map(|s| s.starts_with("decimal") || s.starts_with("integer") || s == "decimal64.3").unwrap_or(false);
        is_metric_type && value_obj.get("value").is_some()
    };

    // Extract data from the CloudEvent according to O-Cloud Notification API v2 format
    if let Some(data) = payload.get("data") {
        let data_is_string = data.as_str().is_some();
        let data_obj = if let Some(str_data) = data.as_str() {
            serde_json::from_str::<Value>(str_data).ok()
        } else if data.is_object() {
            Some(data.clone())
        } else {
            None
        };
        info!("CloudEvent data: {} (parsed: {})", if data_is_string { "string" } else if data.is_object() { "object" } else { "other" }, data_obj.is_some());

        if let Some(data_val) = data_obj {
            let values = data_val.get("values").or_else(|| data_val.get("Values"));
            if let Some(values) = values.and_then(|v| v.as_array()) {
                info!("CloudEvent data.values length: {}", values.len());
                // For state-change events, state is enumeration entry and metric is a separate entry (e.g. value_type "decimal64.3", data_type "metric")
                let mut enum_state: Option<String> = None;
                let mut metric_str: Option<String> = None;
                if event_type == "event.sync.ptp-status.ptp-state-change" || event_type == "event.sync.sync-status.os-clock-sync-state-change" {
                    for value_obj in values.iter() {
                        let value_type = value_obj.get("value_type").or_else(|| value_obj.get("valueType")).and_then(|v| v.as_str());
                        let is_enumeration = value_type.map(|s| s.eq_ignore_ascii_case("enumeration")).unwrap_or(false);
                        if is_enumeration {
                            enum_state = value_obj.get("value").and_then(|v| v.as_str()).map(|s| s.to_string());
                        } else if is_metric_value(value_obj) {
                            metric_str = value_obj.get("value").and_then(|v| {
                                v.as_str().map(|s| s.to_string())
                                    .or_else(|| v.as_i64().map(|n| n.to_string()))
                                    .or_else(|| v.as_u64().map(|n| n.to_string()))
                            });
                        }
                    }
                    info!("State-change parsed: enum_state={:?}, metric_str={:?}", enum_state, metric_str);
                    let formatted = enum_state.map(|state| match &metric_str {
                        Some(m) => format!("{} {}", state, m),
                        None => state,
                    });
                    if event_type == "event.sync.ptp-status.ptp-state-change" {
                        if let Some(s) = &formatted {
                            info!("Setting lock_state: {}", s);
                            ptp_status.lock_state = Some(s.clone());
                        } else {
                            info!("No formatted lock_state (enum_state was None)");
                        }
                    } else if event_type == "event.sync.sync-status.os-clock-sync-state-change" {
                        if let Some(s) = &formatted {
                            info!("Setting os_clock_sync_state: {}", s);
                            ptp_status.os_clock_sync_state = Some(s.clone());
                        } else {
                            info!("No formatted os_clock_sync_state (enum_state was None)");
                        }
                    }
                } else {
                    info!("Event type not a state-change, skipping enum/metric extraction");
                }

                for value_obj in values {
                    let resource_addr = match value_obj.get("ResourceAddress").and_then(|r| r.as_str()) {
                        Some(a) => a,
                        None => continue,
                    };
                    let val = value_obj.get("value");
                    let value_type = value_obj.get("value_type").or_else(|| value_obj.get("valueType")).and_then(|v| v.as_str());

                    // Clock class: ResourceAddress contains CLOCK_CLASS or clock-class (event.sync.ptp-status.ptp-clock-class-change)
                    if resource_addr.contains("CLOCK_CLASS") || resource_addr.contains("clock-class") {
                        if let Some(v) = val {
                            if let Some(n) = v.as_u64() {
                                ptp_status.clock_class = Some(n as u8);
                            } else if let Some(s) = v.as_str() {
                                if let Ok(n) = s.parse::<u64>() {
                                    ptp_status.clock_class = Some(n as u8);
                                }
                            }
                        }
                    }

                    ptp_status.additional.insert(resource_addr.to_string(), value_obj.clone());
                }
            } else {
                info!("CloudEvent data has no 'values' or 'Values' array");
            }
        }
    } else {
        info!("CloudEvent has no 'data' field");
    }
    
    // Also check source field for resource identification
    if let Some(source) = payload.get("source").and_then(|s| s.as_str()) {
        ptp_status.additional.insert("source".to_string(), json!(source));
    }

    // Merge in last known status for the three fields so every broadcast has full state (UI overwrites on each event)
    {
        let last = state.last_ptp_status.read().await;
        if let Some(ref last) = *last {
            if ptp_status.os_clock_sync_state.is_none() {
                ptp_status.os_clock_sync_state = last.os_clock_sync_state.clone();
            }
            if ptp_status.lock_state.is_none() {
                ptp_status.lock_state = last.lock_state.clone();
            }
            if ptp_status.clock_class.is_none() {
                ptp_status.clock_class = last.clock_class;
            }
        }
    }
    {
        let mut last = state.last_ptp_status.write().await;
        *last = Some(ptp_status.clone());
    }

    // Broadcast the status update
    if let Err(e) = state.tx.send(ptp_status) {
        warn!("Failed to broadcast status update: {}", e);
    }

    // Return 204 No Content for CloudEvents (cloud-event-proxy expects this for validation)
    // This indicates the event was successfully processed with no response body
    Ok(StatusCode::NO_CONTENT)
}

// Handle validation requests from cloud-event-proxy (GET/POST requests to verify callback URI)
// The cloud-event-proxy sends an initial notification to validate the callback URI
// It expects a 204 No Content status code
async fn handle_validation() -> Result<impl IntoResponse, StatusCode> {
    // Return 204 No Content for validation requests (cloud-event-proxy expects this)
    Ok(StatusCode::NO_CONTENT)
}

// SSE handler for real-time updates
async fn sse_handler(
    State(state): State<AppState>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.tx.subscribe();

    let stream = BroadcastStream::new(rx).map(|result| {
        let msg = match result {
            Ok(status) => status,
            Err(_) => PtpStatus {
                timestamp: chrono::Utc::now().to_rfc3339(),
                os_clock_sync_state: None,
                lock_state: None,
                clock_class: None,
                http_method: None,
                http_uri: None,
                http_version: None,
                connection_line: None,
                request_headers: None,
                request_body: None,
                additional: HashMap::new(),
            },
        };

        let json = serde_json::to_string(&msg).unwrap_or_else(|_| "{}".to_string());
        Ok(Event::default().data(json))
    });

    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive-text"),
    )
}

// Version (build time) endpoint for UI
async fn get_version() -> Json<Value> {
    Json(json!({ "version": app_version() }))
}

// Get current status endpoint
async fn get_status(State(state): State<AppState>) -> Json<Value> {
    // Try to get the latest status from the channel (non-blocking)
    let mut rx = state.tx.subscribe();
    match rx.try_recv() {
        Ok(status) => Json(serde_json::to_value(status).unwrap_or(json!({}))),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
            // No messages yet, return a default response
            Json(json!({
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "message": "No status available yet"
            }))
        }
        Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
            // Lagged behind, try to get the latest
            match rx.try_recv() {
                Ok(status) => Json(serde_json::to_value(status).unwrap_or(json!({}))),
                _ => Json(json!({
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "message": "No status available yet"
                })),
            }
        }
        Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
            Json(json!({
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "message": "Channel closed"
            }))
        }
    }
}

// Subscription response structure
#[derive(Deserialize, Debug)]
struct SubscriptionResponse {
    #[serde(rename = "ResourceAddress")]
    ResourceAddress: String,
    #[serde(rename = "EndpointUri")]
    EndpointUri: String,
    #[serde(rename = "SubscriptionId")]
    SubscriptionId: String,
    #[serde(rename = "UriLocation")]
    UriLocation: Option<String>,
}

// List current subscriptions
async fn list_subscriptions(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, StatusCode> {
    let config = &state.ptp_config;
    let subscriptions_url = format!("{}/api/ocloudNotifications/v2/subscriptions", config.proxy_url);
    
    match state.client.get(&subscriptions_url).send().await {
        Ok(response) if response.status().is_success() => {
            match response.json::<Value>().await {
                Ok(json) => {
                    // Handle both array and null responses
                    let subscriptions = if json.is_array() {
                        json.as_array().cloned().unwrap_or_default()
                    } else if json.is_null() {
                        vec![]
                    } else {
                        vec![json]
                    };
                    Ok(Json(json!({
                        "status": "success",
                        "subscriptions": subscriptions
                    })))
                }
                Err(e) => {
                    error!("Failed to parse subscriptions response: {}", e);
                    Err(StatusCode::INTERNAL_SERVER_ERROR)
                }
            }
        }
        Ok(response) => {
            warn!("Failed to list subscriptions: {}", response.status());
            Ok(Json(json!({
                "status": "error",
                "message": format!("Failed to list subscriptions: {}", response.status())
            })))
        }
        Err(e) => {
            error!("Failed to list subscriptions: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// Unsubscribe from a specific subscription
async fn unsubscribe(
    State(state): State<AppState>,
    Path(subscription_id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    let config = &state.ptp_config;
    let unsubscribe_url = format!("{}/api/ocloudNotifications/v2/subscriptions/{}", config.proxy_url, subscription_id);
    
    info!("Unsubscribing from subscription: {}", subscription_id);
    
    match state.client.delete(&unsubscribe_url).send().await {
        Ok(response) if response.status().is_success() => {
            info!("Successfully unsubscribed from {}", subscription_id);
            Ok(Json(json!({
                "status": "success",
                "message": format!("Successfully unsubscribed from {}", subscription_id)
            })))
        }
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            warn!("Failed to unsubscribe from {}: {} - {}", subscription_id, status, body);
            Ok(Json(json!({
                "status": "error",
                "message": format!("Failed to unsubscribe: {} - {}", status, body)
            })))
        }
        Err(e) => {
            error!("Failed to unsubscribe from {}: {}", subscription_id, e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// Resubscribe to all events
async fn resubscribe_events(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, StatusCode> {
    info!("Resubscribing to PTP events...");
    
    match subscribe_to_ptp_events(&state).await {
        Ok(_) => {
            info!("Successfully resubscribed to PTP events");
            Ok(Json(json!({
                "status": "success",
                "message": "Successfully resubscribed to PTP events"
            })))
        }
        Err(e) => {
            error!("Failed to resubscribe to PTP events: {}", e);
            Ok(Json(json!({
                "status": "error",
                "message": format!("Failed to resubscribe: {}", e)
            })))
        }
    }
}
