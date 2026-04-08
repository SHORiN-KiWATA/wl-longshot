#!/usr/bin/env python3
import sys, os, gi, cairo

# 强制 Wayland 环境，保证通用性
os.environ["GDK_BACKEND"] = "wayland"

gi.require_version("Gtk", "3.0")
gi.require_version("GtkLayerShell", "0.1")
from gi.repository import Gtk, GtkLayerShell, Gdk

if len(sys.argv) != 5:
    sys.exit(1)
global_x, global_y, w, h = map(int, sys.argv[1:5])

window = Gtk.Window()
window.set_app_paintable(True)
window.set_decorated(False)
window.set_resizable(False)

# 动态获取 Matugen / GTK 主题的强调色
style_context = window.get_style_context()
found, color = style_context.lookup_color("accent_color")
if not found:
    found, color = style_context.lookup_color("theme_selected_bg_color")

if found:
    R, G, B = color.red, color.green, color.blue
else:
    R, G, B = 1.0, 1.0, 1.0

css_provider = Gtk.CssProvider()
css_provider.load_from_data(b"window { background: transparent; margin: 0px; padding: 0px; border: none; } decoration { box-shadow: none; border: none; background: transparent; }")
Gtk.StyleContext.add_provider_for_screen(window.get_screen(), css_provider, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)

drawing_area = Gtk.DrawingArea()
window.add(drawing_area)

if window.get_screen().get_rgba_visual():
    window.set_visual(window.get_screen().get_rgba_visual())

GtkLayerShell.init_for_window(window)
GtkLayerShell.set_layer(window, GtkLayerShell.Layer.OVERLAY)
GtkLayerShell.set_exclusive_zone(window, -1)
GtkLayerShell.set_anchor(window, GtkLayerShell.Edge.TOP, True)
GtkLayerShell.set_anchor(window, GtkLayerShell.Edge.BOTTOM, True)
GtkLayerShell.set_anchor(window, GtkLayerShell.Edge.LEFT, True)
GtkLayerShell.set_anchor(window, GtkLayerShell.Edge.RIGHT, True)

display = Gdk.Display.get_default()
target_monitor = None
local_x, local_y = global_x, global_y

for i in range(display.get_n_monitors()):
    monitor = display.get_monitor(i)
    geom = monitor.get_geometry()
    if geom.x <= global_x < geom.x + geom.width and geom.y <= global_y < geom.y + geom.height:
        target_monitor = monitor
        local_x = global_x - geom.x
        local_y = global_y - geom.y
        break

if target_monitor:
    GtkLayerShell.set_monitor(window, target_monitor)

def on_draw(widget, cr):
    cr.set_antialias(cairo.ANTIALIAS_NONE)
    cr.set_operator(cairo.OPERATOR_SOURCE)
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.0)
    cr.paint()
    cr.set_operator(cairo.OPERATOR_OVER)

    safe_gap = 2
    cx = local_x - safe_gap
    cy = local_y - safe_gap
    cw = w + safe_gap * 2
    ch = h + safe_gap * 2

    # 1. 基础辅助线 (2px)
    cr.set_source_rgba(R, G, B, 0.3)
    cr.set_line_width(2)
    cr.rectangle(cx - 1.0, cy - 1.0, cw + 2.0, ch + 2.0)
    cr.stroke()

    # 2. 画高亮的四个相机取景器边角 (3px)
    cr.set_source_rgba(R, G, B, 1.0)
    cr.set_line_width(3)
    
    corner_len = min(20, cw/4, ch/4)
    bx = cx - 1.5
    by = cy - 1.5
    bw = cw + 3
    bh = ch + 3

    # 左上
    cr.move_to(bx, by + corner_len)
    cr.line_to(bx, by)
    cr.line_to(bx + corner_len, by)
    # 右上
    cr.move_to(bx + bw - corner_len, by)
    cr.line_to(bx + bw, by)
    cr.line_to(bx + bw, by + corner_len)
    # 右下
    cr.move_to(bx + bw, by + bh - corner_len)
    cr.line_to(bx + bw, by + bh)
    cr.line_to(bx + bw - corner_len, by + bh)
    # 左下
    cr.move_to(bx + corner_len, by + bh)
    cr.line_to(bx, by + bh)
    cr.line_to(bx, by + bh - corner_len)
    
    cr.stroke()
    return False

drawing_area.connect("draw", on_draw)
empty_region = cairo.Region()
window.input_shape_combine_region(empty_region)
window.show_all()
Gtk.main()
