# TASKS

Tags: INDEPENDENT or BLOCKED_BY. Status: TODO / RUNNING / DONE / FAILED(n).

## Task 0 - Wire protocol: DONE (SPEC section 2, architect)

## Batch 1 (all INDEPENDENT, dispatched concurrently)
| ID | Task | Owner paths | Tag | Status |
|---|---|---|---|---|
| T1 | Rust wb-proto codec + proto/vectors.json | host/crates/wb-proto, proto/ | INDEPENDENT | TODO |
| T2 | Rust wb-usbmux client + mock-server tests | host/crates/wb-usbmux | INDEPENDENT | TODO |
| T3 | Rust wb-capture (DXGI Desktop Duplication) | host/crates/wb-capture | INDEPENDENT | TODO |
| T4 | Rust wb-encode (VideoProcessor + hw H.264 MFT) | host/crates/wb-encode | INDEPENDENT | TODO |
| T5 | Virtual display driver research + install script | docs/, scripts/ | INDEPENDENT | TODO |
| T6 | iOS project.yml, CI workflow, app shell, WBProto Swift package | ios/project.yml, ios/WBProto, ios/WorkBuddy/App, .github | INDEPENDENT | TODO |
| T7 | iOS VideoDecoder + DisplayView | ios/WorkBuddy/Video | INDEPENDENT | TODO |
| T8 | iOS Session (NWListener, framing, HELLO/STATS) | ios/WorkBuddy/Net | INDEPENDENT | TODO |

## Batch 2
| ID | Task | Tag | Status |
|---|---|---|---|
| T9 | Rust wb-host binary (pipeline, threads, --dump, CLI) | BLOCKED_BY: [T1,T2,T3,T4] | TODO |
| T10 | iOS CI green loop (push, watch, route errors) | BLOCKED_BY: [T6,T7,T8] | TODO |
| T11 | Verify Swift vectors against proto/vectors.json | BLOCKED_BY: [T1,T6] | TODO |

## Batch 3 (architect + user)
| ID | Task | Tag | Status |
|---|---|---|---|
| T12 | Repo create/push (needs gh login) | HALT: user does `gh auth login` | TODO |
| T13 | Install virtual display driver (admin) | HALT: user runs scripts/install-vdd.ps1 | TODO |
| T14 | Sideload IPA with AltStore, launch, run wb-host | HALT: user | TODO |

## Post-M1 (not scheduled)
Tray icon, orientation follow, touch -> InjectTouchInput, direct NVENC + intra-refresh, DPI/resolution control from host, hotplug of virtual monitor.
