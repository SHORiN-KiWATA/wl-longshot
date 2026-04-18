# wl-longshot

## [中文版](README-CN.md) | [ENGLISH](README.md)

A simple scrolling screenshot tool for Wayland Compositors. 

## Features

Three Backends:

- `grim`: For tricky pages. Capture piece by piece. Stitching happens in a background worker. You can capture frames as fast as you want without waiting for stitching.

    ![](pics/grim-manual.gif)

    It alse includes an "Auto" mode where you only draw the region once, and it will captures the same geometry on subsequent scrolls. 

    ![](pics/grimauto.gif)

- `wl-screenrec`: Stream recording. Just select an area, scroll your mouse, and hit stop.

    ![](pics/rec.gif)

- `wf-recorder`: Fallback video backend for better compatibility, as same as `wl-screenrec`

- UI Fallback

    Uses wofi, fuzzel, or rofi for the menu. If none are installed or if run directly in a terminal, it falls back to a CLI text menu.


## Installation

- Arch Linux

    ```
    yay -S wl-longshot-git
    ```

    Then you can `wl-longshot` command to open the menu.

## Dependencies

- `bash` 
- `slurp` (for selecting areas)
- `wl-clipboard` (for copying to clipboard)
- `python`, `python-opencv`, `python-numpy` (for the stitching engine)
- `satty`(for editing image)
- `xdg-utils`(for previewing image through `xdg-open`)
- `wl-screenrec` or `wf-recorder` (for stream recording)

## Thanks

- [snemc](https://github.com/jswysnemc): Provided the inspiration for the screen recording-based scrolling screenshot implementation.
- [SHORiN-KiWATA](https://github.com/SHORiN-KiWATA): Designed the specific implementation methods and features.
- [Google Gemini](https://gemini.google.com/): Completed the main logic and core code.