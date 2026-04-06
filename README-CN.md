# wl-longshot

## [中文版](README-CN) | [English](README)

一个适用于 Wayland 合成器的简单长截图工具。

## 功能特性

智能拼接：使用 Canny 边缘检测和 Sobel 梯度（基于 OpenCV）。它会忽略透明背景，并能精准拼接密集的文本，不会出现内容重复。

三种后端选项：

- `grim`：用于处理复杂的页面。采用逐块截图的方式。拼接工作在后台完成，你可以快速截取，而无需等待拼接过程。

    ![](pics/grim-manual.gif)

    它还包含一个“自动”模式：你只需框选一次区域，在后续截图时它会自动截取相同位置大小的画面。

    ![](pics/grim.gif)

- `wl-screenrec`：流式录制。只需框选区域，滚动鼠标，然后停止即可完成长截图。

    ![](pics/screenrec.gif)

- `wf-recorder`：作为备用视频后端以提供更好的兼容性，使用方式与 `wl-screenrec` 相同。

- UI 降级

    使用 wofi、fuzzel 或 rofi 作为菜单界面。如果没有安装这些工具，或者直接在终端中运行，它会优雅地降级为 CLI 纯文本菜单。

## 安装

- Arch Linux

    ```bash
    yay -S wl-longshot-git
    ```

    安装完成后，你可以使用 `wl-longshot` 命令来打开菜单。

## 依赖项

- `bash` 
- `slurp` （用于选择截图区域）
- `wl-clipboard` （用于将图片复制到剪贴板）
- `python`, `python-opencv`, `python-numpy` （用于图像拼接引擎）
- `satty` （用于编辑图像）
- `xdg-utils` （用于通过 `xdg-open` 预览图像）
- `wl-screenrec` 或 `wf-recorder` （用于流式录制）

## 致谢


- [snemc](https://github.com/jswysnemc) 提供基于录屏的长截图实现的灵感
- [SHORiN-KiWATA](https://github.com/SHORiN-KiWATA) 设计具体的实现方法和功能
- [Google Gemini](https://gemini.google.com/) 完成主要的逻辑判断和代码