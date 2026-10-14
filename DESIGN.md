# OpenDelay — Visual Design Brief

Companion to SPEC.md. Describes the intended look and feel in general terms.
All artwork, icons, wording and exact styling must be created originally.

## Toolkit

- Desktop window: **Slint** (Rust API, GPL-3.0-only licence option), winit
  backend, FemtoVG or Skia renderer. Linux (X11 + Wayland) and Windows x64.
- OBS dock and on-stream overlay (SPEC §7.3): small hand-written HTML/CSS/JS
  pages served by the local web server, styled with the same tokens below.
- Font: Inter (SIL OFL 1.1), bundled. Tabular digits for numbers.

## Mood

Dark, calm, premium "streaming tool" feel: near-black layered surfaces, a
single violet accent, soft glows instead of hard borders, generous spacing,
smooth short animations. Information is glanceable mid-stream.

## Colour tokens (use these values)

| Token | Value | Use |
|---|---|---|
| bg-base | `#060608` | window background |
| bg-elevated | `#0a0a0d` | sidebar |
| bg-surface | `#121217` | cards, inputs |
| bg-surface-hover | `#1a1a21` | hovered cards/controls |
| border-subtle | white @ 6% | card outlines |
| border-default | white @ 10% | inputs, dividers |
| border-hover | white @ 17% | hovered outlines |
| text-primary | `#f4f4f6` | headings, values |
| text-secondary | `#b3b3be` | body text |
| text-muted | `#7a7a88` | captions, hints |
| text-disabled | `#55555f` | disabled |
| accent | `#7f63f0` | primary buttons, active nav, focus |
| accent-hover | `#a48ffb` | hover/pressed accent |
| accent-muted | accent @ 12% | selected nav background, chips |
| accent-glow | accent @ 28% | glows/shadows around active elements |
| success | `#12b886` | "live", healthy |
| warning | `#f2a20c` | health warnings, buffering |
| error | `#ec4646` | errors, emergency stop |

Delay-active hero number: vertical gradient from pale lavender (`#ece6ff`)
through mid violet (`#a690fb`) to deep indigo (`#4a3394`), with a soft violet
glow behind it.

Radii: 4 / 6 / 10 / 14 / 20 px (xs → xl). Shadows: large, very soft, black
at ~30% with a faint top inner highlight on panels.

## Layout

- **Left sidebar** (~200 px, bg-elevated): app logo at top; vertical nav with
  icon + label: Dashboard, Multistream, Controllers, Overlay, Setup, Help,
  Settings. Active item: accent-muted background, accent icon, small accent
  bar. Divider before Help/Settings. Footer: version and an update badge.
- **Main area**: page title row (~56 px), then content in cards on a 12-col
  feel grid with 16–24 px gaps. Pages switch with a quick fade/slide.
- **Dashboard**:
  - Hero card centred at top: very large delay value (seconds) with unit,
    caption underneath, status pill (Offline / Live / Delayed / Buffering /
    Recovering) coloured by state. When delay is active the number uses the
    gradient + glow; when live it is plain text-primary.
  - Digit changes animate with a vertical rolling ("odometer") effect,
    ~0.6 s.
  - Primary action button (enable/disable delay) under the hero; secondary
    "go live" button beside it.
  - Delay duration input with − / + steppers (press-and-hold accelerates)
    and buffer preset selector.
  - Buffer fill progress bar (accent fill, warning colour while not ready).
  - Row of small stat cards: bitrate, uptime, buffer, current delay.
  - Health warnings card (warning colour accents), hidden when healthy.
- **Background**: optional, subtle slow-moving dark violet gradient/noise
  behind the content (cheap to render; must be disableable in settings).
- **Modals**: centred card, dimmed + blurred backdrop, scale-in.
- **Toasts**: bottom-centre, short-lived, success/warning/error tint.
- **Help button**: small floating round button bottom-right.

## Overlay (OBS browser source)

Transparent page. Compact rounded badge: icon + "delay on/off" state and the
delay value; during transitions a circular or linear progress ring. Badge
colour customisable from a palette of ~12 swatches (red, orange, yellow,
lime, green, teal, cyan, blue, indigo, violet, pink, white).

## Logo

Create an original, simple mark (do not imitate any existing product's
logo). Ship SVG plus PNG/ICO sizes 16–256 px.

## Accessibility

Text contrast ≥ 4.5:1 on its background, visible focus rings (accent), all
actions keyboard-reachable, animations respect a "reduce motion" setting.
