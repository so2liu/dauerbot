# Dauerbot terminal component patches

Upstream: gpui-terminal 0.1.0, https://github.com/zortax/gpui-terminal.
License: MIT OR Apache-2.0. Original source and copyright notices retained.

Dauerbot patches add native input-method composition, clipboard paste/copy,
terminal selection, and callbacks for read-only input and remote scrolling.
The terminal emulator remains Alacritty. The component accepts streams and
never owns the Agent process.
