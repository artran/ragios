# Shadow fleet template

These examples illustrate a layout for [comparing Ragios with Nagios](../../docs/shadow-evaluation.md). They are not a runnable fleet: `.invalid` addresses are placeholders, and the custom application, JSON, threshold, version, and Tailscale executable names and arguments illustrate contracts, not an inventory of your installed checks. Use [`../fleet/`](../fleet/) for a runnable local example.

- `ragios.yaml` explicitly disables email and defines independent weekday certificate and staffed notification windows. Review the example timezone and hours before use.
- `checks.yaml` shares the public web and application service bundles. The redirect check follows a redirect to a working page; if your old check requires an exact status or `Location`, preserve that assertion with the verified installed check rather than treating a successful final response as equivalent. The certificate check illustrates expiration thresholds, not a full substitute for any other TLS policy.
- `hosts.yaml` demonstrates ping and TCP reachability, groups independent of bundles, and root → group → host recipient overrides. The authenticated check receives a protected file **path**, not the credential value. See [installation permissions](../../docs/install-al2023.md#existing-python-checks-and-secrets).

Copy all three YAML files together to your private config directory. Remove checks you don't use and substitute verified executable paths, arguments and authorized targets for the rest. Do not edit or restart Nagios to prepare these files. From the repository root, the sample can be checked without any plugins or endpoints:

```sh
cargo run --locked -- --config examples/shadow/ragios.yaml --validate
```

`--validate` checks structure and references only. Verifying binaries and credentials on the new server is a separate operator step.
