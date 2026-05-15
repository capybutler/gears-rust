# Nodes Registry - Quickstart

Tracks and reports information about all Cyber Ware server instances in your deployment.

**Provides:**
- Node identification (ID, hostname, IP address)
- System information (OS, CPU, memory, disk)
- System capabilities (available features, resource limits)
- Registration timestamps

**Use cases:**
- Monitor distributed Cyber Ware deployments
- Load balancing and resource allocation
- Health checks and diagnostics

Full API documentation: <http://127.0.0.1:8087/cw/docs>

The example server uses the gateway prefix `/cw`. This comes from `modules.api-gateway.config.prefix_path` and is configurable.

## Examples

### List All Nodes

```bash
curl -s http://127.0.0.1:8087/cw/nodes-registry/v1/nodes | python3 -m json.tool
```

**Output:**
```json
[
    {
        "id": "35b975fc-3c13-c04e-d62a-43c7623895e5",
        "hostname": "your-hostname",
        "ip_address": "192.168.1.100",
        "created_at": "2026-01-15T15:01:02.000Z",
        "updated_at": "2026-01-15T15:01:02.000Z"
    }
]
```

For additional endpoints, see <http://127.0.0.1:8087/cw/docs>.
