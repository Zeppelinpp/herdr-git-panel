# Herdr palette tokens

`herdr-palettes.json` records the 18 built-in palettes and 19 color-token values from [Herdr v0.9.5 `src/app/state.rs`](https://github.com/herdrdev/herdr/blob/v0.9.5/src/app/state.rs). It contains color data rather than a separate plugin theme. No download is performed at runtime.

The resolver reads `HERDR_CONFIG_PATH` or `~/.config/herdr/config.toml`, selects the named built-in palette, and applies shared custom tokens, the legacy UI accent (when no shared custom accent exists), then any active mode overrides. This follows [Herdr's resolver](https://github.com/herdrdev/herdr/blob/v0.9.5/src/app/mod.rs).

Herdr 0.9.5 does not expose its client's resolved theme/appearance through the public socket schema. The plugin therefore reads local config rather than claiming to query live client state. For `auto_switch`, the `COLORFGBG` appearance hint is used when present, otherwise the dark startup default is used. Live OS appearance changes and a remote client's different local theme cannot currently be synchronized exactly; static local themes and custom overrides are resolved directly. Reopen the panel or press Ctrl+R after editing the config.
