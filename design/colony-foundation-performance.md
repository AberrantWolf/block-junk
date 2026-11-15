# Colony foundation performance report

These measurements are reproducible with the ignored
`requested_space_and_policy_scale_matrix` test and the ordinary
`a_requested_cave_converges_across_64_chunks_without_censusing_other_caves`
test. They were taken on an Apple M4 MacBook Pro (10 cores, 24 GB), macOS
26.6, arm64 Rust 1.95.0, from revision `a2f53eb` plus the foundation working
tree. Cargo's test profile is optimized with debug information.

The matrix runs the production cooperative spatial solver and production Lua
planner for 120 policy frames. Times are wall-clock microseconds. Network bytes
are complete initial replacement frames serialized as real `SpatialMessage`
values, including revision framing. Allocation columns are allocator calls and
cumulative requested bytes during convergence or the 120 policy frames.

| Spaces | NPCs | Convergence ticks | Geometry p50/p95/p99 µs | Policy p50/p95/p99 µs | Framed bytes | Geometry allocations / bytes | Policy allocations / bytes | Queue |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 10 | 44 | 721 / 1,388 / 1,413 | 117 / 139 / 230 | 429 | 85,242 / 12,691,663 | 194,578 / 11,131,595 | 0 |
| 1 | 50 | 44 | 721 / 1,388 / 1,413 | 568 / 691 / 860 | 429 | 85,242 / 12,691,663 | 977,384 / 55,810,175 | 0 |
| 1 | 100 | 44 | 721 / 1,388 / 1,413 | 1,257 / 1,782 / 2,070 | 429 | 85,242 / 12,691,663 | 1,954,801 / 111,617,715 | 0 |
| 10 | 10 | 445 | 602 / 1,268 / 1,540 | 137 / 227 / 323 | 4,462 | 870,897 / 130,677,076 | 194,600 / 11,132,273 | 0 |
| 10 | 50 | 445 | 602 / 1,268 / 1,540 | 671 / 752 / 830 | 4,462 | 870,897 / 130,677,076 | 977,404 / 55,811,091 | 0 |
| 10 | 100 | 445 | 602 / 1,268 / 1,540 | 1,269 / 1,366 / 1,648 | 4,462 | 870,897 / 130,677,076 | 1,954,808 / 111,617,667 | 0 |
| 100 | 10 | 4,459 | 888 / 1,663 / 1,878 | 123 / 202 / 400 | 46,012 | 11,747,401 / 1,839,129,338 | 194,577 / 11,131,353 | 0 |
| 100 | 50 | 4,459 | 888 / 1,663 / 1,878 | 653 / 987 / 1,139 | 46,012 | 11,747,401 / 1,839,129,338 | 977,383 / 55,809,418 | 0 |
| 100 | 100 | 4,459 | 888 / 1,663 / 1,878 | 1,276 / 1,754 / 2,029 | 46,012 | 11,747,401 / 1,839,129,338 | 1,954,801 / 111,617,811 | 0 |

The demanded 64-chunk cave converged in 36,758 ticks and 19.986 seconds of
wall time. Per update it measured p50 510 µs, p95 935 µs, p99 1,519 µs and a
13,118 µs maximum, with 20,387,012 allocator calls and 3,376,208,920 requested
bytes. Sixteen additional generated cave chunks outside demand created no
spatial jobs, identities, capabilities, settlement updates, or publications.

The deterministic budget preserves responsiveness and all finite fixtures
converge after edits stop. The cave's 36,758 simulation-tick latency is the
measured large-cave scale limit. Its work finishes in about 20 wall-clock
seconds in this harness, but publishing that result over ten simulated minutes
would be poor player feedback; this should be optimized before large natural
caves become common gameplay spaces.

Initial architectural streaming is separate from steady traffic. The largest
initial frame here is 46,012 bytes for 100 spaces; unchanged state sends zero
spatial bytes. Sending that complete replacement inside one second would exceed
the 40 kbps/player steady target, while paging it over ten seconds would not.
The revision transport uses scoped deltas after the initial replacement, but
this benchmark does not model a chosen edit rate, so it makes no unlimited-scale
or arbitrary-edit bandwidth claim.
