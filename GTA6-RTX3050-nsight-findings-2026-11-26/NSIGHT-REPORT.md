# Nsight GPU Trace findings: GTA VI (Series S profile) on an RTX 3050 6 GB

Date: 2026-11-26. PC: i5-14400F, RTX 3050 6 GB (GA107, driver 621.01, hardware scheduling on), 16 GB RAM,
Windows 11 25H2. Tool: NVIDIA Nsight Graphics 2026.4.0, GPU Trace Profiler, architecture "Ampere GA10x",
default metric set "Top-Level Triage". Build: our VIctory-derived layer, branch `claude-dev`, with the launcher's
settings (Series S profile, 1280x720 resolution cap, light stand-in, mip drop, card copies from 64 KiB, 512 MB card windows).

## 1. Summary

**The GPU is not short of shader power or memory bandwidth. For about half of every frame it is draining to idle
between commands.**

| What Nsight measured on a world frame | Value |
|---|---|
| GPU frame time (clocks locked to base by the profiler) | 222-232 ms |
| Graphics engine busy | 86-89% |
| Shader units (SMs) actually executing | **7.7-8.2%** |
| SMs completely idle | **65-66%** |
| Front end stalled on "wait for idle" commands | **52-54%** |
| L1 / L2 / video memory throughput | 4-5% each |
| PCIe throughput | ~7% (~430 MB read and ~500 MB written per frame) |
| Barriers per frame | 35,172 (run 223) → 23,626 (run 226, after a fix) |
| Command batches per frame (draws, dispatches, copies) | ~16,700-17,000 |

"Wait for idle" means the GPU must finish everything in flight before it can start the next command. With
thousands of small commands and barriers per frame, those drains add up to more than half the frame.

## 2. The runs

| Run | What | Outcome |
|---|---|---|
| 219 | GPU Trace via the Nsight MCP, **with the real-time shader profiler**, 3 frames at 150 s | The PC froze during collection. A 455 MB trace file was written, but opening it needs more than the ~12 GB of RAM this PC has free (the Nsight server reached 11.9 GB with 80 MB left), so it is unread. It holds the shader-level hotspots and can be opened on a PC with more RAM. |
| 220 | Nsight command line, no shader profiler, 1 frame at 150 s | Clean trace, but of a **menu frame**: the Windows session was locked, so the virtual controller's A presses never reached the game, and it sat at the player prompt (frame 4,485, 33 ms of GPU time, 96% idle). Kept as a contrast. |
| 221 | Same, collection at 240 s, more A presses | Same: player prompt (frame 7,308). |
| 222 | Same, with the layer's own scripted presses (`VICTORY_KEYS`), which work with the session locked | **World frame** (frame 2,152, 392 MB trace), but the frame overflowed Nsight's event buffer (23.6 MB needed) and timestamps (291,045 needed against 100,000), and the export timed out. |
| 223 | Event buffer 96 MB, 1,000,000 timestamps, no timeouts | **Clean world frame** (frame 2,209, 434 MB trace, exported). The baseline below. |
| 226 | As 223, after the barrier fix (section 5) | Clean world frame (frame 3,424, 428 MB trace, exported). |

## 3. The frame (run 223)

* Video memory requested 4,781 of 5,146 MiB (92.9%), nothing demoted to system memory. PC RAM 15,814 of 16,136 MiB
  (98%) during collection.
* Graphics engine busy 89.3% of the frame, yet:
  * SM throughput 8.2%, instructions executed 8.2% of peak;
  * SMs with no warps at all 64.7% of the time; SMs active but warps waiting 25.1%;
  * warps by shader kind: compute 8.4%, pixel 1.3%, vertex/geometry 0.6% of peak occupancy;
  * front end stalled on wait-for-idle: **53.6%**;
  * L1 5.0%, L2 4.2%, video memory 3.7%, PCIe 7.1%; raster 0.3%, ROPs 0.2-0.3%.
* Compute on the sync queue active 32.6% of the frame: most of the work is compute (the game's lighting, culling
  and skinning, plus the passes our layer adds).

Reading: an engine that is "busy" 89% of the time while its shader units are idle two thirds of it is an engine
that spends its time draining and restarting, not computing.

## 4. The timeline: what sits between the draws

From the trace's command timeline (run 223), mid-frame:

* Each indirect draw (`ExecuteIndirect`, about 1.3 µs of GPU time) is preceded by **two or three small buffer copies**
  of about 2.5-2.9 µs each, with barriers around them. These are our layer's constant-buffer copies: the console lets
  a constant buffer start on any 16-byte boundary, the PC needs 256, so the layer copies each misaligned constant
  buffer to an aligned place on the GPU before the draw that reads it.
* Each indirect compute dispatch (55-73 µs) is preceded by a ~3.5 µs dispatch of the layer's own: the pass that
  checks and clamps the dispatch's arguments.
* There are gaps of up to ~80 µs where the GPU does nothing between two commands.

Counted across the frame: **35,172 barriers** and **16,662 command batches**, about 8 barriers for each of the
game's ~4,500 draws.

What it is **not**:

* Not the game's own buffer copies: a counter added afterwards shows ~155 of those per second (~27 per frame).
* Not reads of game data over PCIe: moving every buffer the character shaders read into video memory (a separate
  run) left their cost unchanged (characters ~21 ms per frame, skinning ~2 ms per dispatch, character draws ~1 ms
  each).

## 5. The fix tried, and its result (run 226)

The constant copies moved the big shared buffers every draw reads from the "readable by shaders" state
(`GENERIC_READ`) to "copy source" (`COPY_SOURCE`) and back again for the next draw: two state transitions, each a
drain, per copy. But `GENERIC_READ` already includes `COPY_SOURCE`, so neither transition is needed. The layer now
leaves a buffer in `GENERIC_READ` when only a read state it includes is asked for.

| | Run 223 (before) | Run 226 (after) |
|---|---|---|
| Barriers per frame | 35,172 | **23,626 (-33%)** |
| Wait-for-idle share of the frame | 53.6% | 51.7% |
| SM throughput | 8.2% | 7.7% |
| SMs idle | 64.7% | 66.1% |
| GPU frame time (under the profiler) | 222 ms | 232 ms (a different scene moment) |
| Frame time in normal runs | 177.6 ms | 173.0 ms (within run-to-run noise) |

A third of the barriers gone, the waiting barely changed: the removed transitions were cheap ones. **The drains come
from the per-draw pattern itself**: the copies and their transitions on the constant buffer, the clamp passes, and
the idle gaps.

## 6. What the findings point to

1. **Read constant buffers in place.** Have the translated shaders read a misaligned constant buffer straight
   from its buffer as raw data, so no copy (and none of its transitions) is needed before the draw. The most direct
   attack on the per-draw drains.
2. **Clamp indirect arguments for a run of dispatches at once**, instead of one small pass before each.
3. **Find what the GPU waits for in the ~80 µs idle gaps** (queue waits, fences, or CPU-side starvation).
4. **Open the run 219 shader-profiler trace** on a PC with more RAM, for per-shader and per-line hotspots.

## 7. Files in this bundle

* `NSIGHT-REPORT.md`: this report, with appendices A and B below.
* `exports/run220`, `exports/run221`, `exports/run223`, `exports/run226`: the tables Nsight exported per trace:
  `GPUTRACE_FRAME.xls` (frame metrics; tab-separated text despite the extension), `REPRO_INFO.xls` (system and
  capture settings), `FRAME.xls` (GPU frame time), `GPUTRACE_REGIMES.xls` (empty for these traces),
  `D3DPERF_EVENTS.xls` (no markers).
* Not included, for size: the trace files themselves (`.ngfx-gputrace`: run 219 455 MB, run 222 392 MB, run 223
  434 MB, run 226 428 MB). They open in Nsight Graphics (File, Open) for the full timeline and per-command metrics.

## Appendix A: capture settings and system state (as Nsight recorded them)

| Field | run220 (Menu frame (for contrast)) | run223 (World, before barrier fix) | run226 (World, after barrier fix) |
|---|---|---|---|
| Computer Name | ALEX-FLEXWORKST | ALEX-FLEXWORKST | ALEX-FLEXWORKST |
| Operating System | Windows 11 (25H2) | Windows 11 (25H2) | Windows 11 (25H2) |
| Operating System Build | 10.0.26200 | 10.0.26200 | 10.0.26200 |
| Processor | Intel(R) Core(TM) i5-14400F | Intel(R) Core(TM) i5-14400F | Intel(R) Core(TM) i5-14400F |
| RAM Usage | 15417 MiB / 16136 MiB (95.5%) | 15814 MiB / 16136 MiB (98.0%) | 15482 MiB / 16136 MiB (95.9%) |
| Device Name | NVIDIA GeForce RTX 3050 | NVIDIA GeForce RTX 3050 | NVIDIA GeForce RTX 3050 |
| Chip Name | GA107 | GA107 | GA107 |
| Driver Version | 621.01 | 621.01 | 621.01 |
| Hardware Scheduling | Enabled | Enabled | Enabled |
| VRAM Requested | 430 MiB / 5234 MiB (8.2%) | 4781 MiB / 5146 MiB (92.9%) | 4703 MiB / 5155 MiB (91.2%) |
| VRAM Committed | 433 MiB / 5234 MiB (8.3%) | 4803 MiB / 5146 MiB (93.3%) | 4726 MiB / 5155 MiB (91.7%) |
| VRAM Bytes Demoted | 0 MiB | 0 MiB | 0 MiB |
| Product Version | 2026.4.0.0 (build 39228586) (public-release) | 2026.4.0.0 (build 39228586) (public-release) | 2026.4.0.0 (build 39228586) (public-release) |
| Process File Name | C:\dev\victory-gta6-anaro\target\release\victory.exe | C:\dev\victory-gta6-anaro\target\release\victory.exe | C:\dev\victory-gta6-anaro\target\release\victory.exe |
| Command Line | "C:\dev\victory-gta6-anaro\target\release\victory.exe" run C:\dev\gta6-build\game_scarlett_master.exe --execute --console series-s | "C:\dev\victory-gta6-anaro\target\release\victory.exe" run C:\dev\gta6-build\game_scarlett_master.exe --execute --console series-s | "C:\dev\victory-gta6-anaro\target\release\victory.exe" run C:\dev\gta6-build\game_scarlett_master.exe --execute --console series-s |
| API | Direct3D 12 | Direct3D 12 | Direct3D 12 |
| Troubleshooting Collection Controls | metrics, sampling oldest, instrumented, CB timestamps, FECS, warp state sampling control=31 | metrics, sampling oldest, instrumented, CB timestamps, FECS, warp state sampling control=31 | metrics, sampling oldest, instrumented, CB timestamps, FECS, warp state sampling control=31 |
| Background Shader Compiles at Trace End | Active | Inactive | Inactive |
| Start After | Elapsed Time 150000 ms | Elapsed Time 240000 ms | Elapsed Time 240000 ms |
| Max Duration  | 1000 ms | 1000 ms | 1000 ms |
| Limited To | 1 Frames | 1 Frames | 1 Frames |
| Allocated Event Buffer Memory (kB) | 20000 | 98304 | 98304 |
| Allocated Timestamps | 100000 | 1000000 | 1000000 |
| V-Sync Mode | Enforced Off | Enforced Off | Enforced Off |
| GPU Clocks | Locked to Base | Locked to Base | Locked to Base |
| Force Repaint | No | No | No |
| Force D3D12 Background Optimizations | No | No | No |
| Max Vulkan SC Internal CBs per Queue | 24 | 24 | 24 |
| Max Vulkan SC Internal CB Memory (kB) | 16 | 16 | 16 |
| Collect Screenshot | Yes | Yes | Yes |
| Time Every Action | No | No | No |
| Trace Shader Bindings | Yes | Yes | Yes |
| Collect Shader Pipelines | Yes | Yes | Yes |
| Collect External Shader Debug Info | Yes | Yes | Yes |
| Warp State Sampling Interval | N/A (Warp State Sampling Disabled) | N/A (Warp State Sampling Disabled) | N/A (Warp State Sampling Disabled) |
| Enable NVTX Ranges | Yes | Yes | Yes |
| Recompile Cached Vulkan Shader Modules | Yes | Yes | Yes |
| Metric Set | Async Compute Triage | Async Compute Triage | Async Compute Triage |
| Real-Time Shader Profiler | Disabled | Disabled | Disabled |
| Multi-Pass Metrics | Disabled | Disabled | Disabled |

## Appendix B: every frame metric Nsight exported

Values are percentages of peak (`pct_of_peak_sustained_elapsed`) unless the name says otherwise (`.sum`: a total over the frame; `per_cycle_elapsed`: per cycle; `per_second`: a rate).

| Metric | run220 | run223 | run226 |
|---|---|---|---|
| GPU frame time (ms) | 33.2187 | 221.832 | 232.336 |
| `HOST.TriageAC.gpu__engine_cycles_active_gr_or_ce %` | 4.103 | 89.26 | 86.23 |
| `HOST.TriageAC.gpu__engine_cycles_active_any_syncce %` | 0.0403 | 0.6554 | 0.7001 |
| `HOST.TriageAC.gpu__engine_cycles_active_any_asyncce %` | 0 | 3.489 | 3.162 |
| `FE_A.TriageAC.gr__cycles_active %` | 4.062 | 88.09 | 85.24 |
| `TriageAC.sm__throughput %` | 1.772 | 8.175 | 7.655 |
| `SM_A.TriageAC.l1tex__throughput %` | 1.545 | 5.047 | 4.595 |
| `TPC.TriageAC.vaf__throughput %` | 0.008468 | 0.0009565 | 0.0009769 |
| `LTS.TriageAC.lts__throughput %` | 0.688 | 4.218 | 3.883 |
| `FBSP.TriageAC.dramc__throughput %` | 0.8294 | 3.656 | 3.251 |
| `PCI.TriageAC.pcie__throughput %` | 5.518 | 7.146 | 7.655 |
| `HUB_B.TriageAC.pda__throughput %` | 0.002271 | 0.02391 | 0.02315 |
| `GPC_B.TriageAC.pes__throughput %` | 0.02908 | 0.4691 | 0.4553 |
| `GPC_A.TriageAC.raster__throughput %` | 0.6548 | 0.341 | 0.3297 |
| `GPC_A.TriageAC.prop__throughput %` | 2.341 | 0.2989 | 0.3012 |
| `ROP.TriageAC.zrop__throughput %` | 0 | 0.2902 | 0.2757 |
| `ROP.TriageAC.crop__throughput %` | 2.378 | 0.1901 | 0.1976 |
| `FE_A.TriageAC.gr__compute_cycles_active_queue_sync %` | 0.2215 | 32.65 | 31.39 |
| `FE_A.TriageAC.gr__compute_cycles_active_queue_async %` | 0 | 0.08779 | 0.1169 |
| `FE_A.TriageAC.fe__draw_count.sum` | 14 | 4537 | 4627 |
| `FE_A.TriageAC.gr__dispatch_count.sum` | 9 | 3.125e+04 | 3.03e+04 |
| `SM_B.TriageAC.tpc__warps_active_shader_vtg_realtime %` | 0.002135 | 0.5707 | 0.553 |
| `TPC.TriageAC.tpc__warps_active_shader_ps_realtime %` | 2.565 | 1.26 | 1.245 |
| `TPC.TriageAC.tpc__warps_active_shader_cs_realtime %` | 0.1578 | 8.433 | 7.853 |
| `TriageAC.tpc__warps_inactive_sm_active_realtime %` | 0.7461 | 25.06 | 24.22 |
| `TriageAC.tpc__warps_inactive_sm_idle_realtime %` | 96.53 | 64.68 | 66.13 |
| `FE_A.TriageAC.fe__cycles_stalled_cmd_wfi_queue_sync %` | 3.72 | 53.61 | 51.73 |
| `FE_A.TriageAC.fe__cycles_stalled_cmd_wfi_queue_async %` | 0 | 0 | 0 |
| `FE_B.TriageAC.fe__output_ops_cmd_go_idle_queue_sync.sum` | 27 | 1.248e+04 | 1.267e+04 |
| `FE_B.TriageAC.fe__output_ops_cmd_subchsw_queue_sync.sum` | 18 | 3175 | 3227 |
| `FE_B.TriageAC.fe__output_ops_cmd_go_idle_queue_async.sum` | 0 | 0 | 0 |
| `FE_B.TriageAC.fe__output_ops_cmd_subchsw_queue_async.sum` | 0 | 0 | 0 |
| `FE_A.TriageAC.fe__pixel_shader_barriers.sum` | 1 | 2 | 1 |
| `TPC.TriageAC.tpc__sm_rf_registers_allocated_shader_3d_realtime %` | 1.967 | 3.87 | 3.774 |
| `TPC.TriageAC.tpc__sm_rf_registers_allocated_shader_3d_realtime.avg.per_cycle_elapsed` | 1289 | 2536 | 2474 |
| `TPC.TriageAC.tpc__sm_rf_registers_allocated_shader_cs_queue_sync_realtime %` | 0.1126 | 10.35 | 9.594 |
| `TPC.TriageAC.tpc__sm_rf_registers_allocated_shader_cs_queue_sync_realtime.avg.per_cycle_elapsed` | 73.79 | 6785 | 6287 |
| `TPC.TriageAC.tpc__sm_rf_registers_allocated_shader_cs_queue_async_realtime %` | 0 | 0 | 0 |
| `TPC.TriageAC.tpc__sm_rf_registers_allocated_shader_cs_queue_async_realtime.avg.per_cycle_elapsed` | 0 | 0 | 0 |
| `TPC.TriageAC.tpc__l1tex_mem_shared_data_compute_bytes_allocated_queue_sync_realtime %` | 0.03119 | 2.113 | 1.964 |
| `TPC.TriageAC.tpc__l1tex_mem_shared_data_compute_bytes_allocated_queue_sync_realtime.avg.per_cycle_elapsed` | 63.89 | 4328 | 4021 |
| `TPC.TriageAC.tpc__l1tex_mem_shared_data_compute_bytes_allocated_queue_async_realtime %` | 0 | 0 | 0 |
| `TPC.TriageAC.tpc__l1tex_mem_shared_data_compute_bytes_allocated_queue_async_realtime.avg.per_cycle_elapsed` | 0 | 0 | 0 |
| `SM_B.TriageAC.tpc__ctas_active %` | 0.1134 | 9.664 | 8.867 |
| `SM_B.TriageAC.tpc__ctas_active.avg.per_cycle_elapsed` | 0.0363 | 3.092 | 2.838 |
| `TriageAC.tpc__ctas_active_queue_sync_realtime %` | 0.1134 | 9.664 | 8.867 |
| `TriageAC.tpc__ctas_active_queue_sync_realtime.avg.per_cycle_elapsed` | 0.0363 | 3.092 | 2.838 |
| `TPC.TriageAC.tpc__ctas_active_queue_async_realtime %` | 0 | 0 | 0 |
| `TPC.TriageAC.tpc__ctas_active_queue_async_realtime.avg.per_cycle_elapsed` | 0 | 0 | 0 |
| `SM_A.TriageAC.sm__inst_executed_realtime %` | 1.772 | 8.175 | 7.655 |
| `SM_A.TriageAC.sm__inst_executed_realtime.sum` | 4.4171e+07 | 1.3612e+09 | 1.3350e+09 |
| `SM_A.TriageAC.sm__inst_executed_realtime.avg.per_cycle_elapsed` | 0.07086 | 0.327 | 0.3062 |
| `SM_A.TriageAC.sm__inst_executed_realtime.avg.peak_sustained` | 4 | 4 | 4 |
| `SM_A.TriageAC.sm__inst_executed_pipe_alu_realtime %` | 1.251 | 7.163 | 6.662 |
| `SM_A.TriageAC.sm__inst_executed_pipe_alu_realtime.sum` | 1.5598e+07 | 5.9638e+08 | 5.8094e+08 |
| `SM_A.TriageAC.sm__inst_executed_pipe_alu_realtime.avg.per_cycle_elapsed` | 0.02502 | 0.1433 | 0.1332 |
| `SM_A.TriageAC.sm__inst_executed_pipe_alu_realtime.avg.peak_sustained` | 2 | 2 | 2 |
| `SM_C.TriageAC.smsp__inst_executed_pipe_fma %` | 0.5813 | 2.855 | 2.69 |
| `SM_C.TriageAC.smsp__inst_executed_pipe_fma.sum` | 1.4495e+07 | 4.7541e+08 | 4.6906e+08 |
| `SM_C.TriageAC.smsp__inst_executed_pipe_fma.avg.per_cycle_elapsed` | 0.02325 | 0.1142 | 0.1076 |
| `SM_C.TriageAC.smsp__inst_executed_pipe_fma.avg.peak_sustained` | 4 | 4 | 4 |
| `SM_C.TriageAC.smsp__inst_executed_pipe_fmaheavy %` | 0.453 | 2.906 | 2.746 |
| `SM_C.TriageAC.smsp__inst_executed_pipe_fmaheavy.sum` | 5.6479e+06 | 2.4193e+08 | 2.3945e+08 |
| `SM_C.TriageAC.smsp__inst_executed_pipe_fmaheavy.avg.per_cycle_elapsed` | 0.009061 | 0.05812 | 0.05492 |
| `SM_C.TriageAC.smsp__inst_executed_pipe_fmaheavy.avg.peak_sustained` | 2 | 2 | 2 |
| `SM_C.TriageAC.smsp__inst_executed_pipe_xu %` | 0.2545 | 1.898 | 1.795 |
| `SM_C.TriageAC.smsp__inst_executed_pipe_xu.sum` | 7.934e+05 | 3.9513e+07 | 3.9125e+07 |
| `SM_C.TriageAC.smsp__inst_executed_pipe_xu.avg.per_cycle_elapsed` | 0.001273 | 0.009492 | 0.008974 |
| `SM_C.TriageAC.smsp__inst_executed_pipe_xu.avg.peak_sustained` | 0.5 | 0.5 | 0.5 |
| `SM_A.TriageAC.l1tex__data_pipe_lsu_wavefronts %` | 1.545 | 2.605 | 2.495 |
| `SM_A.TriageAC.l1tex__data_pipe_tex_wavefronts %` | 0.09244 | 5.047 | 4.595 |
| `SM_A.TriageAC.l1tex__lsu_writeback_active %` | 1.487 | 1.25 | 1.199 |
| `SM_A.TriageAC.l1tex__tex_writeback_active %` | 0.1016 | 4.381 | 3.972 |
| `SM_A.TriageAC.l1tex__f_wavefronts_realtime %` | 0.1109 | 4.684 | 4.252 |
| `SM_A.TriageAC.l1tex__data_pipe_lsu_wavefronts_mem_lg %` | 0.1272 | 1.168 | 1.143 |
| `SM_A.TriageAC.l1tex__data_pipe_lsu_wavefronts_mem_shared_realtime %` | 1.407 | 1.214 | 1.139 |
| `SM_A.TriageAC.l1tex__data_pipe_lsu_wavefronts_mem_surface_realtime %` | 0.01082 | 0.2232 | 0.2129 |
| `SM_B.TriageAC.l1tex__t_sector_hit_rate.pct` | 51.54 | 83.52 | 83.43 |
| `SM_A.TriageAC.l1tex__texin_cycles_stalled_on_tsl1_miss %` | 0.00212 | 1.494 | 1.449 |
| `LTS.TriageAC.lts__t_sector_throughput_srcunit_tex %` | 0.1222 | 3.489 | 3.311 |
| `LTS.TriageAC.lts__t_sector_throughput_srcunit_gcc %` | 0.001198 | 0.2162 | 0.2112 |
| `LTS.TriageAC.lts__t_sector_throughput_srcunit_pe %` | 0.02037 | 0.1598 | 0.1555 |
| `ROP.TriageAC.lts__t_sector_throughput_srcunit_crop %` | 0.1135 | 0.05384 | 0.05547 |
| `ROP.TriageAC.lts__t_sector_throughput_srcunit_zrop %` | 0 | 0.07363 | 0.07043 |
| `TriageAC.lts__t_sector_throughput_srcunit_raster %` | 0.0298 | 0.07598 | 0.07276 |
| `LTS.TriageAC.lts__t_sector_throughput_srcnode_fbp %` | 7.838e-05 | 0.03901 | 0.03857 |
| `LTS.TriageAC.lts__t_sector_throughput_srcnode_hub %` | 0.5832 | 1.472 | 1.194 |
| `LTS.TriageAC.lts__t_sector_throughput_aperture_device %` | 0.8692 | 5.319 | 4.869 |
| `LTS.TriageAC.lts__t_sector_throughput_aperture_other %` | 0.001193 | 0.2601 | 0.2403 |
| `LTS.TriageAC.lts__average_t_sector_hit_rate_realtime.pct` | 29.17 | 51.62 | 52.5 |
| `LTS.TriageAC.lts__average_t_sector_hit_rate_srcunit_tex_realtime.pct` | 70.48 | 49.69 | 49.23 |
| `FBSP.TriageAC.dramc__read_throughput %` | 0.7093 | 2.975 | 2.651 |
| `FBSP.TriageAC.dramc__write_throughput %` | 0.1201 | 0.6809 | 0.6001 |
| `PCI.TriageAC.pcie__read_bytes %` | 0.02211 | 6.128 | 7.655 |
| `PCI.TriageAC.pcie__read_bytes.sum` | 2.31e+05 | 4.2756e+08 | 5.5932e+08 |
| `PCI.TriageAC.pcie__read_bytes.sum.per_second` | 0.006953 | 1.927 | 2.407 |
| `PCI.TriageAC.pcie__write_bytes %` | 5.518 | 7.146 | 7.198 |
| `PCI.TriageAC.pcie__write_bytes.sum` | 5.7653e+07 | 4.9854e+08 | 5.2599e+08 |
| `PCI.TriageAC.pcie__write_bytes.sum.per_second` | 1.736 | 2.247 | 2.264 |
| `pcie__rx_requests_aperture_bar1_op_read.sum` | 5 | 484 | 301 |
| `pcie__rx_requests_aperture_bar1_op_write.sum` | 23 | 9.025e+04 | 1.012e+05 |
| `pcie__rx_requests_aperture_bar0_op_read.sum` | 336 | 6219 | 5595 |
| `pcie__rx_requests_aperture_bar0_op_write.sum` | 174 | 6383 | 5563 |
| `pcie__rx_requests_aperture_bar2_op_read.sum` | 0 | 374 | 425 |
| `pcie__rx_requests_aperture_bar2_op_write.sum` | 0 | 12 | 28 |
| `GPC_A.TriageSCG.raster__zcull_input_samples_op_rejected_realtime %` | 0 | 0.02247 | 0.02262 |
| `GPC_A.TriageSCG.raster__zcull_input_samples_op_rejected_realtime.sum` | 0 | 2.6608e+07 | 2.8047e+07 |
| `GPC_A.TriageSCG.raster__zcull_input_samples_realtime %` | 0.2074 | 0.1416 | 0.1373 |
| `GPC_A.TriageSCG.raster__zcull_input_samples_realtime.sum` | 3.6775e+07 | 1.6764e+08 | 1.7022e+08 |
| `gpc__cycles_elapsed.avg.per_second` | 1.042 | 1.042 | 1.042 |
| `gpc__cycles_elapsed.avg` | 3.4630e+07 | 2.3126e+08 | 2.4221e+08 |
| `sys__cycles_elapsed.avg.per_second` | 0.8925 | 0.885 | 0.8775 |
| `sys__cycles_elapsed.avg` | 2.9648e+07 | 1.9632e+08 | 2.0388e+08 |
| `lts__cycles_elapsed.avg.per_second` | 1.005 | 0.9975 | 0.99 |
| `lts__cycles_elapsed.avg` | 3.3385e+07 | 2.2128e+08 | 2.3001e+08 |
| `dramc__cycles_elapsed.avg.per_second` | 6.994 | 6.994 | 6.994 |
| `dramc__cycles_elapsed.avg` | 2.3233e+08 | 1.5515e+09 | 1.6250e+09 |
