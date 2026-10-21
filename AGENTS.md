# GPU prototype context

For GPU encoder or EVDI capture work, read [docs/gpu-handover.md](docs/gpu-handover.md)
first. It identifies the active repositories, source paths, validation boundary
and pending work. The older `docs/handover.md` concerns the historical kernel
upstreaming effort.

Keep EVDI source changes synchronized with the patches and ABI header in
`tools/gpu/`; the companion branch is `cameron1729/evdi:gpu-shared-capture`.
The preparation script applies those patches to unpatched EVDI source.

Treat archived reports as evidence about their recorded build and environment,
not the currently loaded module or attached hardware. Keep the handover current
when the implementation or validation status changes. Preserve useful results
in `docs/`; generated builds and proprietary research inputs stay ignored.
