# Keyboard session recovery

## Design

- WTS notifications are registered for this session only. Lock/logoff/disconnect
  suspend hotkeys; unlock/logon/reconnect reset them. Lock and disconnect are
  tracked separately, so reconnect cannot accidentally enable a locked session.
- Reset discards left/right Win/Ctrl/Alt/Shift state, the previous main key,
  application-switch state and the Windows-menu mask. The overlay is hidden
  without committing its selection; the temporary window-cycle cache is cleared.
- Before processing a non-modifier, the hook checks the high bit of each side's
  `GetAsyncKeyState`. A stale pressed key cancels the old cycle and passes the
  current key through. Modifier events themselves use `vkCode`, never their
  not-yet-updated async state. Ctrl/Alt sides share scan codes but not virtual keys.
- State locks are released before calling another hook, sending messages or
  injecting mask input. The keyboard generation only invalidates actions before
  dispatch; it does not protect UI work already running inside that dispatch.
  No Windows-key release is synthesized.
- The UI has a separate atomic session epoch in stable storage outside `App`.
  Every WTS/stale-modifier UI reset advances it, including unlock/resume resets.
  Initial overlay construction snapshots a token, configuration and icon cache;
  enumeration and `WM_GETICON` queries hold no `App` borrow. After those calls,
  stale results are discarded without publishing state or restoring selection.
  Publication and painting each validate the original token. Window enumeration
  similarly validates before selecting/committing a cache and activating a target.
  Activation rechecks after window restore and input injection as well. These
  checks do not rely on reading an ordinary `self` field through the preexisting
  reentrant `&'static mut App` access pattern, nor do they replace that pattern
  throughout the application.
- Registration owns a UI-thread retry timer for `RPC_S_INVALID_BINDING` (services
  still starting). Failures are logged/reported. Exit, startup failure and message
  loop failure release the notification registration and timer.

## Automated checks (PowerShell, Scoop Rust/LLVM + cached cargo-xwin SDK)

```powershell
$xwin = "$env:LOCALAPPDATA\cargo-xwin\xwin"
$env:LIB = "$xwin\crt\lib\x86_64;$xwin\sdk\lib\um\x86_64;$xwin\sdk\lib\ucrt\x86_64"
$env:INCLUDE = "$xwin\crt\include;$xwin\sdk\include\um;$xwin\sdk\include\shared;$xwin\sdk\include\ucrt"
$env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = 'lld-link'
$env:RC = 'llvm-rc'
$env:CC = 'clang-cl'
$env:CXX = 'clang-cl'
$env:AR = 'llvm-lib'
cargo test --workspace
cargo check --workspace --all-targets
```

Tests cover missed releases and recovery, every modifier's left/right overlap,
stale Shift, ordinary/reverse hotkeys, suspended sessions, cancellation of old
selection state, reset generations, WTS event classification/order and cache
cleanup. UI regression tests simulate a reset during icon lookup/enumeration and
after publication, using the same token gates as production. They assert that old
results cannot republish selection/cache, paint or activate, and that a fresh
token remains usable. These are model tests, not real Win32 reentrancy injection
or an end-to-end secure-desktop simulation.

## Manual Windows checklist (not run by the automated agent)

1. Configure Win + main key; begin a switch, press Win+L, release all keys on the
   lock screen, then unlock. The overlay must not return or activate its previous
   target. Pressing the main key alone must do nothing; a fresh hotkey must work.
2. Repeat with Alt/Ctrl hotkeys and Shift held during the interrupted cycle.
3. Hold both sides of each modifier, release one side, and verify the other side
   still works. Test both release orders and reverse switching with either Shift.
4. Interrupt an open application overlay and a cached window cycle with RDP
   disconnect/reconnect (including a locked remote session). Neither may resume
   or commit the previous selection. A new cycle must use fresh window state.
5. Check startup/exit logs for notification registration/unregistration errors.
   Early-service retry and actual lock-screen/RDP delivery still need live testing.
6. With an uncached application that responds slowly to `WM_GETICON`, interrupt
   the very first overlay build with lock/unlock (or inject WTS messages during
   the icon wait in a debugger). When the query returns, no old overlay may be
   published or painted. Repeat during window enumeration: no old cycle cache
   may be committed and no previous target may be activated. Test both with and
   without an already visible overlay; a fresh hotkey must still work afterward.
