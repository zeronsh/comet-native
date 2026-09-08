# Domain context

## Theme vocabulary

- **Theme family** — A named collection of related theme variants that share an origin, such as Night Owl and Night Owl Light.
- **Theme variant** — One complete, resolved palette for a single appearance (`light` or `dark`). Runtime UI consumes variants, not source-format tokens.
- **Theme source** — The durable origin of a custom family: an imported snapshot, linked file, linked package, or editable native file.
- **Imported snapshot** — A self-contained copy of a compiled theme family. It no longer follows changes to its original source.
- **Linked theme** — A custom family that follows a source on disk and can be reloaded without re-importing it.
- **Linked file** — A link to one VS Code-compatible theme definition.
- **Linked package** — A link to a VS Code extension folder or `package.json` that declares one or more related theme variants.
- **Editable theme** — A duplicate stored as a native resolved-family JSON file. Users edit the file directly and explicitly reload it; invalid edits preserve the last known good family.
- **Last known good** — The most recent successfully compiled family retained by a linked theme when its current source is missing or invalid.
- **Import report** — A per-variant summary of mapped roles, fallbacks, unsupported values, and inferred decisions produced during compilation.
- **Mapping review** — The optional advanced view of an import report. Normal theme selection and import do not expose token names.
- **Theme hardening** — Deterministic post-mapping repairs that prefer stronger semantically related source colors, minimally adjust only shared Zeron roles when needed, and record every decision in the mapping review.
- **Theme default accent** — The interaction accent chosen or inferred for a theme variant by its authoring source.
- **Accent override** — A Zeron preset that replaces interaction roles only; syntax, terminal ANSI, diff, warning, error, and success colors remain owned by the theme.
- **Recommended surface treatment** — A theme variant's authored recommendation for whether its surfaces are frosted or opaque. It is used only when the user keeps the surface preference at theme default.
- **Surface preference** — A device-local choice of theme default, frosted, or opaque that is independent of appearance, theme, and accent selections.
- **Resolved surface treatment** — The effective frosted or opaque treatment produced by applying the surface preference to the active variant's recommendation.
