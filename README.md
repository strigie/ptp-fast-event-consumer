# PTP Fast Event Consumer

A Rust-based application for consuming PTP (Precision Time Protocol) events from Red Hat OpenShift's PTP Operator using the O-Cloud Notification API v2. The application provides real-time PTP status monitoring through a web dashboard with subscription management capabilities.

## Features

- **Dynamic Service Discovery**: Automatically discovers PTP services and endpoints from Kubernetes
- **Event Subscriptions**: Subscribes to PTP events via O-Cloud Notification API v2
- **Real-time Dashboard**: Web-based UI with Server-Sent Events (SSE) for live updates
- **Subscription Management**: View, unsubscribe, and resubscribe to PTP events
- **Event Logging**: Logs PTP Event API calls with source IP, HTTP method, and URI

## Architecture

The application:
- Discovers the PTP event publisher service in the `openshift-ptp` namespace
- Automatically determines the node name from the pod's environment
- Builds resource addresses dynamically based on the discovered node
- Subscribes to PTP events and receives CloudEvents via HTTP callbacks
- Provides a web UI for monitoring and managing subscriptions

## Prerequisites

- Rust 1.75+ (or latest for edition2024 support)
- Podman or Docker for building container images
- OpenShift/Kubernetes cluster with PTP Operator installed
- Access to Quay.io or another container registry
- `oc` (OpenShift CLI) configured with cluster access

## Building

### Local Build

```bash
# Clone or navigate to the project directory
cd ptp-fast-event-consumer

# Build the application
cargo build --release

# Run locally (requires PTP Operator in cluster)
cargo run
```

### Container Build

```bash
# Build the container image
podman build -t quay.io/<your-username>/ptp-fast-event-consumer:latest .

# Push to registry
podman push quay.io/<your-username>/ptp-fast-event-consumer:latest
```

## Deployment

### 1. Configure Quay.io Authentication

Create a Quay.io robot account with write permissions and authenticate:

```bash
# Create auth.json with your Quay.io credentials
cat > auth.json <<EOF
{
  "auths": {
    "quay.io": {
      "auth": "<base64-encoded-username:password>",
      "email": ""
    }
  }
}
EOF

# Login to Quay.io
podman login quay.io --authfile auth.json
```

### 2. Build and Push Image

```bash
# Build the image
podman build -t quay.io/<your-username>/ptp-fast-event-consumer:latest .

# Push to registry
podman push quay.io/<your-username>/ptp-fast-event-consumer:latest
```

### 3. Deploy to OpenShift

```bash
# Set your kubeconfig
export KUBECONFIG=/path/to/your/kubeconfig.yaml

# Apply the deployment
oc apply -f deployment.yaml

# Check deployment status
oc get pods -n ptp-consumer -l app=ptp-fast-event-consumer

# View logs
oc logs -n ptp-consumer -l app=ptp-fast-event-consumer -c ptp-consumer -f
```

### 4. Access the Application

Get the route URL:

```bash
oc get route -n ptp-consumer ptp-fast-event-consumer -o jsonpath='{.spec.host}'
```

Access via:
- **Direct**: `https://<route-host>` (if cluster is accessible)
- **SOCKS5 Proxy**: Configure your browser to use `127.0.0.1:8080` as SOCKS5 proxy, then visit `https://<route-host>`

## Configuration

The application automatically discovers configuration from Kubernetes:

- **PTP Namespace**: Set via `PTP_NAMESPACE` environment variable (default: `openshift-ptp`)
- **Node Name**: Automatically discovered from `NODE_NAME` environment variable (from pod spec)
- **Service Discovery**: Automatically finds PTP event publisher service and application service

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `PTP_NAMESPACE` | Namespace where PTP Operator is deployed | `openshift-ptp` |
| `POD_NAMESPACE` | Namespace where this application is deployed | `ptp-consumer` |
| `NODE_NAME` | Node name (automatically set from pod spec) | - |
| `RUST_LOG` | Logging level | `ptp_fast_event_consumer=info` |

## API Endpoints

### Web UI
- `GET /` - Main dashboard

### REST API
- `GET /api/status` - Get current PTP status
- `GET /api/sse` - Server-Sent Events stream for real-time updates
- `POST /api/events` - Receive CloudEvents from PTP Operator (callback endpoint)
- `GET /api/events` - Validation endpoint for cloud-event-proxy
- `GET /api/subscriptions` - List all active subscriptions
- `DELETE /api/subscriptions/:id` - Unsubscribe from a specific subscription
- `POST /api/subscriptions/resubscribe` - Resubscribe to all PTP events

## Usage

### Web Dashboard

1. **View Status**: The dashboard displays real-time PTP status:
   - OS Clock Sync State
   - Lock State
   - Clock Class

2. **Check Subscriptions**: Click "Check Subscriptions" to view all active subscriptions

3. **Unsubscribe**: Click "Unsubscribe" on any subscription to remove it

4. **Resubscribe**: Click "Resubscribe to Events" to recreate all subscriptions

5. **Event Log**: View incoming CloudEvents in the event log section

### Subscription Management

The application automatically subscribes to:
- `/cluster/node/<node-name>/sync/sync-status/os-clock-sync-state`
- `/cluster/node/<node-name>/sync/ptp-status/lock-state`
- `/cluster/node/<node-name>/sync/ptp-status/clock-class`

Subscriptions are created on application startup. You can manage them via the web UI.

## Logging

The application logs only PTP Event API calls with the following format:

```
PTP Event API: <HTTP_METHOD> <URI> from <SOURCE_IP>
```

Example:
```
PTP Event API: POST /api/events from 10.128.0.5
PTP Event API: GET /api/events from 10.128.0.5
```

## Troubleshooting

### Subscriptions Not Working

1. Check that the PTP Operator is installed and running:
   ```bash
   oc get pods -n openshift-ptp
   ```

2. Verify the PTP event publisher service exists:
   ```bash
   oc get svc -n openshift-ptp | grep ptp-event-publisher
   ```

3. Check application logs for subscription errors:
   ```bash
   oc logs -n ptp-consumer -l app=ptp-fast-event-consumer -c ptp-consumer | grep -i subscription
   ```

4. Verify RBAC permissions:
   ```bash
   oc get rolebinding -n ptp-consumer ptp-consumer
   oc get clusterrolebinding ptp-consumer-cross-namespace
   ```

### No Events Received

1. Verify subscriptions are active:
   ```bash
   oc exec -n ptp-consumer <pod-name> -c ptp-consumer -- curl -s http://localhost:8080/api/subscriptions
   ```

2. Check PTP Operator logs:
   ```bash
   oc logs -n openshift-ptp -l app=linuxptp-daemon -c cloud-event-proxy | grep -i subscription
   ```

3. Verify the callback URI is reachable from the PTP Operator namespace

### UI Not Loading

1. Check the route is created:
   ```bash
   oc get route -n ptp-consumer ptp-fast-event-consumer
   ```

2. Verify the service is running:
   ```bash
   oc get svc -n ptp-consumer ptp-fast-event-consumer
   ```

3. Check pod status:
   ```bash
   oc get pods -n ptp-consumer -l app=ptp-fast-event-consumer
   ```

## Development

### Project Structure

```
ptp-fast-event-consumer/
├── src/
│   └── main.rs          # Main application code
├── static/
│   └── index.html       # Web UI
├── Cargo.toml           # Rust dependencies
├── Dockerfile           # Container build definition
├── deployment.yaml      # Kubernetes deployment manifest
└── README.md            # This file
```

### Key Dependencies

- `axum` - Web framework
- `tokio` - Async runtime
- `reqwest` - HTTP client
- `kube` - Kubernetes client
- `serde` / `serde_json` - JSON serialization
- `tracing` - Logging

### Building Locally

```bash
# Install Rust (if not already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Build
cargo build --release

# Run
cargo run
```

## License

This project is provided as-is for use with Red Hat OpenShift PTP Operator.
