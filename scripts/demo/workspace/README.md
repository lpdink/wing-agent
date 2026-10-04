# retrykit

Tiny HTTP helper used by the demo recording.

`client.fetch` is supposed to retry transient failures; the suite in
`test_client.py` is the contract. Everything runs on the stdlib:

```bash
python3 -m unittest -v
```
