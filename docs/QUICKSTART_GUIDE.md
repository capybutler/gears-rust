# Cyber Ware Server - Quickstart Guide

Start Cyber Ware example server and verify it works. For project overview, see [README.md](../README.md).

---

## Start the Server

```bash
# With example modules (tenant-resolver, users-info)
make example

# Or minimal (no example modules)
make quickstart
```

Server runs on `http://127.0.0.1:8087`.
The example configuration also sets `modules.api-gateway.config.prefix_path: "/cw"` in `config/quickstart.yaml`, so API docs and endpoints are exposed under `/cw`.
Change `prefix_path` if you want a different base path, or set it to an empty string to serve the API at the root.

---

## Verify It's Running

```bash
curl -s http://127.0.0.1:8087/health
# {"status": "healthy", "timestamp": "..."}
```

---

## API Documentation

### Interactive Documentation

Open <http://127.0.0.1:8087/cw/docs> in your browser for the full API reference with interactive testing.

### OpenAPI Spec

```bash
curl -s http://127.0.0.1:8087/cw/openapi.json > openapi.json
```

### Module Examples

Each module has a QUICKSTART.md with minimal curl examples:

- [File Parser](../modules/file-parser/QUICKSTART.md) - Parse documents into structured blocks
- [Nodes Registry](../modules/system/nodes-registry/QUICKSTART.md) - Hardware and system info
- [Tenant Resolver](../modules/system/tenant-resolver/QUICKSTART.md) - Multi-tenant hierarchy

> **Note:** Module quickstarts show basic usage only. Use `/cw/docs` for complete API documentation in the example setup. This path is configurable via `api_gateway.prefix_path`.

---

## Stop the Server

```bash
pkill -f cyberware-server
```

---

## Troubleshooting

| Issue | Solution |
|-------|----------|
| Port 8087 in use | `pkill -f cyberware-server` |
| Empty tenant-resolver | Use `make example` instead of `make quickstart` |
| Connection refused | Server not running - check logs |

---

## Further Reading

- [/cw/docs](http://127.0.0.1:8087/cw/docs) - Full API reference
- [ARCHITECTURE_MANIFEST.md](ARCHITECTURE_MANIFEST.md) - Architecture principles
- [MODKIT_UNIFIED_SYSTEM/README.md](./modkit_unified_system/README.md) - Module system
