# colored 3.x Reference

> Urd dependency: `colored = "3"` (resolves to 3.1.1)
> Previous: `colored = "2"`

## Migration from 2.x to 3.x

**The only breaking change is MSRV raised to 1.80.** The API is identical.
`lazy_static` was replaced with `std::sync::OnceLock` internally.

No code changes needed. Drop-in replacement.

## New in 3.1 (non-breaking)

Hex color support:
```rust
"text".color("#efabea");      // 6-digit hex
"text".on_color("#fd0");      // 3-digit hex
"text".ansi_color(42u8);      // arbitrary ANSI code
```

## Current API

```rust
use colored::Colorize;

// Method chaining
"bold red".red().bold();
"green italic".green().italic();
"on white".blue().on_white();

// Available colors: black, red, green, yellow, blue, magenta (alias: purple),
// cyan, white — plus bright_* variants for all

// Styles: bold(), dimmed(), italic(), underline(), blink(),
// reversed(), hidden(), strikethrough(), normal(), clear()

// Dynamic colors
"text".color("blue");                   // from string
"text".truecolor(0, 255, 136);          // RGB
"text".on_truecolor(135, 28, 167);      // RGB background
"text".custom_color((0, 255, 136));     // via Into<CustomColor>
```

## Color Control

Environment variables (respected automatically):
- `NO_COLOR` — disables color output
- `CLICOLOR` — standard TTY color discovery
- `CLICOLOR_FORCE` — force color even when not a TTY
- Priority: `CLICOLOR_FORCE` > `NO_COLOR` > `CLICOLOR`
- Compile-time disable: `no-color` cargo feature

## Deprecations

- `ColoredString::fgcolor()` getter — use public `fgcolor` field directly
- `ColoredString::bgcolor()` getter — use public `bgcolor` field directly
- `ColoredString::style()` getter — use public `style` field directly
- `Colorize::reverse()` — use `reversed()` instead
