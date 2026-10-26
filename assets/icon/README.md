# ServerOn 图标

`server-on.svg` 是原创 Tri-Runtime S-Rail 母版：三条粗轨道组成 S，分别使用 Windows 蓝、WSL 紫、Docker 青绿，表达统一管理三类运行环境。

- `server-on.svg`：完整底块和渐变，用于窗口、品牌区和 Windows 图标。
- `server-on-small.svg`：小尺寸加粗光学版本。
- `tray.svg`：透明背景、高对比托盘版本。
- `server-on.ico`：16/20/24/32/40/48/64/128/256 多帧 ICO。
- `server-on-*.png`、`tray-*.png`：逐尺寸生成的嵌入资源。

不使用第三方 Logo、字体或外部图片。生成工具是独立的 `tools/icon-gen`，正常产品构建不依赖它。

重新生成（需要已有离线 Rust 依赖缓存）：

```bash
ICON_SMALL_SVG=assets/icon/server-on-small.svg cargo run --offline --manifest-path tools/icon-gen/Cargo.toml -- \
  assets/icon/server-on.svg assets/icon/server-on.ico assets/icon server-on
cargo run --offline --manifest-path tools/icon-gen/Cargo.toml -- \
  assets/icon/tray.svg assets/icon/tray.ico assets/icon tray
```

小尺寸 ICO 帧通过 `ICON_SMALL_SVG` 使用 `server-on-small.svg`，大尺寸保留完整渐变和描边；正常发布不需要重新运行生成工具。
