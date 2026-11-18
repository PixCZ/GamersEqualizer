# egui-wgpu (fork of 0.37.0)

Bindings between [`egui`](https://github.com/emilk/egui) and [wgpu](https://crates.io/crates/wgpu), as
published on crates.io (egui-wgpu 0.37.0, MIT OR Apache-2.0, see `LICENSE-MIT` / `LICENSE-APACHE`),
used through `[patch.crates-io]` in the root `Cargo.toml`.

## The one change

`RenderState::available_adapters` holds `wgpu::AdapterInfo` instead of `wgpu::Adapter`
(`src/lib.rs`), exactly as [emilk/egui#8784](https://github.com/emilk/egui/pull/8784), which fixes
[emilk/egui#8704](https://github.com/emilk/egui/issues/8704).

Why: on DX12 every `wgpu::Adapter` owns an `ID3D12Device`. Stock egui-wgpu kept all enumerated
adapters for the window's lifetime, so the window held a device on every GPU even while it rendered on
the integrated one: the NVIDIA GPU of hybrid laptops stayed awake (listed in NVIDIA's GPU activity),
and G-Helper / Armoury Crate end such apps in iGPU-only mode. Nothing in eframe or our code reads the
field.

Left as is: wgpu's own adapter enumeration still creates a device on every GPU for a moment at
start-up (gfx-rs/wgpu#9932); those are dropped once the adapter is picked.

## When to drop it

Once an egui release carries #8784: move eframe / egui / egui-wgpu to it, delete this directory and
its lines in the root `Cargo.toml` (`[patch.crates-io]` and `exclude`). Checked in the dependency
check before a release (`.claude/rules/release.md`).
