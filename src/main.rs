use axum::{
    extract::{Path, Query, Request, State, ConnectInfo},
    http::{HeaderMap, StatusCode, Method, Uri},
    middleware::Next,
    response::{sse::Event, IntoResponse, Sse},
    routing::{get, post},
    Json, Router,
};
use axum::body::{Body, to_bytes};
use std::net::SocketAddr;
// CloudEvents support - we'll parse JSON directly
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::broadcast;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tower_http::services::ServeDir;
use tracing::{error, info, warn};
use kube::{Api, Client, ResourceExt};
use k8s_openapi::api::core::v1::{Service, Node};
use percent_encoding::percent_decode_str;

// Application state
#[derive(Clone)]
struct AppState {
    tx: broadcast::Sender<PtpStatus>,
    last_status: Arc<RwLock<PtpStatus>>,
    client: reqwest::Client,
    #[allow(dead_code)]
    k8s_client: Client,
    ptp_config: Arc<PtpConfig>,
}

// PTP configuration discovered from Kubernetes
#[derive(Clone, Debug)]
struct PtpConfig {
    proxy_url: String,
    callback_uri: String,
    node_name: String,
    #[allow(dead_code)]
    ptp_namespace: String,
}

// PTP Status structure (also used for event log: request_* and request_body)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PtpStatus {
    timestamp: String,
    os_clock_sync_state: Option<String>,
    lock_state: Option<String>,
    clock_class: Option<u8>,
    /// Source IP of the HTTP request (for event log)
    #[serde(skip_serializing_if = "Option::is_none")]
    source_ip: Option<String>,
    /// HTTP method (for event log)
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<String>,
    /// Request URI (for event log)
    #[serde(skip_serializing_if = "Option::is_none")]
    uri: Option<String>,
    /// Raw HTTP body / CloudEvent JSON (for event log)
    #[serde(skip_serializing_if = "Option::is_none")]
    request_body: Option<Value>,
    /// Direct connection peer IP (e.g. OpenShift router 100.64.0.2)
    #[serde(skip_serializing_if = "Option::is_none")]
    direct_peer_ip: Option<String>,
    /// Request headers for HTTP-style event log (e.g. {"X-Forwarded-For": "..."})
    #[serde(skip_serializing_if = "Option::is_none")]
    request_headers: Option<HashMap<String, String>>,
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

// Body for POST /api/subscriptions/unsubscribe (browser sends this; our app then sends DELETE to cloud-event-proxy)
#[derive(Deserialize, Debug)]
struct UnsubscribeBody {
    #[serde(alias = "deleteTarget")]
    delete_target: Option<String>,
    #[serde(alias = "subscriptionId")]
    subscription_id: Option<String>,
}

// Query for GET /api/subscriptions/unsubscribe?deleteTarget=xxx (fallback when POST is blocked by Route/ingress)
#[derive(Deserialize, Debug)]
struct UnsubscribeQuery {
    #[serde(alias = "deleteTarget")]
    delete_target: Option<String>,
    #[serde(alias = "subscriptionId")]
    subscription_id: Option<String>,
}

// Publisher structure from /publishers endpoint (for future use)
#[allow(dead_code, non_snake_case)]
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

    info!("Starting PTP Fast Event Consumer");

    // Create Kubernetes client
    let k8s_client = Client::try_default().await?;
    info!("Kubernetes client initialized");

    // Discover PTP configuration
    let ptp_config = discover_ptp_config(&k8s_client).await?;
    info!("PTP configuration discovered: proxy_url={}, callback_uri={}, node_name={}", 
          ptp_config.proxy_url, ptp_config.callback_uri, ptp_config.node_name);

    // Create broadcast channel for SSE
    let (tx, _rx) = broadcast::channel::<PtpStatus>(100);

    // Last known status (merged across events)
    let last_status = Arc::new(RwLock::new(PtpStatus {
        timestamp: chrono::Utc::now().to_rfc3339(),
        os_clock_sync_state: None,
        lock_state: None,
        clock_class: None,
        source_ip: None,
        method: None,
        uri: None,
        request_body: None,
        direct_peer_ip: None,
        request_headers: None,
        additional: HashMap::new(),
    }));

    // Create HTTP client
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    let state = AppState { 
        tx, 
        last_status,
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

    // Build the router (with request logging for /api/* to debug DELETE not reaching server)
    let app = Router::new()
        .route("/api/events", post(handle_cloudevent))
        .route("/api/events", get(handle_validation)) // GET handler for cloud-event-proxy validation
        .route("/api/status", get(get_status))
        .route("/api/sse", get(sse_handler))
        .route("/api/subscriptions", get(list_subscriptions))
        .route("/api/subscriptions/unsubscribe", get(unsubscribe_get).post(unsubscribe_post))
        .route("/api/subscriptions/:id", axum::routing::delete(unsubscribe))
        .route("/api/subscriptions/resubscribe", get(resubscribe_events).post(resubscribe_events))
        .route("/api/debug/button-click", get(debug_button_click).post(debug_button_click_post))
        .nest_service("/", ServeDir::new("static"))
        .layer(axum::middleware::from_fn(log_api_requests))
        .with_state(state);

    // Start the server with ConnectInfo support
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    info!("Server listening on http://0.0.0.0:8080");
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
    
    // Get service ClusterIP or construct DNS name
    let proxy_url = if let Some(cluster_ip) = ptp_service.spec.as_ref()
        .and_then(|spec| spec.cluster_ip.as_ref())
        .filter(|ip| !ip.is_empty() && *ip != "None") {
        format!("http://{}:9043", cluster_ip)
    } else {
        // Fall back to DNS name
        format!("http://{}.{}.svc.cluster.local:9043", ptp_service.name_any(), ptp_namespace)
    };
    
    info!("Discovered PTP service: {} at {}", ptp_service.name_any(), proxy_url);
    
    // Discover our own service ClusterIP for callback
    let our_services: Api<Service> = Api::namespaced(k8s_client.clone(), &pod_namespace);
    let our_service_list = our_services.list(&Default::default()).await?;
    
    let our_service = our_service_list.iter()
        .find(|svc| svc.name_any().contains("ptp-fast-event-consumer"))
        .ok_or_else(|| anyhow::anyhow!("ptp-fast-event-consumer service not found in namespace {}", pod_namespace))?;
    
    let callback_uri = if let Some(cluster_ip) = our_service.spec.as_ref()
        .and_then(|spec| spec.cluster_ip.as_ref())
        .filter(|ip| !ip.is_empty() && *ip != "None") {
        format!("http://{}:8080/api/events", cluster_ip)
    } else {
        // Fall back to DNS name
        format!("http://{}.{}.svc.cluster.local:8080/api/events", our_service.name_any(), pod_namespace)
    };
    
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

/// Extract string value from a value object (notification or metric). Handles "value" and "Value" keys.
fn value_from_entry(value_obj: &Value) -> Option<String> {
    let val = value_obj
        .get("value")
        .or_else(|| value_obj.get("Value"));
    let v = match val {
        Some(v) => v,
        None => return None,
    };
    v.as_str()
        .map(|s| s.to_string())
        .or_else(|| v.as_u64().map(|n| n.to_string()))
        .or_else(|| v.as_i64().map(|n| n.to_string()))
        .or_else(|| v.as_f64().map(|n| n.to_string()))
}

/// Parse CloudEvent payload into status fields from data.values[] (ResourceAddress + value).
fn parse_values_from_payload(payload: &Value) -> (String, Option<String>, Option<String>, Option<u8>, HashMap<String, Value>) {
    let timestamp = payload
        .get("time")
        .or_else(|| payload.get("timestamp"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    let mut os_clock_sync_state = None;
    let mut lock_state = None;
    let mut clock_class = None;
    let mut additional = HashMap::new();

    let data = match payload.get("data") {
        Some(d) if d.as_str().is_some() => serde_json::from_str::<Value>(d.as_str().unwrap_or("")).ok(),
        Some(d) if d.as_object().is_some() => Some(d.clone()),
        _ => None,
    };

    if let Some(data_val) = data {
        if let Some(values) = data_val.get("values").and_then(|v| v.as_array()) {
            for value_obj in values {
                let resource_addr = match value_obj.get("ResourceAddress").or_else(|| value_obj.get("resourceAddress")).and_then(|r| r.as_str()) {
                    Some(a) => a,
                    None => continue,
                };
                let data_type = value_obj.get("data_type").or_else(|| value_obj.get("dataType")).and_then(|v| v.as_str());
                // Prefer notification for state enums; accept metric for numeric
                let is_notification = data_type == Some("notification");

                if let Some(val_str) = value_from_entry(value_obj) {
                    let addr_lower = resource_addr.to_lowercase();
                    let addr_lower_compact = addr_lower.replace("-", "").replace("_", "");
                    if addr_lower.contains("os-clock-sync-state") || resource_addr.contains("CLOCK_REALTIME") {
                        if is_notification || os_clock_sync_state.is_none() {
                            os_clock_sync_state = Some(val_str);
                        }
                    } else if addr_lower.contains("lock-state") || addr_lower.contains("lock_state")
                        || addr_lower_compact.contains("lockstate") || addr_lower.contains("sync-state")
                        || addr_lower.contains("sync_state") || addr_lower_compact.contains("syncstate")
                        || (addr_lower.contains("ptp") && addr_lower.contains("lock"))
                    {
                        if is_notification || lock_state.is_none() {
                            lock_state = Some(val_str);
                        }
                    } else if addr_lower.contains("clock-class") || addr_lower.contains("clock_class")
                        || addr_lower_compact.contains("clockclass")
                    {
                        if let Ok(n) = val_str.parse::<u64>() {
                            clock_class = Some(n.min(255) as u8);
                        }
                    }
                }
                additional.insert(resource_addr.to_string(), value_obj.clone());
            }
        }
        // Fallback: check data for direct keys (multiple naming conventions)
        if lock_state.is_none() {
            let s = data_val.get("lock_state").or_else(|| data_val.get("lockState"))
                .or_else(|| data_val.get("lock-state")).or_else(|| data_val.get("LockState"))
                .or_else(|| data_val.get("sync_state")).or_else(|| data_val.get("syncState"))
                .or_else(|| data_val.get("sync-state")).and_then(|v| v.as_str());
            if let Some(s) = s {
                lock_state = Some(s.to_string());
            }
        }
        if clock_class.is_none() {
            let n = data_val.get("clock_class").or_else(|| data_val.get("clockClass"))
                .or_else(|| data_val.get("clock-class")).or_else(|| data_val.get("ClockClass"))
                .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|x| x as u64)));
            if let Some(n) = n {
                clock_class = Some(n.min(255) as u8);
            }
        }
    }
    // Top-level payload fallbacks (some CloudEvents put fields at root or in data as string)
    if lock_state.is_none() {
        let s = payload.get("lock_state").or_else(|| payload.get("lockState"))
            .or_else(|| payload.get("lock-state")).and_then(|v| v.as_str());
        if let Some(s) = s {
            lock_state = Some(s.to_string());
        }
    }
    if clock_class.is_none() {
        let n = payload.get("clock_class").or_else(|| payload.get("clockClass"))
            .or_else(|| payload.get("clock-class")).and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|x| x as u64)));
        if let Some(n) = n {
            clock_class = Some(n.min(255) as u8);
        }
    }

    if let Some(source) = payload.get("source").and_then(|s| s.as_str()) {
        additional.insert("source".to_string(), json!(source));
    }

    (timestamp, os_clock_sync_state, lock_state, clock_class, additional)
}

// Process CloudEvent payload and broadcast (run in background). Merges with last known status and includes request metadata for event log.
fn process_cloudevent_payload(
    source_ip: String,
    direct_peer_ip: String,
    method: String,
    uri: String,
    request_headers: HashMap<String, String>,
    payload: Value,
    tx: broadcast::Sender<PtpStatus>,
    last_status: Arc<RwLock<PtpStatus>>,
) {
    let (timestamp, os_clock_sync_state, lock_state, clock_class, additional) = parse_values_from_payload(&payload);

    // Merge with last known status (only update fields we got from this event)
    let merged = {
        let mut last = last_status.write().unwrap_or_else(|e| e.into_inner());
        let mut next = PtpStatus {
            timestamp: timestamp.clone(),
            os_clock_sync_state: os_clock_sync_state.or(last.os_clock_sync_state.clone()),
            lock_state: lock_state.or(last.lock_state.clone()),
            clock_class: clock_class.or(last.clock_class),
            source_ip: None,
            method: None,
            uri: None,
            request_body: None,
            direct_peer_ip: None,
            request_headers: None,
            additional: last.additional.clone(),
        };
        next.additional.extend(additional);
        // Keep last known for next merge
        last.timestamp = next.timestamp.clone();
        last.os_clock_sync_state = next.os_clock_sync_state.clone();
        last.lock_state = next.lock_state.clone();
        last.clock_class = next.clock_class;
        last.additional = next.additional.clone();
        next
    };

    let headers_opt = if request_headers.is_empty() {
        None
    } else {
        Some(request_headers)
    };

    // Broadcast: merged status + this request's metadata and body for event log
    let to_send = PtpStatus {
        source_ip: Some(source_ip),
        direct_peer_ip: Some(direct_peer_ip),
        method: Some(method),
        uri: Some(uri),
        request_body: Some(payload),
        request_headers: headers_opt,
        ..merged
    };

    if let Err(e) = tx.send(to_send) {
        warn!("Failed to broadcast status update: {}", e);
    }
}

// Effective client IP: prefer original sender. Some proxies append to X-Forwarded-For (router, client),
// so we try last then first. Then X-Real-IP, then direct connection peer (100.64.0.2 is often the router).
fn effective_client_ip(headers: &HeaderMap, direct: std::net::IpAddr) -> String {
    if let Some(v) = headers.get("x-forwarded-for") {
        if let Ok(s) = v.to_str() {
            let parts: Vec<&str> = s.split(',').map(str::trim).filter(|x| !x.is_empty()).collect();
            if let Some(last) = parts.last() {
                return (*last).to_string();
            }
            if let Some(first) = parts.first() {
                return (*first).to_string();
            }
        }
    }
    if let Some(v) = headers.get("x-real-ip") {
        if let Ok(s) = v.to_str() {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    direct.to_string()
}

// All request headers for raw HTTP-style event log display (preserves header names as received).
fn request_headers_for_log(headers: &HeaderMap) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (name, value) in headers.iter() {
        if let Ok(s) = value.to_str() {
            out.insert(name.as_str().to_string(), s.to_string());
        }
    }
    out
}

// Handle incoming CloudEvents from PTP publisher.
// Respond with 204 immediately so cloud-event-proxy validation does not time out; process event in background.
async fn handle_cloudevent(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    method: Method,
    uri: Uri,
    Json(payload): Json<Value>,
) -> Result<impl IntoResponse, StatusCode> {
    let direct = addr.ip();
    let client_ip = effective_client_ip(&headers, direct);
    info!("PTP Event API: {} {} from {}", method, uri, client_ip);

    let source_ip = client_ip;
    let direct_peer_ip = direct.to_string();
    let method_str = method.to_string();
    let uri_str = uri.to_string();
    let request_headers = request_headers_for_log(&headers);
    let tx = state.tx.clone();
    let last_status = state.last_status.clone();
    tokio::spawn(async move {
        process_cloudevent_payload(source_ip, direct_peer_ip, method_str, uri_str, request_headers, payload, tx, last_status);
    });

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
                source_ip: None,
                method: None,
                uri: None,
                request_body: None,
                direct_peer_ip: None,
                request_headers: None,
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

// Subscription response structure (matches proxy API response)
#[allow(dead_code, non_snake_case)]
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

/// Normalize a subscription object from the proxy (various key names) into a consistent shape.
/// Include UriLocation when present so the frontend can use it for DELETE (proxy may require exact path).
fn normalize_subscription(item: &Value) -> Value {
    let get_str = |keys: &[&str]| -> Option<String> {
        for k in keys {
            if let Some(v) = item.get(k).and_then(|v| v.as_str()) {
                return Some(v.to_string());
            }
        }
        None
    };
    let subscription_id = get_str(&["SubscriptionId", "subscriptionId", "subscription_id", "Id", "id"])
        .unwrap_or_else(|| "unknown".to_string());
    let resource_address = get_str(&["ResourceAddress", "resourceAddress", "resource_address"])
        .unwrap_or_else(|| "".to_string());
    let endpoint_uri = get_str(&["EndpointUri", "endpointUri", "endpoint_uri"])
        .unwrap_or_else(|| "".to_string());
    let uri_location = get_str(&["UriLocation", "uriLocation", "uri_location"]);
    let mut out = json!({
        "subscriptionId": subscription_id,
        "resourceAddress": resource_address,
        "endpointUri": endpoint_uri,
        "SubscriptionId": subscription_id,
        "ResourceAddress": resource_address,
        "EndpointUri": endpoint_uri,
    });
    if let Some(ref loc) = uri_location {
        out["uriLocation"] = json!(loc);
    }
    out
}

/// Log every request to /api/* except /api/status (health checks).
async fn log_api_requests(req: Request<Body>, next: Next) -> impl IntoResponse {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let path = uri.path();
    if path.starts_with("/api/") && path != "/api/status" {
        info!("PTP Event API: {} {}", method, uri);
    }
    next.run(req).await
}

// Debug GET: called by frontend when Unsubscribe button is first clicked (before confirm dialog)
async fn debug_button_click() -> impl IntoResponse {
    info!("PTP Event API: debug button-click GET (Unsubscribe button clicked in UI)");
    StatusCode::NO_CONTENT
}

// Debug POST: verify that POST requests from the browser reach the app (Route/ingress might block POST)
async fn debug_button_click_post(req: Request<Body>) -> Result<impl IntoResponse, StatusCode> {
    let (_, body) = req.into_parts();
    let bytes = to_bytes(body, usize::MAX).await.map_err(|_| StatusCode::BAD_REQUEST)?;
    let body_str = String::from_utf8_lossy(&bytes);
    info!("PTP Event API: debug POST received (len {}): {}", bytes.len(), body_str);
    Ok(StatusCode::NO_CONTENT)
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
                    let raw = if json.is_array() {
                        json.as_array().cloned().unwrap_or_default()
                    } else if json.is_null() {
                        vec![]
                    } else {
                        vec![json]
                    };
                    let subscriptions: Vec<Value> = raw
                        .iter()
                        .filter(|v| v.is_object())
                        .map(normalize_subscription)
                        .collect();
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

// Call cloud-event-proxy to DELETE the subscription. Used by both DELETE and POST handlers.
async fn do_proxy_unsubscribe(state: &AppState, subscription_id: String) -> Result<Json<Value>, StatusCode> {
    let config = &state.ptp_config;
    let proxy_base = config.proxy_url.trim_end_matches('/');
    let unsubscribe_url = if subscription_id.starts_with("http://") || subscription_id.starts_with("https://") {
        subscription_id.clone()
    } else if subscription_id.starts_with('/') {
        let id_only = subscription_id.trim_end_matches('/').rsplit('/').next().unwrap_or(&subscription_id);
        format!("{}/api/ocloudNotifications/v2/subscriptions/{}", proxy_base, id_only)
    } else {
        format!("{}/api/ocloudNotifications/v2/subscriptions/{}", proxy_base, subscription_id)
    };
    info!("PTP consumer sending DELETE to cloud-event-proxy: {}", unsubscribe_url);
    match state.client.delete(&unsubscribe_url).send().await {
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if status.is_success() {
                info!("Successfully unsubscribed from proxy: {} - body: {}", status, body);
                Ok(Json(json!({ "status": "success", "message": "Successfully unsubscribed" })))
            } else {
                warn!("Proxy DELETE failed: {} - {}", status, body);
                Ok(Json(json!({
                    "status": "error",
                    "message": format!("Proxy returned {}: {}", status, body)
                })))
            }
        }
        Err(e) => {
            error!("Failed to call proxy DELETE {}: {}", unsubscribe_url, e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// GET /api/subscriptions/unsubscribe?deleteTarget=xxx — fallback when POST is blocked (e.g. by OpenShift Route).
async fn unsubscribe_get(
    State(state): State<AppState>,
    Query(q): Query<UnsubscribeQuery>,
) -> Result<impl IntoResponse, StatusCode> {
    let subscription_id = q
        .delete_target
        .or(q.subscription_id)
        .filter(|s| !s.is_empty() && s != "Unknown" && s.to_lowercase() != "unknown")
        .ok_or_else(|| {
            warn!("Unsubscribe GET missing or invalid deleteTarget/subscriptionId");
            StatusCode::BAD_REQUEST
        })?;
    info!("Unsubscribe GET received (len {}), sending DELETE to cloud-event-proxy", subscription_id.len());
    do_proxy_unsubscribe(&state, subscription_id).await
}

// POST /api/subscriptions/unsubscribe — browser sends POST; this app sends DELETE to cloud-event-proxy.
async fn unsubscribe_post(
    State(state): State<AppState>,
    req: Request<Body>,
) -> Result<impl IntoResponse, StatusCode> {
    let (_, body) = req.into_parts();
    let bytes = to_bytes(body, usize::MAX)
        .await
        .map_err(|e| {
            warn!("Unsubscribe POST failed to read body: {}", e);
            StatusCode::BAD_REQUEST
        })?;
    let body_str = String::from_utf8_lossy(&bytes);
    info!("Unsubscribe POST body (len {}): {}", bytes.len(), body_str);
    let parsed: UnsubscribeBody = serde_json::from_slice(&bytes).map_err(|e| {
        warn!("Unsubscribe POST JSON parse error: {} body: {}", e, body_str);
        StatusCode::BAD_REQUEST
    })?;
    let subscription_id = parsed
        .delete_target
        .or(parsed.subscription_id)
        .filter(|s| !s.is_empty() && s != "Unknown" && s.to_lowercase() != "unknown")
        .ok_or_else(|| {
            warn!("Unsubscribe POST missing or invalid deleteTarget/subscriptionId");
            StatusCode::BAD_REQUEST
        })?;
    info!("Unsubscribe POST parsed subscription_id (len {}), sending DELETE to cloud-event-proxy", subscription_id.len());
    do_proxy_unsubscribe(&state, subscription_id).await
}

// DELETE /api/subscriptions/:id — alternative; same effect (our app sends DELETE to proxy).
async fn unsubscribe(
    State(state): State<AppState>,
    Path(subscription_id_encoded): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    let subscription_id = percent_decode_str(&subscription_id_encoded)
        .decode_utf8_lossy()
        .into_owned();
    info!("Unsubscribe DELETE received for subscription (len {}), sending DELETE to proxy", subscription_id.len());
    do_proxy_unsubscribe(&state, subscription_id).await
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
