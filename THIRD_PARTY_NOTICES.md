# Third-party notices

Zeron bundles the following syntax-highlighting components. Unless noted otherwise, their parsers and queries are consumed from the pinned Rust crates listed in `Cargo.lock`. The Kotlin highlight query is maintained as Zeron source code and is not attributed to the grammar crate.

| Component | Version | License | Source |
| --- | --- | --- | --- |
| Tree-sitter | 0.26.11 | MIT | https://github.com/tree-sitter/tree-sitter |
| Tree-sitter highlight | 0.26.11 | MIT | https://github.com/tree-sitter/tree-sitter |
| Tree-sitter Rust grammar and queries | 0.24.2 | MIT | https://github.com/tree-sitter/tree-sitter-rust |
| Tree-sitter JavaScript grammar and queries | 0.25.0 | MIT | https://github.com/tree-sitter/tree-sitter-javascript |
| Tree-sitter TypeScript grammar and queries | 0.23.2 | MIT | https://github.com/tree-sitter/tree-sitter-typescript |
| Tree-sitter Python, Go, JSON, Bash, HTML, CSS, C, C++, C#, Java, Ruby and PHP grammars and queries | pinned in `Cargo.lock` | MIT | https://github.com/tree-sitter |
| Tree-sitter TOML, Markdown, YAML, Swift, SQL, Lua, Nix, Make and Containerfile grammars and queries | pinned in `Cargo.lock` | MIT-compatible; see each crate | Crate repositories recorded in `Cargo.lock` |
| Tree-sitter Kotlin grammar | 1.1.0 | MIT | https://github.com/tree-sitter-grammars/tree-sitter-kotlin |

The full Zeron distribution remains licensed under the terms in `LICENSE`.

## Bundled theme palette adaptations

Zeron includes manually curated palette adaptations derived from the projects
below. The source repository and exact audited revision are also embedded in
each resolved theme variant. These projects are not affiliated with or endorsed
by Zeron. Their names identify the corresponding palette adaptations.

| Theme project | Audited revision | License and upstream notice |
| --- | --- | --- |
| Visual Studio Code Dark+/Light+ | `e33d147d4c0fa65ce17cb73ec9d798f064b4bf1f` | [MIT](https://github.com/microsoft/vscode/blob/e33d147d4c0fa65ce17cb73ec9d798f064b4bf1f/LICENSE.txt) |
| Catppuccin for VS Code | `befc9e6fc41980f4241408f7049755d47c06ff45` | [MIT](https://github.com/catppuccin/vscode/blob/befc9e6fc41980f4241408f7049755d47c06ff45/LICENSE) |
| Tokyo Night VS Code Theme | `7c0f11eaef322f293621ca7befe462214b7ea468` | [MIT](https://github.com/tokyo-night/tokyo-night-vscode-theme/blob/7c0f11eaef322f293621ca7befe462214b7ea468/LICENSE.txt) |
| Dracula for Visual Studio Code | `1b9ecf4d7e0c8cc2e2e890a7a41ad1db5fff1e6c` | [MIT](https://github.com/dracula/visual-studio-code/blob/1b9ecf4d7e0c8cc2e2e890a7a41ad1db5fff1e6c/LICENSE) |
| GitHub VS Code Theme | `cd78e5e4e7bcf132a6f428ae0f32264bb1b729cf` | [MIT](https://github.com/primer/github-vscode-theme/blob/cd78e5e4e7bcf132a6f428ae0f32264bb1b729cf/LICENSE) |
| Ayu for VS Code | `444ef92911cb75c3933c8003e3a7c79b6b6c914f` | [MIT](https://github.com/ayu-theme/vscode-ayu/blob/444ef92911cb75c3933c8003e3a7c79b6b6c914f/LICENSE) |
| Gruvbox Theme | `ca3b8ad203e84a884ca33fb84b5795cf43032709` | [MIT](https://github.com/jdinhify/vscode-theme-gruvbox/blob/ca3b8ad203e84a884ca33fb84b5795cf43032709/LICENSE) |
| Rosé Pine for VS Code | `d8f5ebe8e096fa833e997c07eb7685ee1677a4ba` | [MIT](https://github.com/rose-pine/vscode/blob/d8f5ebe8e096fa833e997c07eb7685ee1677a4ba/LICENSE) |
| Nord Visual Studio Code | `8ead09822c02d0d49d0f764104505e5a34d3689f` | [MIT](https://github.com/nordtheme/visual-studio-code/blob/8ead09822c02d0d49d0f764104505e5a34d3689f/license) |
| One Dark Pro | `e6ccf638d5b69aa38cd1005edb0ee7ba7ef6fedc` | [MIT](https://github.com/Binaryify/OneDark-Pro/blob/e6ccf638d5b69aa38cd1005edb0ee7ba7ef6fedc/LICENSE.txt) |
| Atom One Dark Theme | `a8be970644982221f9b61fb1c4b3da74b4beab79` | [MIT](https://github.com/akamud/vscode-theme-onedark/blob/a8be970644982221f9b61fb1c4b3da74b4beab79/LICENSE) |
| Night Owl | `cc291eba7976b20d7c66bde6883c27b902196b07` | [MIT](https://github.com/sdras/night-owl-vscode-theme/blob/cc291eba7976b20d7c66bde6883c27b902196b07/LICENSE.md) |
| Winter is Coming | `260547834cb6ac37dd5b8bb5842cc1c8d3164946` | [MIT](https://github.com/johnpapa/vscode-winteriscoming/blob/260547834cb6ac37dd5b8bb5842cc1c8d3164946/LICENSE.md) |
| Palenight Theme | `6291efaace90855abe3d79025327ca41b9a3138c` | [MIT](https://github.com/whizkydee/vscode-palenight-theme/blob/6291efaace90855abe3d79025327ca41b9a3138c/license.md) |
| SynthWave '84 | `ecfa2fe1279f7233663fa3f98a96e6756000567b` | [MIT](https://github.com/robb0wen/synthwave-vscode/blob/ecfa2fe1279f7233663fa3f98a96e6756000567b/LICENSE) |
| Shades of Purple | `e8eb49f33e5db05ceba6677367b33ddb27ad821c` | [MIT text with an additional “With condition” section](https://github.com/ahmadawais/shades-of-purple-vscode/blob/e8eb49f33e5db05ceba6677367b33ddb27ad821c/LICENSE.md); Zeron is MIT-licensed, satisfying the stated condition |
| Cobalt2 | `c4e9574372b85afad1682ed0fdd1ac0411c62512` | [MIT](https://github.com/wesbos/cobalt2-vscode/blob/c4e9574372b85afad1682ed0fdd1ac0411c62512/LICENSE) |
| Andromeda | `d1abb48c69493000aa0133a32d594eb25e523d4f` | [MIT](https://github.com/EliverLara/Andromeda/blob/d1abb48c69493000aa0133a32d594eb25e523d4f/LICENSE.md) |

The palette values are adapted under the corresponding upstream license. The
linked license pages contain each project's copyright and permission notice and
are pinned to the same revision as the adapted source.

Copyright notices retained from those pinned upstream licenses:

- Copyright (c) 2015 - present Microsoft Corporation
- Copyright (c) 2021 Catppuccin
- Copyright (c) 2018-present Enkia
- Copyright (c) 2016 Dracula Theme
- Copyright (c) 2020 Primer
- Copyright (c) 2016 Ike Kurghinyan
- Copyright © 2017 JD
- Copyright (c) 2021 Rosé Pine
- Copyright (c) 2016-present Sven Greb <development@svengreb.de> (https://www.svengreb.de)
- Copyright (c) 2013-2022 Binaryify
- Copyright (c) 2015 Mahmoud Ali
- Copyright (c) 2018 Sarah Drasner
- Copyright (c) 2015-2017 JohnPapa.net, LLC
- Copyright (c) 2017-present Olaolu Olawuyi
- Copyright (c) 2019 Robb Owen
- Copyright (c) 2015-∞ Ahmad Awais
- Copyright (c) 2018 Wes Bos, Roberto Achar
- Copyright (c) 2017 <eliverlara@gmail.com>

The common MIT permission notice for the adaptations above follows:

> Permission is hereby granted, free of charge, to any person obtaining a copy
> of this software and associated documentation files (the “Software”), to deal
> in the Software without restriction, including without limitation the rights
> to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
> copies of the Software, and to permit persons to whom the Software is
> furnished to do so, subject to the following conditions:
>
> The above copyright notice and this permission notice shall be included in
> all copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED “AS IS”, WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
> IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
> FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
> AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
> LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
> OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
> SOFTWARE.

The pinned Shades of Purple license additionally says that anything built with
it should also be MIT licensed. Zeron is distributed under MIT terms.
