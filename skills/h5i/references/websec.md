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

Four things about `replay` that are easier to know than to discover.

**A `json.` value is typed the way it reads.** `json.id=99` is a number,
`json.active=true` a boolean, `json.role=admin` a string. Quote it to insist on
a string: `json.password="0e830400451993494058024219903391"` is the magic hash a
PHP `==` compares equal, and unquoted it is the *number* zero. When the encoded
value differs from the text you typed, the receipt says so in `encoded`.

**Header names go out lower-cased unless you ask otherwise.** That is what every
HTTP client does, and a proxy that looks a header up by exact string does not
care: it finds `Content-Length` and misses `content-length`. `--raw-headers`
sends the request the edits produced with the names written as given — `--set`
still applies, cookies still travel. Reach for `--raw-request` only when the
framing itself is the test, since that one makes you write every byte.

**Walking a list is `--set-each`, not a shell loop.** `--set-each
query.id=./ids.txt` sends once per line and returns one sample per send, each
naming the value that produced it. A loop outside pays process startup per
request and gives back N documents nothing correlates.

**A body that is not text comes out with `--body-to`.** `show --raw` says
`[85 bytes, not text — sha256 …]` rather than mangling a PNG or a git object
through the terminal; `show --body-to PATH` writes the bytes, and `--body-to -`
writes them to standard output for a pipe.

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
