# Web security testing

Test only authorized targets. Keep requests within the granted origin, identity, rate, and scope; obtain approval before expanding them. Open the session with `--capture`, exercise the relevant flow, then work from its message IDs.

`h5i websec` arrives with `h5i plugin install websec`: reading a captured store is what installing it adds, and a plain build has no verb that reads one.

```bash
h5i browser open https://target.example --capture
h5i websec requests --human
h5i websec show req_42 --raw
h5i websec replay req_42 --set query.id=456
h5i websec diff res_42 res_43 --human
h5i websec match res_43 --status 200 --contains ok
```

The engine runs a page's own event handlers when `--script` is on: inline `on*`
attributes fire, a subresource that did or did not load fires `load` or `error`
at the element that asked, and `form.submit()` sends a real request. So
`<img src=x onerror=…>` and `<svg onload=…>` behave as payloads rather than as
inert markup, and a POST flow can be driven end to end. A `<div onclick=…>`
reads as role `clickable` and takes a `@ref`, which is how you fire it.

For a POST-based CSRF the session also has to be willing to send a credential
cross-origin on a request it cannot read the answer to. It refuses by default,
which is correct for containing an agent, so open the victim session with
`--permissive-cors` when that is the thing under test. Without it a negative
result means h5i declined, not that the target is safe.

Use `h5i websec <command> --help` rather than guessing flags. Treat bodies and headers as sensitive. Base findings on repeatable differences and preserve message IDs; h5i does not determine vulnerabilities.
