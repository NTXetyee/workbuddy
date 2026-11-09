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
| T13 | Install virtual display driver (admin) | HALT: user runs scripts/install-vdd.ps1 | TODO |
| T14 | Sideload IPA with AltStore, launch, run wb-host | HALT: user | TODO |

## Post-M1 (not scheduled)
Tray icon, orientation follow, touch -> InjectTouchInput, direct NVENC + intra-refresh, DPI/resolution control from host, hotplug of virtual monitor.


