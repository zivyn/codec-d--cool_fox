# Native Graphics controls proposal

VIctory-NVIDIA's F1 menu already gives players a way to change graphics
and diagnostics options while the game is open. A native Graphics row
would give players a second way to reach common settings through the game's
own keyboard and controller navigation.

| | F1 settings | Native Graphics prototype |
|---|---|---|
| Where it opens | VIctory overlay over the game | The game's own Settings → Graphics tab |
| What it controls | DLSS, frame generation, output size, FPS cap, lighting and diagnostics | Two VIctory-owned rows for Upscaler and Frame generation, beside the restored stock Graphics rows |
| Applying changes | Some options apply live. Others are saved for restart. | The Upscaler choice is saved and used on the next launch. Frame generation currently saves the choice. |

The prototype's native navigation, custom option names and persistence have
been exercised in game on the upstream test build. The option lookup
cleanup is in local testing. The NVIDIA fork has its own settings and
renderer, so its native control still needs integration and a title run.

## First integration

1. Add one opt-in Frame Generation row to the native Graphics tab.
2. Use the F1 menu's existing setting and apply path for both controls, with
   live changes when Streamline is ready and the same restart behavior
   otherwise.
3. Verify that changing either control updates the other, that the choice
   survives restart, and that both input methods work.

The first integration exercises one VIctory-owned setting. Restored stock
rows are outside that test.

## Rust sketch

The launcher already resolves the SDK directory and sets
`VICTORY_STREAMLINE_DIR`. Keep using that path for the signed
`sl.interposer.dll`, `sl.dlss_g.dll`, `nvngx_dlssg.dll` and the rest of their
dependencies. [Existing DLL setup](dlss.md#setup)

Use the same option values for the native row and F1. Place the commit
handler in `victory-gpu` so it can call the existing Streamline helpers. The
save callback below represents the shared settings helper to extract from
F1's save path.

```rust
use std::io;

const FRAMEGEN_KEY: &str = "VICTORY_MENU_FRAMEGEN";
const FRAMEGEN_OPTIONS: &[(&str, u32)] = &[
    ("Off", 0),
    ("2×", 2),
    ("3×", 3),
    ("4×", 4),
    ("5×", 5),
    ("6×", 6),
];

// The proposed shared setter updates F1's state and the same settings file.
fn commit_framegen(
    picked: usize,
    save_shared_choice: impl FnOnce(&str, &str) -> io::Result<()>,
) -> io::Result<()> {
    let &(_, multiplier) = FRAMEGEN_OPTIONS
        .get(picked)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid frame generation choice"))?;
    save_shared_choice(FRAMEGEN_KEY, &multiplier.to_string())?;

    if crate::streamline::framegen_live() {
        crate::streamline::set_framegen(multiplier);
    }
    Ok(())
}
```

If the Streamline swap-chain proxy is ready, the new multiplier takes
effect from the next present. Otherwise, the launcher uses the saved
choice next time. The existing Streamline code checks the GPU's maximum
and caps the request. For example, 6× requests five generated frames for
each rendered frame.

[F1 menu implementation](../crates/victory-gpu/src/host/menu.rs) ·
[Upstream prototype](https://github.com/Marvisak/VIctory/pull/2863) ·
[Native Graphics design discussion](https://github.com/Marvisak/VIctory/issues/2862)
