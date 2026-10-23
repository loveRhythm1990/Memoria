# Service access

Replace placeholders with values approved for your dedicated experiment. Do not commit live addresses, kubeconfigs, passwords, or owner keys.

## Network and health

Memoria serves HTTP on 8100; MatrixOne serves MySQL-compatible SQL on 6001. Inside Kubernetes, use `http://<memoria-service>.<namespace>.svc.cluster.local:8100` and `<mo-service>.<namespace>.svc.cluster.local:6001`.

For workstation access, bind port forwards to loopback only:

```bash
QA_KUBECONFIG='/path/to/your/read-only-kubeconfig'
EXPERIMENT_NAMESPACE='replace-with-experiment-namespace'
MEMORIA_SERVICE='replace-with-memoria-service'
MO_SERVICE='replace-with-matrixone-service'

# Terminal 1
kubectl --kubeconfig "$QA_KUBECONFIG" -n "$EXPERIMENT_NAMESPACE" \
  port-forward --address 127.0.0.1 "service/$MEMORIA_SERVICE" 18100:8100

# Terminal 2
kubectl --kubeconfig "$QA_KUBECONFIG" -n "$EXPERIMENT_NAMESPACE" \
  port-forward --address 127.0.0.1 "service/$MO_SERVICE" 16001:6001

# Another terminal
curl --fail --silent --show-error http://127.0.0.1:18100/health
curl --fail --silent --show-error http://127.0.0.1:18100/health/instance
```

`/health` returns `ok`. `/health/instance` checks `SELECT 1`; inspect `status` and `db`. Neither proves CAGRA usage. Kubernetes observation and port forwarding do not require changing deployed resources. The application tests intentionally write synthetic data in the dedicated test tenant.

## Authenticated API example

The acceptance run used trusted-backend authentication: `Authorization: Memoria-Owner <owner-key>` and `X-User-Id: <synthetic-user>`. This credential can select a user identity and belongs in a trusted server/test client. Obtain it through your team's approved secret channel.

This example prompts without echoing the key and creates one synthetic memory. It does not put the key in shell arguments, a URL, or output:

```python
import getpass
import json
import urllib.request
import uuid

base = "http://127.0.0.1:18100"
key = getpass.getpass("Approved experiment owner key: ")
user = "cuvs-review-" + uuid.uuid4().hex

def post(path, body):
    request = urllib.request.Request(
        base + path,
        data=json.dumps(body, ensure_ascii=False).encode(),
        headers={"Content-Type": "application/json",
                 "Authorization": "Memoria-Owner " + key,
                 "X-User-Id": user},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=90) as response:
        return json.load(response)

receipt = post("/v1/memories", {
    "content": "Synthetic preference: drink unsweetened jasmine tea.",
    "memory_type": "semantic", "session_id": "review-session-a",
})
result = post("/v1/memories/retrieve", {
    "query": "Which tea do I prefer?", "top_k": 5,
    "session_id": "review-session-b", "explain": "verbose",
})
print({"synthetic_user": user, "memory_id": receipt["memory_id"],
       "result_count": len(result["results"]),
       "vector_hit": result["explain"]["vector_hit"]})
```

A fresh user's table has no CAGRA index until the explicit index step is performed.

## SQL access

```bash
mysql --host=127.0.0.1 --port=16001 \
  --user='<tenant>:<user>:accountadmin' --password
```

Use the interactive password prompt. Check `SELECT current_account_name();` before any writes. User databases are named `mem_u_` plus the first 16 hexadecimal characters of SHA-256 of the user ID; the shared registry records that mapping too.

Create indexes only in the intended synthetic user's database. Check `SHOW CREATE TABLE` first, follow [the dataset sequence](datasets.md), and inspect `EXPLAIN` separately from API results.
