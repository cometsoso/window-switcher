# App-switcher UI delay

Automated model tests cover the deadline measured from first hotkey processing,
zero icon work while hidden or quickly released, one-time loading on expiry,
second main key/directional navigation, zero delay and timer failure, stale timer
IDs and hidden mouse gating. A source-wiring regression ensures lightweight
enumeration does not query process identities, clone the icon cache or load icons.
Existing input epoch tests cover lock/disconnect/stale-input invalidation during
construction. These do not exercise actual Windows message delivery or
secure-desktop events.

Selection/target HWNDs and owned icon sources are published without icon queries.
Only the unified presentation path starts identity checks and icon loading, once
the UI is requested. The overlay stays hidden and ignores mouse input until that
batch is ready; its pointer baseline is initialized after display. Cached icons
are reused only for known, identical sorted/deduplicated process identities;
unknown or changed identities require loading again, even if the path is unchanged.
Release commits the live lightweight selection immediately and invalidates the
builder, so no remaining application's icons or identities are requested. Loading
has a reentrancy guard: nested navigation changes the live selection without
loading recursively, and the final paint reads that latest state rather than a
pre-load snapshot. Tests exercise these checkpoints, empty/single targets,
new-round isolation, and a mock ShowWindow completing after nested cancellation
or a newer round's show. These remain model tests, not Win32 reentrancy injection.

## Manual Windows checks (not run by the agent)

Enable `[switch-apps]`, set `ui_delay_ms = 150`, then restart:

1. Tap the hotkey and release its modifier quickly: switch once, with no flash.
   With an empty icon cache, use debugger breakpoints on `get_app_icon` and
   `get_process_start_time` to confirm neither is called by the app-switch path
   while hidden or on quick release. Enumeration may do its own non-icon process
   queries. Repeat after a previous round has populated the cache.
2. Hold the modifier: after about 150 ms start icon preparation and then show the
   selected app. Slow icon queries can delay actual display beyond 150 ms; they
   must never start before the deadline. Release to commit.
3. Before the delay, press the main key again, Shift+main key, or each arrow:
   immediately show the advanced/reversed selection. Check key-repeat too.
4. Escape before/after reveal: do not switch or show a stale overlay later.
5. Move/click the mouse during the delay: do not alter/commit the selection.
   Keep the pointer still when the UI appears: no hover until a fresh movement.
6. Repeat rapidly across rounds; a previous timer must not reveal the next round.
7. Repeat with `ui_delay_ms = 0` (immediate) and `500` (easier hidden-phase testing).
8. Follow `session-recovery.md` for lock/unlock/RDP/stale-input cancellation,
   both while hidden and visible, including a slow uncached icon query. No old
   overlay may reappear; a fresh switch must still work. Do not lock unattended.
9. Timer-allocation failure needs debugger/fault injection: confirm a warning
   and immediate display rather than a permanently hidden switcher.
10. Use a deliberately slow uncached `WM_GETICON` responder. Release the modifier
    during its first query, optionally after forward/reverse/arrow navigation:
    the selected target must activate before that query returns; subsequent app
    icons must not be requested. Repeat with one app and with no eligible apps.
    While the query is pending, repeat navigation without releasing: no recursive
    icon load/duplicate batch, and the UI must show the latest selected index
    when loading finishes. Repeat with Escape, lock/reset, and a nested fresh
    round: old work must not publish icons/cache or overwrite the new selection.
11. In a debugger/message injector, cancel/reset during `ShowWindow(SW_SHOW)`:
    when the outer call returns the overlay must be hidden. Repeat with a nested
    new round that shows its overlay: the old completion must not hide it. Also
    test a nested new show during an old hide; the new overlay must remain visible.
12. Show the UI twice without restarting eligible applications: identities are
    checked only on requested display, and matching cached icons are reused.
    Restart an app at the same executable path (and test a failed identity query):
    the old process's icon must not be reused. Test a cold/empty icon cache too.
