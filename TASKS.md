# TASKS

Tags: INDEPENDENT or BLOCKED_BY. Status: TODO / RUNNING / DONE / FAILED(n).

## Task 0 - Wire protocol: DONE (SPEC section 2, architect)

## Batch 1 (all INDEPENDENT, dispatched concurrently)
| ID | Task | Owner paths | Tag | Status |
|---|---|---|---|---|
| T1 | Rust wb-proto codec + proto/vectors.json | host/crates/wb-proto, proto/ | INDEPENDENT | DONE |
| T2 | Rust wb-usbmux client + mock-server tests | host/crates/wb-usbmux | INDEPENDENT | DONE |
| T3 | Rust wb-capture (DXGI Desktop Duplication) | host/crates/wb-capture | INDEPENDENT | DONE |
| T4 | Rust wb-encode (VideoProcessor + hw H.264 MFT) | host/crates/wb-encode | INDEPENDENT | DONE |
| T5 | Virtual display driver research + install script | docs/, scripts/ | INDEPENDENT | DONE |
| T6 | iOS project.yml, CI workflow, app shell, WBProto Swift package | ios/project.yml, ios/WBProto, ios/WorkBuddy/App, .github | INDEPENDENT | DONE |
| T7 | iOS VideoDecoder + DisplayView | ios/WorkBuddy/Video | INDEPENDENT | DONE |
| T8 | iOS Session (NWListener, framing, HELLO/STATS) | ios/WorkBuddy/Net | INDEPENDENT | DONE |

## Batch 2
| ID | Task | Tag | Status |
|---|---|---|---|
| T9 | Rust wb-host binary (pipeline, threads, --dump, CLI) | BLOCKED_BY: [T1,T2,T3,T4] | DONE |
| T10 | iOS CI green loop (push, watch, route errors) | BLOCKED_BY: [T6,T7,T8] | DONE (CI green, run 42628487305, IPA in dist/) |
| T11 | Verify Swift vectors against proto/vectors.json | BLOCKED_BY: [T1,T6] | DONE (testSharedVectors in CI) |

## Batch 3 (architect + user)
| ID | Task | Tag | Status |
|---|---|---|---|
| T12 | Repo create/push (needs gh login) | HALT: user does `gh auth login` | DONE (NTXetyee/workbuddy public) |
| T13 | Install virtual display driver (admin) | HALT: user runs scripts/install-vdd.ps1 | DONE (2 nodes cleaned, 2266x1488 set via targeted ChangeDisplaySettingsEx) |
| T14 | Sideload IPA with AltStore, launch, run wb-host | HALT: user | DONE (M1 verified live on iPad mini 6, 2026-11-09) |

## Post-M1 (not scheduled)
Tray icon, orientation follow, touch -> InjectTouchInput, direct NVENC + intra-refresh, DPI/resolution control from host, hotplug of virtual monitor.



## Milestone 1 reached (2026-11-09)
Live stream over USB works. Measured: ~33 fps (target 60), enc_ms 12-14 under real content (3-5 ms in synthetic dump), ipad_drop 4-5 of ~600, 0 host drops.
Next candidates: (a) find why fps is ~33 and encode ms is 3x the synthetic number (capture pacing / VideoProcessor / MFT warm-up / GPU clocks); (b) glass-to-glass measurement; (c) touch input; (d) rotation; (e) tray; (f) direct NVENC.


## Tuning round 1 (2026-11-09)
- Cause 1: NVIDIA dGPU idled in P8 (~680 MHz) -> encode 12-14 ms. Locking clocks (nvidia-smi -lgc 1800,3090, admin, temporary) -> 5-8 ms. Lasting fix TODO (per-app 'Prefer maximum performance' or GPU keepalive).
- Cause 2: pacing bug in CaptureEncoder (24 ms period -> ~42 fps). Fixed (should_encode). Under motion: 60.0 fps host, 59-60 fps iPad decode, 0 drops, enc ~7.8 ms, iPad decode ~11 ms.
- Open: iPad decoder enters drop-until-keyframe mode for ~2 s when motion starts after idle (burst of large frames, in-flight > 2). Fix in ios/WorkBuddy/Video/VideoDecoder.swift in the next iOS batch (needs a re-sideload).
- Open: static-screen repeats still cost 6-12 Mbps at ~32 fps (CBR); consider lower repeat rate.
- Open: real glass-to-glass measurement; est. now ~40-45 ms (cap phase ~8, enc ~8, USB ~3, iPad decode ~11, present ~8).

