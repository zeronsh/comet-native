# Separate surface preference from theme selection

Zeron treats glass as an independent, device-local surface preference instead of a property that changes implicitly with theme selection. Each theme variant retains a recommended surface treatment (`Frosted` for Zeron and `Opaque` for VS Code-derived themes), while the user may preserve that recommendation or globally force `Frosted` or `Opaque`; forced frost derives its tints from the selected theme's own shell and background roles, and unsupported window compositors remain opaque.

This preserves theme authors' contrast assumptions by default while allowing users to keep a consistent glass preference across every built-in and custom theme. Syntax, terminal, semantic, status, diff, and accent roles are unaffected by surface preference.
