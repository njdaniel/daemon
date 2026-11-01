# #143 — test: `lookup_while_pinned_the_search_and_the_second_request_complete` waits for the search instead of assuming it already arrived

Labels: `size:S, model:sonnet, m2, lane:llm, bug`

Found 2026-11-01 while merging main into PR #131 (#130): `just test` failed once on this test in `apps/dmn-cli/src/suggest.rs` (#102's web lookup), and then passed 7 of 8 times when run alone. The failure:

```
panicked at apps/dmn-cli/src/suggest.rs:4224:9:
assertion `left == right` failed: the search is running
  left: 0
 right: 1
```

## Cause
The test waits until Q1's answer is `Progress::Done`, dispatches `Pin`, and then at once asserts `server.requests().len() == 1`. The lookup runs on its own thread, and the mock `SearchServer` records a request only after its per-connection thread has read the whole HTTP request. `Done` does not wait for that, so under load (a full `cargo test --workspace` run) the request can still be in flight when the assertion runs. The product code is not at fault, only the test's timing.

The neighbouring test at line ~4129 already handles the same race with a bounded wait:

```rust
let until = Instant::now() + Duration::from_secs(5);
while server.requests().is_empty() {
    assert!(Instant::now() < until, "the search never started");
    std::thread::sleep(Duration::from_millis(5));
}
```

## Goal
The test waits (bounded, a few seconds) for the search request to reach the mock server before going on, and still fails if the search never starts or if more than one search is made. What it checks otherwise stays the same.

## Size / Model / Turns
S | Sonnet | 30 (docs/PROCESS.md). One test in one file.

## Scope
`apps/dmn-cli/src/suggest.rs`, tests module only. If more than one test needs the wait, a small helper beside `SearchServer` (e.g. `wait_for_requests(&server, n, timeout)`) is fine; it can replace the loop at ~4129 too. Look for other `server.requests()` assertions that come right after a hub state change without a wait, and fix them the same way if they have the same race.

Out of scope: changing the lookup's threading, `Progress` semantics, or the mock server.

## Acceptance
- [ ] The assertion at `suggest.rs:4224` becomes a bounded wait for exactly one request, failing with "the search never started" on timeout; the count is still checked to be 1 afterwards.
- [ ] The test passes 50 times in a row while the machine is loaded (e.g. alongside `cargo build --release` or `stress -c $(nproc)`); the run and its result are in the PR body.
- [ ] `just check`, `just test` and `just lint` pass.

## Verify
```bash
for i in $(seq 50); do cargo test -q -p dmn-cli --bin dmn lookup_while_pinned || break; done
just check
just test
just lint
```
