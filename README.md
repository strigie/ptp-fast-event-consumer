# PTP Fast Event Consumer

Consumes PTP (Precision Time Protocol) events from OpenShift PTP Operator via the O-Cloud Notification API v2 and shows OS clock sync state, lock state, and clock class on a web dashboard with a live event log.

## Prerequisites

- Rust (for local build)
- Podman or Docker
- OpenShift/Kubernetes cluster with PTP Operator
- `oc` or `kubectl` with cluster access

## Build

**Local:**

```bash
cargo build --release
```

**Container:**

```bash
podman build -t quay.io/<your-username>/ptp-fast-event-consumer:latest .
podman push quay.io/<your-username>/ptp-fast-event-consumer:latest
```

## Deploy

1. Create the namespace and apply the manifest:

   ```bash
   oc apply -f deployment.yaml
   ```

2. Use your image:

   ```bash
   oc set image deployment/ptp-fast-event-consumer ptp-consumer=quay.io/<your-username>/ptp-fast-event-consumer:latest -n ptp-consumer
   oc rollout status deployment/ptp-fast-event-consumer -n ptp-consumer
   ```

3. Expose the UI (e.g. route or port-forward):

   ```bash
   oc port-forward -n ptp-consumer svc/ptp-fast-event-consumer 8080:8080
   ```

   Then open http://localhost:8080

## Configuration

Set via deployment or env:

- `PTP_NAMESPACE` – PTP operator namespace (default: `openshift-ptp`)
- `NODE_NAME` – Node name (from downward API)
- `POD_NAMESPACE` – Pod namespace (from downward API)

The app discovers the PTP event publisher service and its own callback URL (Kubernetes DNS) and subscribes to os-clock-sync-state, lock-state, and clock-class for the node.

## License

See [LICENSE](LICENSE).
