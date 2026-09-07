// Wrangler `rules` (type "Text", glob "**" + ".sh") imports shell scripts as
// strings — the installer served at /install.sh.
declare module "*.sh" {
  const text: string;
  export default text;
}

// Node-only test helpers (vitest unit tier reads the shared Rust fixture).
declare module "node:fs" {
  export function readFileSync(path: string | URL, encoding: "utf8"): string;
}
interface ImportMeta {
  url: string;
}
