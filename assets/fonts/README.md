# Bundled fonts

Deltos ships its fonts so text looks the same on every system.

| Font | Files | License |
| ---- | ----- | ------- |
| [Adwaita Sans](https://gitlab.gnome.org/GNOME/adwaita-fonts) 51.0, the default interface font | `AdwaitaSans-Regular.ttf`, `AdwaitaSans-Medium.ttf`, `AdwaitaSans-SemiBold.ttf`, `AdwaitaSans-Bold.ttf` | SIL Open Font License 1.1, see [`AdwaitaSans-OFL.txt`](AdwaitaSans-OFL.txt) |
| [Go Mono](https://go.dev/blog/go-fonts), the default grid and code font | `GoMono-Regular.ttf`, `GoMono-Bold.ttf` | BSD 3-Clause, see [`GoMono-LICENSE.txt`](GoMono-LICENSE.txt) |
| [Inter](https://github.com/rsms/inter) 3.19 | `Inter-Regular.ttf`, `Inter-Medium.ttf`, `Inter-SemiBold.ttf`, `Inter-Bold.ttf` | SIL Open Font License 1.1, see [`Inter-OFL.txt`](Inter-OFL.txt) |
| [JetBrains Mono](https://github.com/JetBrains/JetBrainsMono) | `JetBrainsMono-Regular.ttf`, `JetBrainsMono-Bold.ttf` | Apache License 2.0, Copyright 2020 JetBrains s.r.o. |

Adwaita Sans is published as a variable font. The four files here are static instances
of it at the 400, 500, 600 and 700 weights (optical size 14), made with fontTools'
`varLib.instancer`, so every platform's text stack picks the same weight. The family
name is unchanged; the font has no Reserved Font Name.
