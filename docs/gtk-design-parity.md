# cmux-gtk design parity specification

Design reference for making `cmux-gtk` a 1:1 visual replica of the upstream
cmux macOS frontend. All values were extracted from the upstream sources kept
at the `pre-linux-prune` tag; paths below are paths inside that tag. Units are
macOS points (1pt = 1 logical pixel @1x).

## 1. Window structure

- No native titlebar and no toolbar: the macOS app uses
  `[.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView]`
  with `titleVisibility = .hidden` and a self-drawn 28pt titlebar band
  spanning the full window width (`Sources/AppDelegate.swift:9029-9080`,
  `Sources/WindowChromeMetrics.swift`).
- Default content size 1000x700; minimum 300x200
  (`Sources/SessionPersistence.swift:17-30`).
- Vertical layering: window backdrop (terminal background color) at the root,
  sidebar + content on top, the 28pt titlebar band floating above everything
  (`Sources/ContentView.swift:2565-2668`).
- The tab strip is NOT at the window top. It sits at the top of each
  workspace's terminal area, height 28
  (`Sources/WindowChromeMetrics.swift:4-9`).
- There is no status bar / bottom bar.
- Titlebar text: 13pt bold, color derived for readability against the chrome
  background (`Sources/ContentView.swift:2176-2178`).

## 2. Color system

### 2.1 Chrome derives from the terminal background

The single most important mechanism: window chrome (titlebar, tab strip, pane
backgrounds, separators) is not a fixed palette. It is derived from the
terminal background color (`Sources/Workspace.swift:2924-2966`). By default
(`usesWindowRootTerminalBackdrop() == true`) the tab bar, split-button
backdrop and pane backgrounds are fully transparent (`#00000000`) over one
solid window-root layer painted in the terminal background color.

### 2.2 Separator color algorithm

`Packages/macOS/CmuxAppKitSupportUI/.../WindowChromeColorResolver.swift:10-30`:

```
luminance = 0.299*r + 0.587*g + 0.114*b
isLight   = luminance > 0.5
amount    = isLight ? -0.12 : +0.16      # per-channel RGB offset
alpha     = isLight ? 0.26  : 0.36
separator = rgba(clamp(r+amount), clamp(g+amount), clamp(b+amount), alpha)
```

Borders are drawn 1pt solid. Examples: dark default `#1e1e1e` gives
`#474747 @ 36%`; light default `#feffff` gives `#E0E1E1 @ 26%`.

### 2.3 Default terminal colors (= window base)

From `Resources/ghostty/themes/Apple System Colors[ Light]`:

| | dark | light |
| --- | --- | --- |
| background | `#1e1e1e` | `#feffff` |
| foreground | `#ffffff` | `#000000` |
| cursor | `#98989d` | `#98989d` |
| selection bg | `#3f638b` | `#abd8ff` |
| selection fg | `#ffffff` | `#000000` |

`unfocusedSplitOpacity` default 0.7 (unfocused panes dim to 70%).

### 2.4 Accent

`Sources/Sidebar/SidebarAppearanceSupport.swift:78-96`: dark `#0091FF`,
light `#0088FF`. Used for: selected sidebar row background, multi-select row
fill at 25%, drop indicator bars, unread badges, task-status rings.

### 2.5 Sidebar background

macOS uses `NSVisualEffectView(.sidebar, .withinWindow)` blur with a black
tint at opacity 0.18. GTK has no equivalent material; composite a solid
approximation instead: terminal background blended with black @ 18%
(dark `#1e1e1e` -> ~`#181818`; light `#feffff` -> ~`#d0d1d1`).

### 2.6 TUI ChromeTheme palette (explicit light/dark reference)

The only explicit paired light/dark chrome palette in the tree, from
`cmux-tui/crates/cmux-tui/src/config.rs:229-412`. Use these semantics for the
tab strip and chrome where the macOS value is derived or unavailable:

| semantic | dark | light |
| --- | --- | --- |
| selection_bg | `#3a3a3a` | `#ccddf5` |
| tab_bar_bg | `#303030` | `#e4e4e4` |
| tab_fg | `#a8a8a8` | `#585858` |
| tab_active_bg | `#585858` | `#d0d0d0` |
| tab_active_fg | `#eeeeee` | `#1c1c1c` |
| tab_active_unfocused_bg | `#444444` | `#dadada` |
| tab_active_unfocused_fg | `#d0d0d0` | `#303030` |
| sidebar_selected_bg | `#303030` | `#dadada` |
| sidebar_selected_fg | `#eeeeee` | `#1c1c1c` |
| sidebar_dim_fg | `#6c6c6c` | `#6c6c6c` |
| sidebar_border | `#3a3a3a` | `#949494` |
| border_fg (inactive pane) | `#444444` | `#949494` |
| border_active_fg (active pane) | `#87afd7` | `#0087af` |
| menu_bg / menu_fg | `#3a3a3a` / `#d0d0d0` | `#e4e4e4` / `#303030` |
| toast_bg / toast_fg | `#585858` / `#eeeeee` | `#d0d0d0` / `#1c1c1c` |
| scrollbar_thumb_fg | `#949494` | `#949494` |

`theme.chrome` auto detection (`config.rs:401-412`):
`0.2126*r + 0.7152*g + 0.0722*b > 128` (0..255 scale) selects light.

## 3. Typography

- UI font: system font (SF Pro on macOS; use the GTK system UI font on
  Linux), global magnification 50-200%, default 100.
- Sidebar base font size 12.5 (range 10-20); all sidebar metrics scale by
  `size / 12.5`.
- Sidebar row: title 12.5 semibold; description 10.5 regular; subtitle 10;
  remote target 10 monospaced; remote status 9 medium; badges 9 semibold.
- Group header: name 11, chevron 9, folder icon 11, unread badge 10, plus 11.
- Titlebar text 13 bold. Tab title 11 (range 8-14).
- Terminal font: Menlo 13 upstream; keep the existing `cmux-gtk.json`
  monospace default on Linux.

## 4. Metrics

| item | value |
| --- | --- |
| chrome bar height (titlebar, tab bar) | 28 |
| sidebar default/min width | 240 |
| sidebar max width | 600, and at most 1/3 of window width |
| sidebar resizer hit area | 10 total (6 sidebar side + 4 content side), visual line 1pt |
| titlebar button | 20x20, icon 12, corner radius 6 |
| sidebar first row top offset | 30 (titlebar 28 + 2) |
| sidebar row vertical padding | 8 |
| sidebar row outer/content horizontal padding | 6 / 10 |
| sidebar row spacing | 2 |
| row selection corner radius | 6 |
| left color rail | 3pt wide, radius 1.5, inset 5 top/bottom, alpha 0.95 |
| drop indicator | 2pt tall, 8pt side margins, accent color |
| group header corner radius | 4 (6 when multi-selected), hover bg = fg @ 8% |
| group member indent | 12 |
| unread badge | 16pt circle, 9pt semibold text |
| active pane border | 2pt (only when a color is configured; default off) |
| pane separator | 1pt, color from the 2.2 algorithm |
| top/bottom sidebar fade scrims | 50pt |

## 5. Interaction states

- Row hover: no background change; only the close button fades in
  (120ms ease-out opacity 0 -> 1).
- Group header hover: background fg @ 8%, radius 4; plus button fades in.
- Pressed row buttons: content alpha 0.5. Dragged row: alpha 0.6.
- Multi-selected rows: accent @ 25%.
- No animations anywhere else: upstream sets `enableAnimations: false` and
  disables implicit layer animations; geometry snaps, it does not interpolate.
- Unfocused panes dim to 70% opacity.

## 6. Tab strip

Upstream tabs are drawn by the vendored Bonsplit package whose sources are
not in this repository; only the configuration surface is known: height 28,
title 11pt, close button configurable, drag reordering allowed, no
animations, `tabBarVisibility: .always`, colors passed as ChromeColors with a
transparent tab bar over the shared backdrop.

Because the exact tab shape is unknowable from this tree, model tabs on the
TUI semantics (2.6): active is indicated by background fill (three levels:
active-focused, active-unfocused, inactive), no bottom indicator line, no
rounded pill unless evidence appears. The right end of the tab bar has an
action lane with a ~100pt fade mask (ramp starts at 60%, trailing opacity
0.86) occluding tab content beneath it.

## 7. Sidebar rows

Row anatomy (title row, horizontal, spacing 8): optional leading unread
badge/spinner, pin icon 9pt, media icons 9pt, task-status ring (9pt circle in
an 11pt slot), title 12.5 semibold single-line truncated, trailing
badge/spinner/close (close fades in on hover). Optional detail slots stack
vertically with spacing 4. Selected background radius 6 plus a 3pt left rail
in the workspace color. Task-status ring colors: neutral = secondary @ 0.8,
running = accent, attention = `#FF6B33`, done = `#739E80`.

The spinner is a 12-spoke "spokes" style spinner, not the GTK default.
Agent brand icons do NOT appear in sidebar rows upstream; activity is shown
only via the spinner.

There is no fixed "new" button at the sidebar bottom; creation lives in the
titlebar control cluster and a hover-revealed plus on group headers.

## 8. GTK4 implementation notes

- Implement the chrome-from-terminal-background derivation (2.1-2.2) as a
  runtime function generating `@define-color` values; recompute on theme
  change. This is the foundation, not an option.
- Use GTK CSS for: bar heights, paddings, radii, hover/pressed opacities,
  badge shapes, 1pt separators, fade-in transitions.
- Self-draw (DrawingArea/Snapshot): spokes spinner, task-status ring, tab
  strip fade mask, fade scrims, unfocused-pane dimming overlay.
- Sidebar blur: composite to a solid color (2.5); do not chase compositor
  blur on Wayland.
- `theme.chrome` auto/light/dark should follow the TUI luminance rule (2.6).
- Workspace color palette (16 built-ins) and its dark-mode brightness boost:
  `Sources/WorkspaceTabColorSettings.swift:21-37, 256-270`
  (`boostedBrightness = min(1, max(b, 0.62) + (1-b)*0.28)`, skip when
  saturation <= 0.08).
