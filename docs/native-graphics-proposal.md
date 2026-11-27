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

## Rust example

The native settings module uses a fieldless enum and one `Choice` implementation per row. The row's labels, option order, default, and config value all come from that type. This example adds the proposed `dlss_frame_generation` field to `Config` and uses the same pattern as the existing `GraphicsUpscaler` and `FrameGeneration` enums.

```rust
use crate::settings::{Choice, ConfigKey, Opt, Text};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum DlssFrameGeneration {
    #[default]
    Off = 0,
    X2 = 2,
    X3 = 3,
    X4 = 4,
    X5 = 5,
    X6 = 6,
}

impl Choice for DlssFrameGeneration {
    const CONFIG: ConfigKey = ConfigKey {
        section: "graphics",
        key: "dlss_frame_generation",
    };
    const NAME: Text = Text::own("victory_dlss_fg", "DLSS Frame Generation");
    const HELP: Text = Text::own("victory_dlss_fg_help", "Choose a frame multiplier.");
    const OPTIONS: &'static [Opt<Self>] = &[
        Opt { value: Self::Off, label: Text::own("victory_dlss_fg_off", "Off"), toml: "off" },
        Opt { value: Self::X2, label: Text::own("victory_dlss_fg_x2", "2×"), toml: "x2" },
        Opt { value: Self::X3, label: Text::own("victory_dlss_fg_x3", "3×"), toml: "x3" },
        Opt { value: Self::X4, label: Text::own("victory_dlss_fg_x4", "4×"), toml: "x4" },
        Opt { value: Self::X5, label: Text::own("victory_dlss_fg_x5", "5×"), toml: "x5" },
        Opt { value: Self::X6, label: Text::own("victory_dlss_fg_x6", "6×"), toml: "x6" },
    ];

    fn from_config(config: &crate::Config) -> Self {
        config.dlss_frame_generation
    }
}
```

Register `ListSetting::of::<DlssFrameGeneration>()` in the native Graphics section. For this fork, load the typed field from F1's saved value and route the row's commit through a shared F1 setter in `victory-gpu`. The setter needs to save the numeric value to `victory-settings.txt` and update the value F1 displays. Both menus can then read the same choice.

```rust
use std::io;
use victory_core::{config::DlssFrameGeneration, settings::Choice};

fn commit_dlss_frame_generation(
    row_index: usize,
    save_f1_choice: impl FnOnce(&str, u32) -> io::Result<()>,
) -> io::Result<()> {
    let choice = DlssFrameGeneration::from_index(row_index)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid multiplier"))?;
    let multiplier = choice as u32;
    save_f1_choice("VICTORY_MENU_FRAMEGEN", multiplier)?;

    if crate::streamline::framegen_live() {
        crate::streamline::set_framegen(multiplier);
    }
    Ok(())
}
```

The launcher already uses `VICTORY_STREAMLINE_DIR` to find the NVIDIA SDK DLLs, including `sl.interposer.dll`, `sl.dlss_g.dll`, and `nvngx_dlssg.dll`. This proposal uses that setup and the existing Streamline multiplier limits. With a live proxy the choice takes effect on the next present. Otherwise the launcher reads it on the next start. [NVIDIA fork DLL setup](https://github.com/trustno1-coder/VIctory-NVIDIA/blob/main/docs/dlss.md#setup)

[F1 menu implementation](../crates/victory-gpu/src/host/menu.rs) ·
[Upstream prototype](https://github.com/Marvisak/VIctory/pull/2863) ·
[Native Graphics design discussion](https://github.com/Marvisak/VIctory/issues/2862)
