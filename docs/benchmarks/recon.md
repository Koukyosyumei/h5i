# Discovery, scored against targets that know what they have

Recon's smoke suite proves the verbs work. This says whether a change made
discovery *better*: four small targets with declared ground truth, one fixed
budget each, scored on endpoints confirmed, endpoints missed, false
confirmations, and requests spent per confirmed endpoint
(`docs/design/design-recon.md` N15).

```bash
./scripts/recon/bench/run.sh            # release binaries, four targets, one table
```

## Results, 2026-09-07

| target | real | confirmed | missed | false | requests | per confirmed |
|---|---:|---:|---:|---:|---:|---:|
| plain | 6 | 6 | 0 | 0 | 123 | 20.5 |
| soft | 6 | 6 | 0 | 0 | 123 | 20.5 |
| bundle | 4 | 4 | 0 | 0 | 120 | 30.0 |
| quiet | 4 | 4 | 0 | 0 | 120 | 30.0 |

Twenty of twenty real endpoints confirmed, nothing confirmed that is not there.
The requests are dominated by the wordlist: seven words with three extensions
and backup forms is 120 sends whatever the target holds, and the crawl and the
calibration are the other three. A run against a real application spends its
requests the same way, which is why `--max-requests` is on both verbs.

## The four shapes

Each is the smallest target that separates a discovery tool from a request
counter, and each serves its own list of real paths at `/__truth`, which the
scorer reads directly rather than through h5i.

| shape | what it tests |
|---|---|
| `plain` | links, a form, robots.txt, and honest 404s |
| `soft` | the same site answering `200` for every path, in its own template |
| `bundle` | endpoints reachable only through a JavaScript file |
| `quiet` | nothing linked: everything real is behind the wordlist |

## What it changed

The first run scored 17 of 20 and found two real defects, which is what a
benchmark is for.

**A shared template read as a missing page.** Triage treated a matching DOM
skeleton as proof that nothing was there. Most sites build every page from one
template, so on the `quiet` target the home page and the 404 page had the same
tags and sizes within a few bytes of each other, and the home page was not
confirmed. Shape alone is no longer enough: when the skeleton matches, what the
page *says* decides, hashed with the asked-for path removed so that a soft 404
echoing the path still reads as the same sentence. Both halves are needed. Drop
the text check and the `soft` target confirms nothing; drop the skeleton check
and it confirms everything.

**A word is not only a file.** `paths` asked for `/admin` and not `/admin/`, and
a server that answers the directory and 404s the bare word is ordinary, so the
one real page behind the wordlist was missed. Generation now includes the
directory form, and not a backup of it.

## What this is not

Four fixtures on loopback, not a corpus of real applications. It measures
whether discovery finds what is there and refuses what is not, on shapes chosen
because they are the ones that break naive tools. It says nothing about scale,
about authentication, or about how a real site's noise behaves; those want the
corpus the design still asks for.
