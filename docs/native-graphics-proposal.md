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

[F1 menu implementation](../crates/victory-gpu/src/host/menu.rs) ·
[Upstream prototype](https://github.com/Marvisak/VIctory/pull/2863) ·
[Native Graphics design discussion](https://github.com/Marvisak/VIctory/issues/2862)
